//! 文件 I/O 工具
//!
//! 原子写入 (tmp + rename)，避免进程中段被 kill 导致目标文件半写损坏。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 原子地把字符串写入文件
///
/// 1. 写入同目录临时文件 `<target>.tmp.<pid>.<nanos>`
/// 2. flush + sync_all 确保数据落盘
/// 3. rename 原子替换（POSIX 保证原子；Windows 上需先删旧）
pub fn atomic_write_string<P: AsRef<Path>>(path: P, content: &str) -> std::io::Result<()> {
    write_internal(path.as_ref(), content.as_bytes(), None)
}

/// 同 `atomic_write_string`，但在 rename 前对临时文件 chmod 0o600（Unix）。
/// 防同主机其他用户读取凭据 / 配置等敏感文件。
pub fn atomic_write_string_secure<P: AsRef<Path>>(
    path: P,
    content: &str,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write_internal(
            path.as_ref(),
            content.as_bytes(),
            Some(std::fs::Permissions::from_mode(0o600)),
        )
    }
    #[cfg(not(unix))]
    {
        write_internal(path.as_ref(), content.as_bytes(), None)
    }
}

fn write_internal(
    path: &Path,
    bytes: &[u8],
    perms: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    let tmp_path = make_tmp_path(path);

    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
    }

    if let Some(p) = perms {
        if let Err(e) = std::fs::set_permissions(&tmp_path, p) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    }

    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path).ok();
    }

    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

fn make_tmp_path(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let mut tmp = target.to_path_buf();
    tmp.set_file_name(format!("{}.tmp.{}.{}", name, pid, nanos));
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_target(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        dir.join(format!("xkiro-iotest-{}-{}-{}.json", pid, nanos, suffix))
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let target = tmp_target("a");
        atomic_write_string(&target, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        atomic_write_string(&target, "world").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "world");
        let _ = std::fs::remove_file(&target);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_secure_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let target = tmp_target("sec");
        atomic_write_string_secure(&target, "secret").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got mode {:o}", mode & 0o777);
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn atomic_write_no_tmp_residue_on_success() {
        let target = tmp_target("clean");
        atomic_write_string(&target, "x").unwrap();
        let dir = target.parent().unwrap();
        let stem = target.file_name().unwrap().to_str().unwrap();
        let leaked: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(&format!("{}.tmp.", stem)))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leaked.is_empty(), "leaked tmp files: {:?}", leaked);
        let _ = std::fs::remove_file(&target);
    }
}
