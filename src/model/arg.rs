use clap::{Parser, Subcommand};

/// Anthropic <-> Kiro API 客户端
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 配置文件路径
    #[arg(short, long)]
    pub config: Option<String>,

    /// 凭证文件路径
    #[arg(long)]
    pub credentials: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// 子命令
#[derive(Subcommand, Debug)]
pub enum Command {
    /// 交互式生成最小可运行的 config.json（host/port/apiKey/adminApiKey）
    ///
    /// 仅写入用户回答过的字段，其它字段全部走 Config 默认值。
    /// adminApiKey 留空 → Admin API + Admin UI 不启用。
    Init {
        /// 强制覆盖已存在的配置文件（默认遇到已存在文件会确认）
        #[arg(long)]
        force: bool,
    },

    /// 本机 Social 登录助手（远程部署场景）
    ///
    /// 在用户本机完成 GitHub/Google OAuth（本机回调 + token 交换），
    /// 再把最终凭据回传给远程 xkiro 服务，绕开"回调只能命中本机"的限制。
    /// 配合 Admin UI 的 helper 模式会话使用。
    SocialHelper {
        /// 远程 xkiro 服务地址，例如 https://kiro.example.com
        #[arg(long)]
        server: String,

        /// Admin UI 发起 helper 登录返回的会话 ID
        #[arg(long)]
        session: String,

        /// 登录提供方：Github 或 Google
        #[arg(long)]
        provider: String,

        /// Admin API Key（留空则从 stdin 读取，避免泄露到终端历史）
        #[arg(long)]
        api_key: Option<String>,

        /// Kiro 认证端点（默认 prod.us-east-1.auth.desktop.kiro.dev）
        #[arg(long)]
        auth_endpoint: Option<String>,

        /// 本机换 token 时使用的代理（http/https/socks5）
        #[arg(long)]
        proxy: Option<String>,
    },
}
