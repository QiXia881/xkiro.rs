# xkiro.rs

> **本项目是 [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs) 的增强分叉。**
>
> 编译、配置、API、Region、Docker 等基础说明请直接看上游：
>
> - 上游 README: <https://github.com/hank9999/kiro.rs/blob/master/README.md>
> - 上游讨论帖: <https://linux.do/t/topic/1571986>
>
> 本 README 只列 xkiro.rs 在上游基础上多出来的东西，不重复上游已有的内容。

---

## 免责声明

本项目仅供研究使用，Use at your own risk，使用本项目所导致的任何后果由使用人承担，与本项目无关。本项目与 AWS / KIRO / Anthropic / Claude 等官方无关，本项目不代表官方立场。

本分叉新增的所有功能（含系统提示注入、预设、提示词清洗规则等）同样仅供研究与个人授权场景使用。使用者需自行确保使用方式符合所在地区法律、目标 AI 服务的使用条款，以及账号授权范围。任何因功能误用、规避平台政策、生成违法 / 侵权内容、或在未授权环境使用所引发的后果，均由使用人独立承担，与本项目作者及上游作者无关。

## 注意

继承自上游。TLS 默认 rustls，配代理可能要装证书。出问题先把 `config.json` 的 `tlsBackend` 切回 `native-tls`。

持续 Write Failed / 会话卡死 → 看上游 Issue [#22](https://github.com/hank9999/kiro.rs/issues/22) 与 [#49](https://github.com/hank9999/kiro.rs/issues/49)。

---

## 增强功能一览

### 调度与凭据

| 能力 | 说明 |
|------|------|
| FIFO 异步队列 | 每个凭据独立信号量，permit 满了新请求排队等待，不丢单 |
| 多额度调度 | 综合主余额 + 超额额度 + 当前并发数挑凭据 |
| 会话亲和 | 同一会话尽量回到原凭据，提升 prompt cache 命中 |
| 启动预取 | 进程起来就并发拉所有凭据余额 |
| 周期刷新 | 后台定时批量刷余额，单条可强制走云端 |
| 缓存持久化 | 余额缓存原子写盘，重启不丢 |
| 凭据热更新 | 增删改、禁用状态、代理、endpoint 都不用重启 |
| CLI endpoint | 支持 Kiro CLI 拉的 token（区别 IDE 端） |

### 请求转换与压缩

| 能力 | 说明 |
|------|------|
| 8 层压缩管线 | 空白、思考块、tool result 头尾、tool 输入、tool 描述、历史轮数、历史字符、请求体上限 |
| 工具 schema 分级压 | description 与 inputSchema 分别截断 |
| 图像压缩 | 长边、单图像素、多图像素、张数阈值四档可调 |
| Unicode 安全截断 | UAX #29 grapheme cluster，emoji / 中日韩不会被腰斩 |
| 流式 permit 提前释放 | 上游消费完就归还，不被慢客户端拖死并发 |

### 系统提示注入

| 能力 | 说明 |
|------|------|
| 总开关 | `systemPromptEnabled=false` 全部不注入 |
| 内置预设 | `code_complete`（代码完整性）、`concise`（简洁回复）、`chunked_write`（长文件分块写入，规避 AWS 截断死循环） |
| 用户预设 | UI / API 增删改，id 限 `[a-z0-9_-]{1,32}`，禁止与内置冲突 |
| 自定义文本 | 自由文本补充段 |
| 拼接顺序 | 内置预设 → 用户预设 → 自定义文本，按 `\n\n` 连接 |
| 注入位置 | `prepend`（system 头部）/ `append`（system 尾部） |
| 热更新 | Admin API 改完立即生效，并落盘 `config.json` |

### 系统提示清洗（双层）

| 层 | 触发 | 作用 |
|----|------|------|
| Layer-1 | 用户配置开关 | 删 `--- SYSTEM PROMPT ---` 边界、`# Environment` / `# auto memory` 噪音、客户端塞的安全限制段；自定义 regex / 行级过滤 |
| Layer-2 | 始终运行 | 删 xkiro 上轮自注入残留、Kiro IDE 时间戳与 `<execution_discipline>`、收敛连续空行 |

Layer-1 四个内置开关：

- `filterClaudeCode` 命中 ≥2 个 Claude Code 标记 → 替换为精简后端提示
- `filterStripBoundaries` 删 `--- SYSTEM PROMPT ---` / `--- END SYSTEM PROMPT ---`
- `filterEnvNoise` 删环境噪音 section 与 git/build 单行
- `filterStripRestrictions` 起止串硬剥客户端注入的限制段

### OpenAI 兼容层

| 端点 | 说明 |
|------|------|
| `/openai/v1/chat/completions` | OpenAI Chat Completions 协议，复用同一套调度 + 转换 |
| `/openai/v1/models` | 模型列表 |

400 错误（model not found 等）统一规范化。

### Admin API 增强

| 能力 | 说明 |
|------|------|
| 凭据 CRUD | 单条增删改 + 批量导入 token.json |
| 配置热更新 | region / endpoint / 全局代理 / 单凭据代理 |
| 压缩配置 | 压缩管线全字段在线改 |
| 提示词配置 | 清洗规则、注入开关、预设、用户规则 |
| 余额接口 | `/credentials/balances/cached` 双源合并（运行时 + 磁盘） |
| 强制刷新 | 单条强刷走云端 |
| 并发热更新 | 全局 / per-credential 并发上限即时生效 |

### Admin UI

技术栈：React + TanStack Query + Radix UI + Tailwind。构建产物通过 `include_dir!` 嵌入二进制，访问 `/admin` 即可。

| 模块 | 说明 |
|------|------|
| 凭据卡片 | 双进度条（主余额 + 超额额度），剩余额度直读 |
| 实时刷新 | `/credentials/runtime-stats` 1s 轮询并发 / 余额，仅前端可见时跑（切 tab / 失焦 / 2min 无交互自动停） |
| 超额开关 | 乐观更新，点完立即反馈 |
| 批量导入 | 一键导入 token.json |
| 运行时统计 | 每凭据并发、已完成、排队中 |
| 压缩面板 | 11 字段可视化编辑 |
| 清洗开关 | 4 个内置过滤器 + 用户规则增删 |
| 系统提示弹窗 | 4 tab：基础 / 内置 / 用户 / 自定义 |
| 界面缩放 | 整体缩放，适配高 DPI / 大屏 |

---

## 致谢

本分叉的协议层、调度框架、压缩管线、提示词治理思路均建立在前人工作之上：

- **[hank9999/kiro.rs](https://github.com/hank9999/kiro.rs)** — 直接上游。Anthropic ↔ Kiro 转换、Token 管理、流式协议、Admin 基座
- **[BenedictKing/kiro.rs](https://github.com/BenedictKing/kiro.rs)** — 调度策略、会话亲和、后台余额刷新、压缩思路、批量导入格式参考来源
- **[seven7763/kiro.rs](https://github.com/seven7763/kiro.rs)** — 系统提示注入与双层清洗框架的参考实现
- **kiro-account-manager (KAM)** — Tauri 形态的 Kiro 账号管理器，其内置 gateway 的 prompt_filter / 工具命名映射 / schema 归一化等思路被本项目参考
- **[caidaoli/kiro2api](https://github.com/caidaoli/kiro2api)** 与 **[aiclientproxy/proxycast](https://github.com/aiclientproxy/proxycast)** — 上游 README 已致谢的早期实现，启发了协议层的多个细节

向所有前辈致以诚挚的感谢。

## License

MIT（继承自上游）
