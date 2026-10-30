//! 文件发现：基于 ripgrep 的 ignore crate（.gitignore 感知）+ 目录黑名单 + 扩展名过滤。
//! 输出统一为「相对 root 的 / 分隔路径」，Windows 反斜杠在此处归一，
//! 后续所有模块（解析/解析/存储）都只见规范路径。

use crate::config::Config;
use crate::error::CoreResult;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    /// 相对路径，始终 `/` 分隔
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
    /// mtime 纳秒（溢出饱和即可，仅用于脏检测预筛，不参与内容判定）
    pub mtime_ns: u64,
}

pub fn discover(root: &Path, cfg: &Config) -> CoreResult<Vec<DiscoveredFile>> {
    let exts: HashSet<String> = cfg.include_extensions.iter().cloned().collect();
    let excludes: HashSet<String> = cfg.exclude_dirs.iter().cloned().collect();

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .require_git(false) // 非 git 目录也尊重 .gitignore，行为与宿主 VCS 解耦
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .parents(true);

    let mut files = Vec::new();
    // filter_entry 对目录名做黑名单剪枝，避免深入 node_modules 后再丢弃
    let root_clone = root.to_path_buf();
    let walker = builder
        .filter_entry(move |entry| {
            let p = entry.path();
            if p == root_clone.as_path() {
                return true;
            }
            if let Some(name) = p.file_name() {
                let n = name.to_string_lossy();
                if excludes.contains(n.as_ref()) {
                    return false;
                }
            }
            true
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let ext_ok = path
            .extension()
            .map(|e| exts.contains(e.to_string_lossy().as_ref()))
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        let rel_os = match path.strip_prefix(root) {
            Ok(r) => r,
            // walker 可能产出 root 之外的路径（如全局 ignore），直接跳过
            Err(_) => continue,
        };
        let rel = rel_os.to_string_lossy().replace('\\', "/");
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            // 文件可能在遍历间隙被删除（编辑器原子写），跳过并记录
            Err(e) => {
                tracing::warn!("跳过无法读取的文件 {}: {}", rel, e);
                continue;
            }
        };
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        files.push(DiscoveredFile {
            rel,
            abs: path.to_path_buf(),
            size: meta.len(),
            mtime_ns,
        });
    }

    // 排序保证遍历顺序确定 → 节点临时 id 分配确定 → 快照测试可复现
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(files)
}
