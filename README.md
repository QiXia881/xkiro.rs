# xkiro.rs

xkiro.rs 是一个面向 Kiro 凭据聚合、调度、协议转换和可视化管理的一体化代理。它把多种 Kiro 登录来源统一归一到同一套凭据模型，再向下游暴露 Anthropic / Claude Code / OpenAI 兼容接口，并提供 Admin UI 管理凭据、代理池、额度、端点和运行时配置。

本项目只描述 xkiro.rs 当前实现，不代表 AWS、Kiro、Anthropic、Claude、OpenAI 或其它官方项目。

## 免责声明

本项目仅供研究和个人授权场景使用，Use at your own risk。使用者需要自行确保使用方式符合所在地法律、目标服务条款和凭据授权范围。任何因误用、规避平台策略、未授权访问、生成违法或侵权内容等行为导致的后果，均由使用者独立承担。

## 参考项目

xkiro.rs 是兼容和归一化实现，不是简单照搬单个项目。当前登录流程、凭据模型、机器码策略、远程回调和协议细节主要参考并合并了以下项目：

- [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs)：Rust 版 Kiro 代理的基础实现和上游兼容参考。
- [Kiro-Go](https://github.com/zsecducna/Kiro-Go)：Kiro 登录、凭据结构、模型/额度接口和协议兼容逻辑的主要参考来源。
- [Kiro-Go feat/azure-tenant-sso](https://github.com/zsecducna/Kiro-Go/tree/feat/azure-tenant-sso)：Microsoft / Entra ID tenant SSO 流程参考。
- [ngh1105/Kiro-Go](https://github.com/ngh1105/Kiro-Go)：Kiro-Go 变体实现，用于对比登录、凭据和接口行为。
- [kiro-account-manager](https://github.com/hj01857655/kiro-account-manager)：凭据导入导出、账号元数据、machineId 生成与保真字段参考。
- [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)：远程 OAuth 回调、helper 模式和本机浏览器授权回传模式参考。
- [CLIProxyAPIPlus](https://github.com/Ravens2121/CLIProxyAPIPlus)：后台刷新、指纹和 CLIProxyAPI 系列实现对比参考。
- [kiro-gateway](https://github.com/jwadow/kiro-gateway)：Kiro 网关兼容行为和 bracket-style 工具调用解析参考。

最终行为以 xkiro.rs 的兼容规则为准：导入时尽量保留原始凭据字段，运行时再统一归一化 `authMethod`、`provider`、`machineId`、端点、代理和额度信息。

## 核心能力

| 模块 | 能力 |
|------|------|
| 协议代理 | Anthropic Messages、Claude Code、OpenAI Chat Completions、OpenAI Responses 兼容接口 |
| 多凭据调度 | 多账号聚合、余额/overage 感知、并发感知、权重、优先级、禁用状态和失败回退 |
| 凭据模型 | 统一保存 social / idc / external_idp / api_key，兼容 GitHub、Google、AWS Builder ID、IAM Identity Center、Microsoft / Entra ID |
| 登录导入 | Admin UI 发起可复制登录地址，不自动打开浏览器；支持远程部署时本机 helper 回传 |
| 导入导出 | 一个导入入口自动识别完整备份、缓存凭据、外部账号快照和扁平凭据；完整导出保留敏感令牌、设备 ID、区域、SSO 缓存和代理信息 |
| 机器码 | 初次运行生成本机 `machineId`；新凭据可选择使用本机 machineId 或随机 machineId |
| 代理 | 全局代理、单凭据代理、`direct` 直连、代理池 `proxyId`、代理池自动分配和验活前预分配 |
| Admin UI | 凭据、余额、overage、代理池、登录、导入导出、请求日志、设置和系统提示管理 |
| 端点 | 支持 `ide`、`codewhisperer`、`amazonq`、`cli`，并可按凭据覆盖和故障转移 |
| 运行时配置 | API Key、Admin Key、Thinking、端点、代理、Prompt Filter、系统提示、并发和余额刷新热更新 |

## 快速启动

### 本地运行

```bash
cargo build
./target/debug/xkiro-rs init
./target/debug/xkiro-rs
```

默认读取当前目录下的：

- `config.json`
- `credentials.json`

如果 `config.json` 不存在，直接运行 `xkiro-rs` 也会进入初始化向导。

### 开发模式

```bash
./dev.sh
```

`dev.sh` 用于本地调试：

- 后端配置固定读取 `target/debug/config.json`
- 凭据固定读取 `target/debug/credentials.json`
- 自动构建并启动 debug 后端
- 同时启动 `admin-ui` 的 Vite dev server
- 如果监听端口被旧的 `target/debug/xkiro-rs` 占用，会自动停止旧后端
- 如果占用端口的不是 xkiro.rs 后端，会提示手动处理

Admin UI 开发模式要求 `target/debug/config.json` 中配置了 `adminApiKey`，否则前端登录后 Admin API 会返回 404。

常用命令：

```bash
./dev.sh init --force
./dev.sh social-helper --server http://127.0.0.1:8080 --session <session-id> --provider GitHub
```

### Docker

镜像工作目录为 `/app`，配置目录为 `/app/config`。

```bash
docker run --rm -it \
  -p 8990:8990 \
  -v "$PWD/config:/app/config" \
  ghcr.io/qixia881/xkiro-rs:latest
```

Docker 场景建议 `config.json` 使用：

```json
{
  "host": "0.0.0.0",
  "port": 8990,
  "apiKey": "replace-with-client-token",
  "adminApiKey": "replace-with-admin-token",
  "requireApiKey": true
}
```

## 配置文件

最小 `config.json`：

```json
{
  "host": "127.0.0.1",
  "port": 8080,
  "apiKey": "replace-with-client-token",
  "adminApiKey": "replace-with-admin-token",
  "requireApiKey": true
}
```

常用字段：

| 字段 | 说明 |
|------|------|
| `host` / `port` | 后端监听地址和端口 |
| `apiKey` | 下游客户端访问 `/v1/messages` 等接口的 Bearer Token |
| `adminApiKey` | 开启 Admin API 和 `/admin` UI；留空则不启用管理面板 |
| `requireApiKey` | 是否强制校验下游客户端 API Key |
| `region` / `authRegion` / `apiRegion` | 默认区域、认证区域和 API 区域 |
| `machineId` | 本机 machineId，初始化时生成 |
| `credentialMachineIdStrategy` | 新凭据缺少 machineId 时使用 `random` 或 `local` |
| `tlsBackend` | `rustls` 或 `native-tls`；代理或证书异常时可切换 |
| `proxyUrl` / `proxyUsername` / `proxyPassword` | 全局 HTTP/SOCKS 代理 |
| `defaultEndpoint` | 默认端点，默认 `ide` |
| `preferredEndpoint` | 首选端点策略，默认 `auto` |
| `endpointFallback` | 端点瞬态失败时是否尝试其它端点 |
| `allowOverUsage` | 全局 overage 允许开关 |
| `perCredentialConcurrency` | 单凭据默认并发上限 |
| `globalConcurrency` | 全局并发上限，`0` 表示不限 |
| `acquireWaitTimeoutSecs` | 等待并发 permit 的超时时间 |
| `balanceRefreshEnabled` | 是否启用后台周期余额刷新 |
| `balanceRefreshIntervalSecs` | 周期余额刷新间隔，低于 180 会被钳制 |
| `balanceRefreshConcurrency` | 周期余额刷新并发，最大 10 |
| `sessionAffinityEnabled` | 是否启用会话/API Key 亲和调度 |
| `privacyMode` | Admin UI 隐私展示模式 |
| `preciseTokenCounting` | 是否启用 tiktoken cl100k_base 精确计数 |

## 数据文件

| 文件 | 说明 |
|------|------|
| `credentials.json` | 凭据文件，支持单对象或数组，多凭据会按优先级排序 |
| `proxies.json` | 代理池文件，位于 `credentials.json` 同目录 |
| `kiro_balance_cache.json` | Admin 余额磁盘缓存，位于 `credentials.json` 同目录 |
| `kiro_stats.json` | 运行统计缓存 |
| `kiro_api_keys.json` | Admin UI 创建的多客户端 API Key 存储 |

`credentials.json` 示例：

```json
[
  {
    "authMethod": "social",
    "provider": "GitHub",
    "refreshToken": "replace-with-refresh-token",
    "region": "us-east-1",
    "priority": 0,
    "weight": 1,
    "concurrency": 1,
    "endpoint": "ide"
  }
]
```

也可以通过环境变量临时注入 Kiro API Key 凭据：

```bash
KIRO_API_KEY=ksk_xxxxx ./target/debug/xkiro-rs
```

## 凭据与登录

Admin UI 的“添加凭据”支持：

| 登录方式 | 说明 |
|----------|------|
| AWS Builder ID | 设备授权流程，适合个人 Builder ID |
| GitHub / Google | 社交 OAuth，可手动复制登录地址，也可用 `social-helper` 在本机浏览器完成授权并回传远程服务 |
| IAM Identity Center | 企业 SSO OIDC / authorization-code 流程 |
| Microsoft / Entra ID | external IdP / tenant SSO，保留 Microsoft issuer、token endpoint、scopes 等字段 |
| SSO Token | 批量导入 SSO bearer token，并执行完整导入流程 |
| 本地缓存 / Web Cookie | 从本地缓存或 Web 侧材料提取并归一化为 xkiro.rs 凭据 |
| API Key | Headless 模式，直接以 API Key 作为上游 Bearer Token |

登录流程不会自动打开浏览器标签页。UI 会展示可复制地址，建议用户复制到无痕窗口、隐私窗口或其它浏览器完成登录，避免当前浏览器已有会话导致选错账号。

远程部署时，GitHub / Google 可使用 helper 模式：

```bash
./xkiro-rs social-helper \
  --server https://your-xkiro.example.com \
  --session <session-id-from-admin-ui> \
  --provider GitHub
```

`authMethod` 会被归一化为：

| 归一化值 | 常见 provider |
|----------|---------------|
| `social` | `GitHub`、`Google`、`Kiro SSO` |
| `idc` | `BuilderId`、`Enterprise` |
| `external_idp` | `AzureAD`、`Microsoft / Entra ID` |
| `api_key` | API Key / headless 凭据 |

## 导入与导出

导入只有一个统一入口，后端会自动识别来源格式：

| sourceFormat | 说明 |
|--------------|------|
| `xkiro.credentials.bundle` | xkiro.rs 完整备份格式 |
| `compatible.cache-record` | 兼容缓存凭据记录 |
| `external.account-export` | 外部账号导出快照 |
| `flat.credentials` | 扁平凭据对象或数组 |

导入模式：

| 模式 | 行为 |
|------|------|
| `skipExisting` | 已存在凭据跳过 |
| `mergeMissing` | 只补齐已有凭据缺失字段 |
| `replaceExisting` | 覆盖已有凭据的令牌、设备 ID、区域、代理和 SSO 缓存字段 |

完整导出会包含敏感信息，包括访问令牌、刷新令牌、API Key、设备 ID、区域、SSO 缓存、代理 URL/账号密码和可用模型缓存。只应在可信环境保存和传输。

## 代理与代理池

xkiro.rs 支持三层代理：

| 层级 | 说明 |
|------|------|
| 全局代理 | `config.json` 的 `proxyUrl`，所有无单独设置的凭据默认使用 |
| 单凭据代理 | 凭据内 `proxyUrl` / `proxyUsername` / `proxyPassword`，优先于全局代理 |
| 代理池 | `proxies.json` 独立维护代理，凭据只保存 `proxyId`，运行时回填真实代理 |

特殊值：

- `proxyUrl: "direct"` 表示该凭据强制直连，即使全局配置了代理。
- 绑定 `proxyId` 后，凭据文件只保留代理池引用，避免代理 URL 和账号密码散落在凭据记录里。

代理池能力：

- 支持 `http://`、`https://`、`socks5://`、`socks5h://`
- 支持代理账号密码
- 支持区域、国家、备注和最大并发
- 支持批量导入代理文本
- 支持代理连通性测试和自动记录健康状态
- 支持按凭据 region、当前绑定负载和可用并发自动分配
- 文件导入和 OAuth / SSO 登录验活之前会先分配代理，避免验活请求走错出口

## 调度与额度

调度目标是尽量让多凭据在额度、并发和可用性之间平衡：

- 每个凭据有独立并发信号量，默认严格串行。
- 可设置全局并发上限。
- 支持凭据权重，`weight >= 2` 会增加调度份额。
- 支持优先级，数字越小越优先。
- 调度会考虑主额度、overage 余额、当前并发、禁用状态、模型支持和失败状态。
- 流式请求在上游响应完成后提前释放 permit，不被慢客户端拖住上游并发。
- 启动后可并发预取余额，后台可周期刷新余额和订阅信息。
- 余额缓存同时存在运行时缓存和磁盘缓存，重启后可恢复最近状态。
- 可选会话/API Key 亲和调度，用于提高 prompt cache 命中率；默认关闭以便多账号天然平摊。

FREE 订阅默认不显示 overage 余额；非 FREE 订阅才展示超额余额、超额上限、超额状态等信息。

## API 端点

公开健康检查：

| 方法 | 路径 |
|------|------|
| `GET` | `/health` |
| `GET` | `/` |

模型列表公开，其它模型请求默认需要 `Authorization: Bearer <apiKey>` 或 `x-api-key: <apiKey>`。

| 协议 | 端点 |
|------|------|
| Anthropic | `POST /v1/messages` |
| Anthropic token count | `POST /v1/messages/count_tokens` |
| Claude Code | `POST /cc/v1/messages` |
| Claude Code token count | `POST /cc/v1/messages/count_tokens` |
| OpenAI Chat Completions | `POST /v1/chat/completions` |
| OpenAI Responses | `POST /v1/responses` |
| Models | `GET /v1/models`、`GET /models` |
| Stats | `GET /v1/stats` |
| Telemetry sink | `POST /api/event_logging/batch` |

示例：

```bash
curl http://127.0.0.1:8080/v1/models
```

```bash
curl http://127.0.0.1:8080/v1/messages \
  -H "Authorization: Bearer replace-with-client-token" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [
      {"role": "user", "content": "hello"}
    ]
  }'
```

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer replace-with-client-token" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4.1",
    "messages": [
      {"role": "user", "content": "hello"}
    ]
  }'
```

## Admin UI

配置 `adminApiKey` 后访问：

```text
http://127.0.0.1:8080/admin
```

Admin API 同时挂载在：

- `/api/admin`
- `/admin/api`

Admin API 支持 `Authorization: Bearer <adminApiKey>` 或 `x-api-key: <adminApiKey>`。

主要页面和能力：

| 模块 | 能力 |
|------|------|
| Dashboard | 凭据卡片、余额、订阅、overage、运行时并发、请求统计 |
| 添加凭据 | Builder ID、GitHub、Google、IAM Identity Center、Microsoft / Entra ID、SSO Token、本地缓存、Web Cookie、API Key |
| 导入导出 | 统一导入入口、dry run 预览、三种合并模式、完整 xkiro.rs 备份导出 |
| 代理池 | 添加、导入、测试、删除、按区域/负载自动分配代理 |
| 设置 | 常用、访问控制、Thinking、端点、全局代理、Prompt Filter |
| 模型 | 查询单凭据可用模型、刷新模型缓存 |
| 日志 | 请求日志查看和清空 |
| 系统提示 | 内置预设、用户预设、自定义提示、prepend/append 注入位置 |
| API Key | 管理多个下游客户端 API Key 和用量 |

## 请求转换

xkiro.rs 在协议转换层处理以下兼容问题：

- Anthropic Messages 与 Kiro 请求格式互转。
- OpenAI Chat Completions / Responses 转 Anthropic/Kiro 请求。
- OpenAI `reasoning_content` 与 Claude `thinking` 格式可配置。
- 支持流式和非流式响应。
- 支持 tool use / tool result 映射。
- 支持 bracket-style 工具调用兜底解析。
- 支持工具名称映射和流式反向映射。
- 支持图片输入透传，包含 `image`、`image_url`、data URL 和多种 media type。
- 支持 `maxRequestBodyBytes` 请求体上限控制。
- 支持 Prompt Cache 记账和 cache ratio 上限。
- 支持 Prompt Filter 清理客户端注入的噪声 system prompt。

## 系统提示与 Prompt Filter

系统提示注入默认关闭。开启后可组合：

- 内置预设
- 用户自定义预设
- 自定义文本
- 插入位置：`prepend` 或 `append`

Prompt Filter 可清理：

- Claude Code / IDE 注入的环境噪声
- `--- SYSTEM PROMPT ---` 边界
- 客户端限制段
- 自定义 `contains`、`regex`、`lines-containing` 规则

## 端点

可用端点：

| endpoint | 说明 |
|----------|------|
| `ide` | 默认 Kiro IDE 风格端点 |
| `codewhisperer` | CodeWhisperer / Q 风格端点 |
| `amazonq` | Amazon Q data-plane 端点 |
| `cli` | Kiro CLI 令牌端点 |

可通过 `config.defaultEndpoint` 设置默认端点，也可在单条凭据中设置 `endpoint` 覆盖。启用 `endpointFallback` 后，瞬态错误会尝试备用端点；认证失败、额度问题等致命错误不会盲目 fallback。

## 开发

前端：

```bash
cd admin-ui
pnpm install
pnpm dev
pnpm build
```

后端：

```bash
cargo check
cargo test
cargo build --release
```

完整本地开发：

```bash
./dev.sh
```

构建产物会把 `admin-ui/dist` 嵌入二进制，生产运行不需要单独启动前端服务。

## 常见问题

### 进入 `/admin` 后 API 404

通常是 `adminApiKey` 为空，后端没有挂载 Admin API 和 Admin UI。重新运行：

```bash
./xkiro-rs init --force
```

或直接编辑 `config.json` 增加 `adminApiKey`。

### 代理或 TLS 证书异常

默认 `tlsBackend` 是 `rustls`。如果本机代理或系统证书链不兼容，可尝试：

```json
{
  "tlsBackend": "native-tls"
}
```

### 登录时为什么不自动打开浏览器

这是刻意设计。登录地址需要复制到无痕窗口、隐私窗口或其它浏览器中打开，避免当前浏览器已有 GitHub / Google / Microsoft 会话导致导入错账号。

### 端口被占用

`dev.sh` 只会自动停止旧的 `target/debug/xkiro-rs` 后端进程。如果占用端口的是其它进程，需要手动停止。

### 凭据导出文件能否公开

不能。完整导出包含刷新令牌、访问令牌、API Key、设备 ID、SSO 缓存和代理凭据，只能在可信环境保存。

## License

MIT
