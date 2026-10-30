//! git 集成（考古）：`git blame --line-porcelain` 包装与解析。
//! 选择子进程而非 libgit2（why）：libgit2 的 blame 对 -C（跨文件移动追踪）
//! 支持不全，而 -C 正是考古可解释性的关键；子进程方案要求环境有 git，
//! 失败时报错并给出明确提示（可恢复错误）。

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub author_mail: String,
    pub author_time: i64,
    pub summary: String,
    pub boundary: bool,
}

/// 返回 [start, end] 行区间内每行的 blame 归属（每行一条，保持行序）。
pub fn blame_lines(root: &Path, rel: &str, start: u32, end: u32) -> Result<Vec<BlameLine>> {
    if end < start {
        return Err(anyhow!("行范围无效: {start}-{end}"));
    }
    let out = Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "blame",
            "-C",
            "-C",
            "--line-porcelain",
            &format!("-L{},{}", start, end),
            "--",
            rel,
        ])
        .output()
        .context("无法启动 git（请确认已安装并加入 PATH）")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("git blame 失败: {}", stderr.trim()));
    }
    parse_porcelain(&String::from_utf8_lossy(&out.stdout))
}

fn parse_porcelain(text: &str) -> Result<Vec<BlameLine>> {
    let mut lines = Vec::new();
    let mut cur: Option<BlameLine> = None;
    for raw in text.lines() {
        if let Some(content) = raw.strip_prefix('\t') {
            // 内容行 = 一条 blame 记录的结尾
            let _ = content;
            if let Some(c) = cur.take() {
                lines.push(c);
            }
            continue;
        }
        if raw.len() >= 40 && raw.as_bytes().get(40) == Some(&b' ') && !raw.starts_with("previous ")
        {
            // 记录头：<sha40> <orig> <final> [<num>]，前缀 ^ 表示 boundary
            let (boundary, sha) = if let Some(stripped) = raw.strip_prefix('^') {
                (true, &stripped[..40.min(stripped.len())])
            } else {
                (false, &raw[..40])
            };
            if let Some(c) = cur.take() {
                lines.push(c);
            }
            cur = Some(BlameLine {
                commit: sha.to_string(),
                author: String::new(),
                author_mail: String::new(),
                author_time: 0,
                summary: String::new(),
                boundary,
            });
            continue;
        }
        if let Some(c) = cur.as_mut() {
            if let Some(v) = raw.strip_prefix("author ") {
                c.author = v.to_string();
            } else if let Some(v) = raw.strip_prefix("author-mail ") {
                c.author_mail = v.trim_matches(|ch| ch == '<' || ch == '>').to_string();
            } else if let Some(v) = raw.strip_prefix("author-time ") {
                c.author_time = v.parse().unwrap_or(0);
            } else if let Some(v) = raw.strip_prefix("summary ") {
                c.summary = v.to_string();
            }
            // boundary / previous / filename 等头忽略
        }
    }
    if let Some(c) = cur.take() {
        lines.push(c);
    }
    if lines.is_empty() {
        return Err(anyhow!("git blame 输出为空（文件不在 git 历史中？）"));
    }
    Ok(lines)
}
