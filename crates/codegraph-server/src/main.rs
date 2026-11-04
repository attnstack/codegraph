//! CLI + MCP 双出口的入口。main 只做：日志初始化 → 分发 → 退出码映射。
//! 退出码约定：0 成功；1 运行错误；2 目标未找到/歧义（可脚本化判断）。

use clap::{ArgAction, Parser, Subcommand};
use codegraph_server::{commands, mcp, output};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "codegraph",
    version,
    about = "确定、可解释、可溯源的代码知识图谱——每条结果附证据调用链",
    long_about = None
)]
struct Cli {
    /// 输出 JSON（稳定 schema，供脚本/MCP 使用）
    #[arg(long, global = true)]
    json: bool,
    /// 日志详细度（-v info，-vv debug；日志一律走 stderr，不污染 MCP stdout）
    #[arg(long, short = 'v', global = true, action = ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 监听文件变更自动增量重建（开发态）
    Watch {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 构建索引（增量：只重解析变化的文件）
    Index {
        #[arg(long, help = "仓库根目录，默认当前目录")]
        root: Option<PathBuf>,
        #[arg(long, help = "强制全量重解析")]
        force: bool,
        #[arg(long, help = "启用 tsc 类型消歧（需仓库有 node_modules/typescript）")]
        types: bool,
    },
    /// 增量更新（与 index 等价，语义别名）
    Update {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        types: bool,
    },
    /// 谁调用了这个符号
    WhoCalls {
        symbol: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 影响面分析：反向调用闭包 + 风险排序 + 证据链
    Impact {
        symbol: String,
        #[arg(long)]
        depth: Option<usize>,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long, help = "展开动态调用点明细")]
        show_dynamic: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 链路追踪：entry 到 sink 的最短调用链（双向 BFS）
    Trace {
        entry: String,
        sink: String,
        #[arg(long)]
        depth: Option<usize>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 这个符号调用了谁（who-calls 的镜像）
    Callees {
        symbol: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 孤儿函数：无静态调用者且非对外 API
    DeadCode {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// import 循环依赖
    Cycles {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 加权 PageRank 核心函数
    Hotspots {
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 符号搜索（精确/前缀）
    Search {
        query: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 代码考古：引入 commit / 作者贡献度
    Archaeology {
        /// 符号名，或 path:start-end / path:line
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 导出 DOT 供 Graphviz 可视化
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 图谱统计
    Stats {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 启动 MCP stdio 服务（对接 Claude Code / Cursor 等 Agent）
    Mcp {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 打印文件 AST（语言适配器调试用）
    DebugAst { file: PathBuf },
}

fn main() {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    let code = match run(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    use Cmd::*;
    let json = cli.json;
    match cli.cmd {
        Watch { root } => {
            let cfg = commands::load_cfg(root)?;
            codegraph_server::watch::run_watch(&cfg)?;
            Ok(0)
        }
        Index { root, force, types } => {
            let mut cfg = commands::load_cfg(root)?;
            if types {
                cfg.use_types = true;
            }
            let stats = commands::run_build(&cfg, force)?;
            output::print_build_stats(&stats, json)?;
            Ok(0)
        }
        Update { root, types } => {
            let mut cfg = commands::load_cfg(root)?;
            if types {
                cfg.use_types = true;
            }
            let stats = commands::run_build(&cfg, false)?;
            output::print_build_stats(&stats, json)?;
            Ok(0)
        }
        WhoCalls { symbol, root } => {
            let cfg = commands::load_cfg(root)?;
            let (db, snap) = commands::open_for_query(&cfg)?;
            let start = commands::resolve_unique(&db, &symbol)?;
            let report = codegraph_core::query::who_calls(&snap, start.id);
            output::print_who_calls(&report, json)?;
            Ok(0)
        }
        Impact {
            symbol,
            depth,
            max_nodes,
            show_dynamic,
            root,
        } => {
            let cfg = commands::load_cfg(root)?;
            let (db, snap) = commands::open_for_query(&cfg)?;
            let start = commands::resolve_unique(&db, &symbol)?;
            let report = codegraph_core::query::impact(&snap, start.id, &cfg, depth, max_nodes);
            output::print_impact(&report, json, show_dynamic)?;
            Ok(0)
        }
        Trace {
            entry,
            sink,
            depth,
            root,
        } => {
            let cfg = commands::load_cfg(root)?;
            let (db, snap) = commands::open_for_query(&cfg)?;
            let e = commands::resolve_unique(&db, &entry)?;
            let s = commands::resolve_unique(&db, &sink)?;
            let report = codegraph_core::query::trace(&snap, e.id, s.id, &cfg, depth);
            output::print_trace(&report, json)?;
            Ok(0)
        }
        Callees { symbol, root } => {
            let cfg = commands::load_cfg(root)?;
            let (db, snap) = commands::open_for_query(&cfg)?;
            let start = commands::resolve_unique(&db, &symbol)?;
            let report = codegraph_core::query::callees(&snap, start.id);
            output::print_callees(&report, json)?;
            Ok(0)
        }
        DeadCode { root } => {
            let cfg = commands::load_cfg(root)?;
            let (_, snap) = commands::open_for_query(&cfg)?;
            let report = codegraph_core::query::dead_code(&snap, &cfg);
            output::print_dead_code(&report, json)?;
            Ok(0)
        }
        Cycles { root } => {
            let cfg = commands::load_cfg(root)?;
            let (_, snap) = commands::open_for_query(&cfg)?;
            let report = codegraph_core::query::cycles(&snap);
            output::print_cycles(&report, json)?;
            Ok(0)
        }
        Hotspots { top, root } => {
            let cfg = commands::load_cfg(root)?;
            let (_, snap) = commands::open_for_query(&cfg)?;
            let items = codegraph_core::query::hotspots(&snap, &cfg, top);
            output::print_hotspots(&items, json)?;
            Ok(0)
        }
        Search { query, limit, root } => {
            let cfg = commands::load_cfg(root)?;
            let (db, _) = commands::open_for_query(&cfg)?;
            let hits = db.search_nodes(&query, limit.unwrap_or(cfg.search_limit))?;
            output::print_search(&hits, json)?;
            Ok(0)
        }
        Archaeology { target, root } => {
            let cfg = commands::load_cfg(root)?;
            let report = commands::archaeology(&cfg, &target)?;
            output::print_archaeology(&report, json)?;
            Ok(0)
        }
        Export { out, root } => {
            let cfg = commands::load_cfg(root)?;
            let (db, _) = commands::open_for_query(&cfg)?;
            let dot = db.export_dot()?;
            match out {
                Some(p) => {
                    std::fs::write(&p, dot)?;
                    eprintln!("DOT 已写入 {}", p.display());
                }
                None => print!("{dot}"),
            }
            Ok(0)
        }
        Stats { root } => {
            let cfg = commands::load_cfg(root)?;
            let (db, _) = commands::open_for_query(&cfg)?;
            let stats = db.stats()?;
            output::print_stats(&stats, json)?;
            Ok(0)
        }
        Mcp { root } => {
            let cfg = commands::load_cfg(root)?;
            mcp::serve(cfg)?;
            Ok(0)
        }
        DebugAst { file } => {
            let bytes = std::fs::read(&file)?;
            let rel = file.to_string_lossy().replace('\\', "/");
            match codegraph_core::parse::debug_tree(&rel, &bytes) {
                Some(tree) => {
                    print!("{tree}");
                    Ok(0)
                }
                None => {
                    eprintln!("不支持的文件类型: {rel}");
                    Ok(1)
                }
            }
        }
    }
}
