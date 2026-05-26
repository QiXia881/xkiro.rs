//! 交互式初始化向导
//!
//! 通过命令行问答生成最小可运行的 `config.json`：
//! - host / port / apiKey 必填，提供默认值
//! - adminApiKey 选填；留空则 Admin API + Admin UI 全部禁用
//! - 其它字段（region / 压缩 / 提示词等）全部使用 [`Config::default`] 兜底
//!
//! 设计目标：让首次跑 xkiro-rs 的人 30 秒内拿到能用的配置文件，
//! 同时把"前端入口需要 adminApiKey"这件事讲清楚。

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::config::Config;

/// 默认 API Key（与下游客户端约定的访问凭据）
fn suggest_api_key() -> String {
    // 16 字节十六进制：足够熵 + 输入方便
    let bytes: [u8; 16] = std::array::from_fn(|_| fastrand::u8(..));
    bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{:02x}", b);
        acc
    })
}

/// 提示并读取一行输入
fn prompt(msg: &str, default: Option<&str>) -> Result<String> {
    if let Some(d) = default {
        print!("{} [{}]: ", msg, d);
    } else {
        print!("{}: ", msg);
    }
    io::stdout().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line).context("读取输入失败")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.unwrap_or("").to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// y/N 二元提示，回车默认 No
fn prompt_yes_no(msg: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{} [{}]: ", msg, hint);
    io::stdout().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line).context("读取输入失败")?;
    let trimmed = line.trim().to_lowercase();
    Ok(match trimmed.as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    })
}

/// 交互式生成 `config.json`
///
/// `path` 为目标文件路径；`force=true` 时覆盖已存在文件。
pub fn run_init(path: &Path, force: bool) -> Result<()> {
    println!();
    println!("============================================================");
    println!("  xkiro-rs 配置向导");
    println!("============================================================");
    println!("将生成最小可运行的 config.json，回车使用括号内默认值。");
    println!("除此处询问的字段外，其它配置（region/压缩/提示词等）使用内置默认值。");
    println!();

    if path.exists() && !force {
        let overwrite = prompt_yes_no(
            &format!("文件 {} 已存在，覆盖？", path.display()),
            false,
        )?;
        if !overwrite {
            println!("已取消。");
            return Ok(());
        }
    }

    // host / port
    let host = prompt("监听地址 host", Some("127.0.0.1"))?;
    let port_str = prompt("监听端口 port", Some("8080"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| anyhow::anyhow!("端口必须是 0-65535 的整数: {}", port_str))?;

    // apiKey（下游客户端访问 xkiro-rs 时的 Bearer Token）
    println!();
    println!("【apiKey】下游客户端调用 /v1/messages 等接口时携带的 Bearer Token。");
    println!("         留空将拒绝启动；建议使用随机生成的默认值。");
    let suggested_api_key = suggest_api_key();
    let api_key = prompt("apiKey", Some(&suggested_api_key))?;
    if api_key.is_empty() {
        bail!("apiKey 不能为空");
    }

    // adminApiKey（开启 Admin API + Admin UI 的开关）
    println!();
    println!("【adminApiKey】开启 Admin API + 浏览器 Admin UI（/admin 入口）的密钥。");
    println!("              留空则不启用 Admin 功能（仅作为代理使用，无前端）。");
    println!("              强烈建议设置：可视化管理凭据 / 余额 / 压缩 / 系统提示。");
    let admin_api_key = prompt("adminApiKey（留空跳过）", None)?;
    let admin_api_key = if admin_api_key.is_empty() {
        None
    } else {
        Some(admin_api_key)
    };

    // 图片压缩开关
    println!();
    println!("【图片压缩】对入站图片做缩放 + 重编码，节省 token + 请求体。");
    println!("            上游若已压缩（如 TRAE 国际版），重复压缩会损失质量，建议关闭。");
    let image_compression_enabled = prompt_yes_no("启用图片压缩？", true)?;

    // 组装配置：从 default 起步，仅覆盖问到的字段
    let mut config = Config::default();
    config.host = host;
    config.port = port;
    config.api_key = Some(api_key);
    config.admin_api_key = admin_api_key.clone();
    config.compression.image_compression_enabled = image_compression_enabled;

    // 写盘
    let content = serde_json::to_string_pretty(&config).context("序列化配置失败")?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建父目录失败: {}", parent.display()))?;
        }
    }
    std::fs::write(path, content)
        .with_context(|| format!("写入配置文件失败: {}", path.display()))?;

    println!();
    println!("------------------------------------------------------------");
    println!("配置已写入: {}", path.display());
    println!(
        "  监听: {}:{}",
        config.host, config.port
    );
    if admin_api_key.is_some() {
        println!(
            "  Admin UI: http://{}:{}/admin (使用 adminApiKey 登录)",
            if config.host == "0.0.0.0" {
                "127.0.0.1"
            } else {
                &config.host
            },
            config.port
        );
    } else {
        println!("  Admin UI: 未启用（adminApiKey 留空）");
    }
    println!(
        "  图片压缩: {}",
        if image_compression_enabled {
            "启用"
        } else {
            "禁用（透传原图）"
        }
    );
    println!("------------------------------------------------------------");
    println!("下一步：");
    println!("  1. 准备 credentials.json（社交登录或 idc 账号）");
    println!("  2. 直接运行 xkiro-rs 即可启动服务");
    println!();

    Ok(())
}
