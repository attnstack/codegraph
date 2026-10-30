//! 配置定义与三层合并：内置默认值 ← `<root>/.codegraph.toml` ← CLI 参数覆盖。
//! 所有可调参数（权重、深度、排除规则）都外置在这里，代码中不出现魔法数字。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// 仓库根目录
    pub root: PathBuf,
    /// 图谱数据库路径；None = `<root>/.codegraph/index.db`
    pub db: Option<PathBuf>,
    /// 纳入索引的源码扩展名
    pub include_extensions: Vec<String>,
    /// 遍历时剪掉的目录名（精确匹配任一路径分段）
    pub exclude_dirs: Vec<String>,
    /// 影响面闭包剪枝：相对路径中任一分段命中即整支剪掉。
    /// 注意用「分段精确匹配」而非子串匹配——子串会把 latest.ts 误判成测试文件。
    pub impact_exclude_components: Vec<String>,
    /// 影响面闭包剪枝：文件名主干含这些标记即剪掉（checkout.test.ts / order.spec.ts）
    pub impact_exclude_stem_markers: Vec<String>,
    /// 反向闭包最大深度
    pub impact_depth: usize,
    /// 反向闭包最大访问节点数，超过即截断并输出 truncated 标记
    pub impact_max_nodes: usize,
    /// 风险衰减：risk(child) = risk(parent) * decay * type_weight
    pub risk_decay: f64,
    /// 边类型权重（直接调用 > 继承重写 > 类继承/实现）
    pub weight_call: f64,
    pub weight_override: f64,
    pub weight_extends: f64,
    pub weight_implements: f64,
    /// PageRank 阻尼
    pub pagerank_damping: f64,
    /// L1 收敛阈值
    pub pagerank_tolerance: f64,
    pub pagerank_max_iter: usize,
    /// 跨文件调用边权加成（跨模块耦合比文件内更值得警惕）
    pub cross_file_bonus: f64,
    /// trace 双向 BFS 单侧限深
    pub trace_depth: usize,
    /// search 默认上限
    pub search_limit: usize,
    /// 查询前自动检测文件变化并增量重建（mtime+size 预筛 → hash 确认）
    pub auto_refresh: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("."),
            db: None,
            include_extensions: vec![
                "ts".into(),
                "tsx".into(),
                "js".into(),
                "jsx".into(),
                "mjs".into(),
                "cjs".into(),
            ],
            exclude_dirs: vec![
                "node_modules".into(),
                "dist".into(),
                "build".into(),
                "out".into(),
                ".git".into(),
                ".codegraph".into(),
                "vendor".into(),
                "coverage".into(),
                ".next".into(),
                ".nuxt".into(),
            ],
            impact_exclude_components: vec![
                "tests".into(),
                "test".into(),
                "__tests__".into(),
                "__mocks__".into(),
                "spec".into(),
                "specs".into(),
                "e2e".into(),
                "generated".into(),
                "__generated__".into(),
                "vendor".into(),
                "dist".into(),
                "node_modules".into(),
                "coverage".into(),
                "stories".into(),
            ],
            impact_exclude_stem_markers: vec![".test".into(), ".spec".into()],
            impact_depth: 8,
            impact_max_nodes: 500,
            risk_decay: 0.8,
            weight_call: 1.0,
            weight_override: 0.7,
            weight_extends: 0.6,
            weight_implements: 0.6,
            pagerank_damping: 0.85,
            pagerank_tolerance: 1e-6,
            pagerank_max_iter: 100,
            cross_file_bonus: 1.5,
            trace_depth: 5,
            search_limit: 50,
            auto_refresh: true,
        }
    }
}

impl Config {
    /// 读取 `<root>/.codegraph.toml`。文件缺失用默认值；文件损坏降级默认值并告警——
    /// 配置错误不应该让索引彻底不可用（可恢复 vs 不可恢复错误的分界）。
    pub fn load(root: &Path) -> Config {
        let mut cfg = Config {
            root: root.to_path_buf(),
            ..Config::default()
        };
        let file = root.join(".codegraph.toml");
        if let Ok(text) = std::fs::read_to_string(&file) {
            match toml::from_str::<Config>(&text) {
                Ok(mut over) => {
                    over.root = cfg.root; // root 以 CLI 为准，不允许被配置文件改写
                    cfg = over;
                }
                Err(e) => {
                    tracing::warn!("配置文件 {} 解析失败，使用默认配置: {}", file.display(), e)
                }
            }
        }
        cfg
    }

    pub fn db_path(&self) -> PathBuf {
        self.db
            .clone()
            .unwrap_or_else(|| self.root.join(".codegraph").join("index.db"))
    }

    /// 影响面闭包剪枝判定。相对路径使用 `/` 分隔（walk 层已统一）。
    pub fn is_pruned(&self, rel_path: &str) -> bool {
        let p = Path::new(rel_path);
        for comp in p.components() {
            let c = comp.as_os_str().to_string_lossy();
            if self.impact_exclude_components.iter().any(|x| x == &c) {
                return true;
            }
        }
        if let Some(stem) = p.file_stem() {
            let s = stem.to_string_lossy();
            if self
                .impact_exclude_stem_markers
                .iter()
                .any(|m| s.contains(m.as_str()))
            {
                return true;
            }
        }
        false
    }
}
