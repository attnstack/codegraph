//! 增量一致性 fuzz（本项目的核心正确性测试）：
//! 随机变异文件序列后，「增量 update 的查询结果 ≡ 全量重建的查询结果」必须成立。
//! 为什么能成立：重解析只做变更文件，但符号解析永远对全量视图重跑——
//! 增量与全量是同一个确定性函数的两次求值。本测试就是这条等式的执行证明。

mod common;

use codegraph_core::config::Config;
use codegraph_core::query;
use codegraph_server::commands;
use codegraph_store::GraphDb;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// 无依赖 xorshift RNG（固定种子 → 结果可复现）
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn ts_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if p.file_name().map(|n| n == ".codegraph").unwrap_or(false) {
                    continue;
                }
                walk(&p, out);
            } else if matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ts") | Some("tsx")
            ) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[derive(Debug)]
enum Mutation {
    AppendFunction {
        file: usize,
        n: u64,
        call: &'static str,
    },
    PrependComment {
        file: usize,
    },
    NewFile {
        n: u64,
    },
    DeleteFile {
        file: usize,
    },
    AppendOverloads {
        file: usize,
        n: u64,
    },
}

fn apply(root: &Path, m: &Mutation) {
    match m {
        Mutation::AppendFunction { file, n, call } => {
            let files = ts_files(root);
            let p = &files[*file];
            let mut content = fs::read_to_string(p).unwrap();
            content.push_str(&format!(
                "\nexport function gen_{n}(): string {{\n  return {call}(1);\n}}\n"
            ));
            fs::write(p, content).unwrap();
        }
        Mutation::PrependComment { file } => {
            let files = ts_files(root);
            let p = &files[*file];
            let content = fs::read_to_string(p).unwrap();
            fs::write(p, format!("// touched {}\n{content}", std::process::id())).unwrap();
        }
        Mutation::NewFile { n } => {
            let p = root.join("src").join(format!("gen_{n}.ts"));
            fs::write(
                p,
                format!(
                    "import {{ charge }} from './services/payment';\n\nexport function gen_{n}(x: number): string {{\n  return charge(x);\n}}\n"
                ),
            )
            .unwrap();
        }
        Mutation::DeleteFile { file } => {
            let files = ts_files(root);
            let p = &files[*file];
            // 只删 gen_ 开头或独立文件，避免破坏夹具里被依赖的结构使语义不可比
            let name = p.file_stem().unwrap().to_string_lossy().to_string();
            if !name.starts_with("gen_") && name != "broker" {
                return;
            }
            fs::remove_file(p).unwrap();
        }
        Mutation::AppendOverloads { file, n } => {
            let files = ts_files(root);
            let p = &files[*file];
            let mut content = fs::read_to_string(p).unwrap();
            // 同名重载：触发节点去重逻辑
            content.push_str(&format!(
                "\nexport function gen_{n}(x: number): number;\nexport function gen_{n}(x: string): string;\nexport function gen_{n}(x: unknown): unknown {{ return x; }}\n"
            ));
            fs::write(p, content).unwrap();
        }
    }
}

/// 规范化查询输出：剥掉 id（增量与全量的节点 id 分配不同，属实现细节）
fn canonical(v: &Value) -> String {
    fn strip(x: &Value) -> Value {
        match x {
            Value::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, val) in o {
                    if k == "id" {
                        continue;
                    }
                    m.insert(k.clone(), strip(val));
                }
                Value::Object(m)
            }
            Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&strip(v)).unwrap()
}

#[test]
fn incremental_matches_full_rebuild() {
    let seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);
    let iterations: usize = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let tmp = common::temp_root("fuzz");
    common::copy_fixture(&tmp);
    let cfg = Config::load(&tmp);
    commands::run_build(&cfg, true).expect("初始构建失败");

    let mut rng = Rng(seed ^ 0x9E3779B97F4A7C15);

    for iter in 0..iterations {
        // 1) 随机变异（计数器兼作生成符号的唯一名）
        let counter = 1000u64 + iter as u64 + 1;
        let pick = rng.below(5);
        let files = ts_files(&tmp);
        let mutation = match pick {
            0 => Mutation::AppendFunction {
                file: rng.below(files.len()),
                n: counter,
                call: ["charge", "format", "log"][rng.below(3)],
            },
            1 => Mutation::PrependComment {
                file: rng.below(files.len()),
            },
            2 => Mutation::NewFile { n: counter },
            3 => Mutation::DeleteFile {
                file: rng.below(files.len()),
            },
            _ => Mutation::AppendOverloads {
                file: rng.below(files.len()),
                n: counter,
            },
        };
        apply(&tmp, &mutation);

        // 2) 增量更新
        commands::run_build(&cfg, false)
            .unwrap_or_else(|e| panic!("iter {iter}: 增量更新失败 ({mutation:?}): {e}"));

        // 3) 同一树的全量重建（独立目录 + 全新 DB）
        let full = common::temp_root("fuzz-full");
        common::copy_dir(&tmp, &full);
        let cfg_full = Config::load(&full);
        commands::run_build(&cfg_full, true).unwrap();

        // 4) 查询电池对比：两个库里全部符号的 who_calls + impact
        let db_inc = GraphDb::open(&cfg.db_path()).unwrap();
        let db_full = GraphDb::open(&cfg_full.db_path()).unwrap();
        let snap_inc = db_inc.load_snapshot(cfg.cross_file_bonus).unwrap();
        let snap_full = db_full.load_snapshot(cfg_full.cross_file_bonus).unwrap();

        let mut names: Vec<String> = db_inc
            .all_symbol_nodes()
            .unwrap()
            .into_iter()
            .map(|(_, q)| q)
            .collect();
        let mut names_full: Vec<String> = db_full
            .all_symbol_nodes()
            .unwrap()
            .into_iter()
            .map(|(_, q)| q)
            .collect();
        // id 序不同（增量重插节点），按集合语义比较
        names.sort();
        names_full.sort();
        assert_eq!(
            names, names_full,
            "iter {iter}: 增量与全量的符号集合不一致（mutation={mutation:?}）"
        );

        for name in &names {
            // 同名符号（如两个 handle.ts）必须确定性选取，否则两边可能选到不同节点
            let pick = |snap: &codegraph_core::model::Snapshot| -> i64 {
                let mut cands: Vec<&codegraph_core::model::NodeRef> = snap
                    .nodes
                    .values()
                    .filter(|n| &n.qualified_name == name)
                    .collect();
                cands.sort_by(|a, b| {
                    (a.file.clone(), a.kind.clone()).cmp(&(b.file.clone(), b.kind.clone()))
                });
                cands[0].id
            };
            let id_inc = pick(&snap_inc);
            let id_full = pick(&snap_full);
            let wc_inc =
                canonical(&serde_json::to_value(query::who_calls(&snap_inc, id_inc)).unwrap());
            let wc_full =
                canonical(&serde_json::to_value(query::who_calls(&snap_full, id_full)).unwrap());
            assert_eq!(
                wc_inc, wc_full,
                "iter {iter}: who_calls({name}) 不一致（mutation={mutation:?}）"
            );
            let im_inc = canonical(
                &serde_json::to_value(query::impact(&snap_inc, id_inc, &cfg, None, None)).unwrap(),
            );
            let im_full = canonical(
                &serde_json::to_value(query::impact(&snap_full, id_full, &cfg_full, None, None))
                    .unwrap(),
            );
            assert_eq!(
                im_inc, im_full,
                "iter {iter}: impact({name}) 不一致（mutation={mutation:?}）"
            );
        }

        common::cleanup(&full);
    }

    common::cleanup(&tmp);
}
