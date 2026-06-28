//! 隔离式浏览器启动：每次登录用一个全新的临时 profile 打开 URL，确保零 cookie 会话。
//!
//! 协议怪癖 + 根因（已用 curl 实测验证）：Kiro 登录走中间层
//! `auth.desktop.kiro.dev/login` → AWS Cognito `oauth2/authorize` → `github/authorize`
//! → 真正的 github.com/google。中间层在【第一跳】就剥离了我们传入的所有额外 query 参数，
//! 所以 `prompt=select_account` 永远到不了 GitHub/Google，无法靠 URL 参数强制选号。
//! 账号复用真正发生在 github.com / google.com 的【浏览器 cookie】层。
//!
//! `--incognito` 单独不够：第二次 `--incognito` 会复用一个已存在的无痕窗口，里面仍带着
//! 上个账号的 cookie。唯一可靠解是每次登录给浏览器一个【全新的空 profile】——
//! Chromium 系用 `--user-data-dir=<临时目录>`，Firefox 用 `-profile <临时目录> -no-remote`，
//! 用后即弃。

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// 启动句柄：drop 时尽力删除临时 profile 目录（用后即弃，避免磁盘泄漏）。
pub struct IsolatedBrowser {
    profile_dir: PathBuf,
}

impl Drop for IsolatedBrowser {
    fn drop(&mut self) {
        // 浏览器可能仍开着；Linux/macOS 下 unlink 不影响已打开的 fd，Windows 锁定则忽略错误
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

/// 用全新的临时 profile 打开 URL。
///
/// 成功返回 `Some(guard)`（其 drop 负责清理 profile，调用方需持有到登录结束）；
/// 找不到任何可用浏览器时返回 `None`，调用方应提示用户【手动用隐私窗口打开】——
/// 注意不要回退到普通打开，否则会复用旧账号 cookie，正是要解决的问题。
pub fn open_isolated(url: &str) -> Option<IsolatedBrowser> {
    let profile_dir = std::env::temp_dir().join(format!("xkiro-oauth-{}", uuid::Uuid::new_v4()));
    if std::fs::create_dir_all(&profile_dir).is_err() {
        return None;
    }
    let dir = profile_dir.to_string_lossy().to_string();

    for (bin, args) in launch_attempts(&dir, url) {
        if Command::new(bin)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return Some(IsolatedBrowser { profile_dir });
        }
    }

    let _ = std::fs::remove_dir_all(&profile_dir);
    None
}

/// 按平台构造"隔离 profile 打开 URL"的候选命令列表，逐个尝试直到 spawn 成功。
fn launch_attempts(dir: &str, url: &str) -> Vec<(String, Vec<String>)> {
    let chromium_args = |dir: &str, url: &str| -> Vec<String> {
        vec![
            format!("--user-data-dir={}", dir),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            url.to_string(),
        ]
    };

    #[cfg(target_os = "linux")]
    {
        let chromium = [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "brave-browser",
            "microsoft-edge",
        ];
        let mut attempts: Vec<(String, Vec<String>)> = chromium
            .iter()
            .map(|b| (b.to_string(), chromium_args(dir, url)))
            .collect();
        attempts.push((
            "firefox".to_string(),
            vec![
                "-profile".into(),
                dir.into(),
                "-no-remote".into(),
                url.into(),
            ],
        ));
        attempts
    }

    #[cfg(target_os = "macos")]
    {
        // macOS 经 `open -na <App> --args <浏览器参数>` 启动指定 App 的独立实例
        let chromium_apps = [
            "Google Chrome",
            "Chromium",
            "Brave Browser",
            "Microsoft Edge",
        ];
        let mut attempts: Vec<(String, Vec<String>)> = chromium_apps
            .iter()
            .map(|app| {
                let mut args = vec!["-na".to_string(), app.to_string(), "--args".to_string()];
                args.extend(chromium_args(dir, url));
                ("open".to_string(), args)
            })
            .collect();
        attempts.push((
            "open".to_string(),
            vec![
                "-na".into(),
                "Firefox".into(),
                "--args".into(),
                "-profile".into(),
                dir.into(),
                "-no-remote".into(),
                url.into(),
            ],
        ));
        attempts
    }

    #[cfg(target_os = "windows")]
    {
        // `cmd /c start "" <bin> ...`：空标题占位，再交给已注册的浏览器名
        let chromium = ["chrome", "msedge", "brave"];
        let mut attempts: Vec<(String, Vec<String>)> = chromium
            .iter()
            .map(|b| {
                let mut args = vec![
                    "/c".to_string(),
                    "start".to_string(),
                    "".to_string(),
                    b.to_string(),
                ];
                args.extend(chromium_args(dir, url));
                ("cmd".to_string(), args)
            })
            .collect();
        attempts.push((
            "cmd".to_string(),
            vec![
                "/c".into(),
                "start".into(),
                "".into(),
                "firefox".into(),
                "-profile".into(),
                dir.into(),
                "-no-remote".into(),
                url.into(),
            ],
        ));
        attempts
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (dir, url, chromium_args);
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn launch_attempts_carry_isolated_profile_and_url() {
        let dir = "/tmp/xkiro-oauth-test";
        let url = "https://example.com/login?idp=Github";
        let attempts = launch_attempts(dir, url);
        assert!(!attempts.is_empty(), "应至少有一个浏览器候选命令");

        // 每条候选都必须携带 URL，且通过独立 profile 隔离（user-data-dir 或 firefox -profile）
        for (bin, args) in &attempts {
            let joined = args.join(" ");
            assert!(joined.contains(url), "{bin} 命令缺少登录 URL: {joined}");
            let isolated = joined.contains(dir)
                && (joined.contains("--user-data-dir") || joined.contains("-profile"));
            assert!(isolated, "{bin} 命令未使用隔离 profile 目录: {joined}");
        }
    }
}
