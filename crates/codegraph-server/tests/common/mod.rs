//! 测试公共设施：临时目录与夹具仓库复制（标准库实现，零额外依赖）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 系统临时目录下的唯一工作区；测试结束由调用方清理（Drop 守卫见 TempRoot）
pub fn temp_root(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dir =
        std::env::temp_dir().join(format!("codegraph-{tag}-{}-{}-{n}", std::process::id(), t));
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}

/// 递归复制目录（跳过 .codegraph 与 .git）
pub fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("创建目标目录失败");
    for entry in fs::read_dir(src).expect("读取源目录失败") {
        let entry = entry.expect("读取目录项失败");
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if name_str == ".codegraph" || name_str == ".git" || name_str == "target" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).expect("复制文件失败");
        }
    }
}

pub fn copy_fixture(dest: &Path) {
    let manifest = env!("CARGO_MANIFEST_DIR");
    // CARGO_MANIFEST_DIR = crates/codegraph-server，上两级 = 仓库根
    let fixture = Path::new(manifest).join("../../fixtures/demo-app");
    copy_dir(&fixture, dest);
}

/// 测试结束清理（best-effort，Windows 上文件句柄释放晚时不致命）
pub fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}
