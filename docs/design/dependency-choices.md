# 依赖选型

## 1. 定位

本文档记录 Rust 版每一项核心依赖的选型决策和 rationale。它不是 `Cargo.toml` 本身，但所有 `Cargo.toml` 的依赖都必须与本文档一致。

选型原则（按优先级）：

1. 正确性与类型安全
2. 生态稳定性（维护活跃度、issue 响应、breaking change 频率）
3. 编译速度与二进制体积
4. API 人体工学
5. 极致性能（在前四项满足后考虑）

## 2. 依赖清单

### 2.1 异步运行时

| 选项 | 决策 | 理由 |
|---|---|---|
| **tokio** | ✅ 采用 | 事实标准，生态最广，sqlx / reqwest / octocrab 均依赖 |
| async-std | ❌ | 生态较窄，与 sqlx 兼容性不如 tokio |
| smol | ❌ | 太轻量，缺乏成熟 I/O 集成 |

features：`rt-multi-thread`, `macros`, `time`, `signal`

### 2.2 HTTP 客户端

| 选项 | 决策 | 理由 |
|---|---|---|
| **reqwest** | ✅ 采用 | API 最友好，原生支持代理、超时、重试、gzip |
| hyper | ❌ | 太底层，需自建连接池和重定向 |
| ureq | ❌ | 同步阻塞，不适合异步架构 |

features：`rustls-tls`（不用 native-tls，避免 OpenSSL 依赖）, `gzip`, `json`, `cookies`

### 2.3 TLS

| 选项 | 决策 | 理由 |
|---|---|---|
| **rustls** | ✅ 采用 | 纯 Rust，跨平台一致，无系统 OpenSSL 依赖 |
| native-tls | ❌ | 跨平台行为不一致，Docker 镜像需额外安装 OpenSSL |

CA 证书：使用 `webpki-roots` 内嵌 Mozilla CA bundle，加上 `rustls-native-certs` 做 fallback。

### 2.4 Feed 解析

| 选项 | 决策 | 理由 |
|---|---|---|
| **feed-rs** | ✅ 采用 | 统一 RSS 1.0/2.0、Atom、JSON Feed 解析到 `Feed` 模型 |
| rss + atom_syndication | ❌ | 需要分别处理两种格式，增加适配代码 |

注意：`feed-rs` 解析结果需要在 `feed` crate 内转换为 `domain::FeedEntryMeta`，不直接暴露 `feed-rs` 类型到 `domain`。

### 2.5 数据库

| 选项 | 决策 | 理由 |
|---|---|---|
| **sqlx** | ✅ 采用 | 编译期 SQL 校验、原生异步、同时支持 SQLite 和 PG |
| diesel | ❌ | ORM 风格与本项目显式 SQL 的设计哲学不符 |
| rusqlite | ❌ | 仅支持 SQLite，不能满足 PG 替换目标 |
| sea-orm | ❌ | 抽象层过重 |

features：`runtime-tokio`, `sqlite`, `postgres`, `time`, `migrate`

编译期校验策略：开发环境使用 `sqlx::query!` 宏做编译期校验；CI 使用 `SQLX_OFFLINE=true` + `.sqlx/` 缓存文件。

### 2.6 序列化

| 选项 | 决策 | 理由 |
|---|---|---|
| **serde** + **serde_json** | ✅ 采用 | Rust 事实标准 |
| **toml** | ✅ 采用 | 配置文件解析 |

### 2.7 时间

| 选项 | 决策 | 理由 |
|---|---|---|
| **time** | ✅ 采用 | sqlx 原生支持，API 清晰；用于数据库 `TIMESTAMP` 列的读写与 UTC 计算 |
| **jiff** | ✅ 采用 | IANA 时区数据库内置，`Zoned` / `Timestamp` 类型区分清晰，纳秒精度，DST 正确处理 |
| chrono | ❌ | 历史包袱重，`NaiveDateTime` 设计易引入时区 bug |
| time-tz | ❌ | 相较 `jiff`，API 更笨重，维护活跃度更低 |

**职责分工**：

- `time`：所有 DB 读写（`OffsetDateTime`、`PrimitiveDateTime`），UTC-only 场景
- `jiff`：任何涉及 `target_timezone`（IANA 时区名）的业务逻辑，如：
  - 根据分类配置的时区计算 `report_date`
  - 发布文件名与 frontmatter 中的"本地日"语义
  - 发布窗口（`publish_window_start_local` 等）与 UTC 的换算

**转换约定**：跨边界时显式转换 `time::OffsetDateTime ↔ jiff::Timestamp`，不引入隐式类型互转。`domain` crate 暴露 `TimeZone` 新类型包装 `jiff::tz::TimeZone`，避免下游 crate 直接依赖 `jiff` 类型。

### 2.8 CLI

| 选项 | 决策 | 理由 |
|---|---|---|
| **clap** | ✅ 采用 | derive 宏支持，子命令、补全、帮助生成 |

features：`derive`, `env`

### 2.9 配置与环境

| 选项 | 决策 | 理由 |
|---|---|---|
| **dotenvy** | ✅ 采用 | `dotenv` 的维护活跃 fork |

`app.toml` / `categories/*.toml` 直接用 `toml` + `serde` 反序列化，不引入额外的配置框架（如 `config` crate）。理由：配置结构已完全确定，框架的动态合并能力是不必要的复杂度。

### 2.10 日志与可观测性

| 选项 | 决策 | 理由 |
|---|---|---|
| **tracing** | ✅ 采用 | 结构化、span 嵌套、生态最广 |
| **tracing-subscriber** | ✅ 采用 | 格式化输出、过滤、Layer 组合 |
| **metrics** | ✅ 采用（Phase 5）| 轻量、Prometheus 格式原生支持 |
| **metrics-exporter-prometheus** | ✅ 采用（Phase 5）| HTTP endpoint 暴露 |

### 2.11 正文提取

| 选项 | 决策 | 理由 |
|---|---|---|
| **readability** (Rust port) | ✅ 首选 | Mozilla Readability 的 Rust 实现 |
| **scraper** | ✅ 辅助 | CSS 选择器提取，自定义规则场景 |

注意：Rust 生态的 readability 实现不如 Python 的 `trafilatura` 成熟。如果 Rust 实现质量不足，备选方案是通过 HTTP 调用一个轻量的 readability 微服务。`extractor` crate 通过 trait 抽象提取策略，替换不影响上层。

### 2.12 AI 客户端

| 选项 | 决策 | 理由 |
|---|---|---|
| **async-openai** | ✅ 采用 | 类型安全、原生异步、支持 custom base URL |
| 手写 reqwest 调用 | ❌ | 需自建重试、流式、错误类型 |

`async-openai` 支持通过 `OpenAIConfig::new().with_api_base()` 设置自定义 endpoint，兼容所有 OpenAI-compatible API。

### 2.12b AI 速率限制

| 选项 | 决策 | 理由 |
|---|---|---|
| **governor** | ✅ 采用 | `DirectRateLimiter` + `KeyedRateLimiter`，GCRA 算法，零分配路径 |
| tower::limit::RateLimit | ❌ | 与 async-openai（非 tower Service）集成成本高 |
| 手写 `Semaphore` + `tokio::time::sleep` | ❌ | 无法正确实现 token bucket / GCRA |

**用途**：

- 单 `model_id` 粒度的 RPM / TPM 限制（配置见 [config-schema](./config-schema.md)）
- 多模型共用 HTTP 客户端但独立限速，通过 `KeyedRateLimiter<String>` 实现
- 命中限速时 `ai` crate 返回 `AiError::RateLimited { retry_after }`，由 runtime 释放 lease 并在下一个批次重试

**不解决**：provider 返回的 429 与本地限速器不同步。`governor` 只做客户端预算，服务端 429 仍需 `async-openai` 的 retry-after header 配合处理。

### 2.13 GitHub 发布

| 选项 | 决策 | 理由 |
|---|---|---|
| **octocrab** | ✅ 采用 | GitHub API 最成熟的 Rust 客户端 |

tree commit 推送需要使用 octocrab 的低级 API（`repos().create_tree` / `repos().create_commit` / `repos().update_ref`）。如果 octocrab 不暴露这些接口，回退到 `reqwest` 直接调用 GitHub REST API。

### 2.14 哈希

| 选项 | 决策 | 理由 |
|---|---|---|
| **sha2** | ✅ 采用 | SHA-256，用于 content_hash / link_hash / artifact sha256 |

### 2.15 唯一标识

| 选项 | 决策 | 理由 |
|---|---|---|
| **ulid** | ✅ 采用 | `run_id` 生成，时间有序且全局唯一 |

### 2.16 URL 处理

| 选项 | 决策 | 理由 |
|---|---|---|
| **url** | ✅ 采用 | 标准 URL 解析与规范化 |

link 规范化逻辑（去 fragment、排序 query、去 tracking 参数等）在 `domain` crate 的 `link_normalizer` 模块自行实现，`url` crate 只负责解析。

### 2.17 错误处理

| 选项 | 决策 | 理由 |
|---|---|---|
| **thiserror** | ✅ 采用 | 库代码：派生 `Error` trait，类型安全 |
| **anyhow** | ❌ 不采用 | 本项目要求分类错误，anyhow 的类型擦除与此冲突 |

所有 crate 使用 `thiserror` 定义具体错误类型。`anyhow` 仅在 `app` crate 的 `main` 入口处有限使用（如果需要）。

### 2.18 测试

| 选项 | 决策 | 理由 |
|---|---|---|
| **tokio::test** | ✅ 采用 | 异步测试 |
| **mockall** | ✅ 采用 | trait mock，用于单元测试 |
| **tempfile** | ✅ 采用 | 临时文件/目录，用于 SQLite 测试 |
| **wiremock** | ✅ 采用 | HTTP mock server，用于集成测试 |
| **insta** | ✅ 考虑 | 快照测试，用于报告渲染验证 |

## 3. 不采用的依赖

| 依赖 | 排除理由 |
|---|---|
| `chrono` | 与 `time` 功能重叠，`time` 更轻且 sqlx 原生支持 |
| `diesel` | ORM 抽象与显式 SQL 设计不符 |
| `anyhow` | 类型擦除与分类错误体系冲突 |
| `log` | 被 `tracing` 完全替代 |
| `config` (crate) | 动态合并能力过度，本项目配置结构固定 |
| `native-tls` | 跨平台不一致，增加系统依赖 |

## 4. 版本锁定策略

- `Cargo.lock` 必须提交到 git
- 依赖版本使用 `x.y` 形式（如 `reqwest = "0.12"`），不用 `*` 或 `>=`
- 每月一次 `cargo update`，CI 验证通过后合并
- 安全审计：使用 `cargo audit` 作为 CI 检查项

## 5. workspace 依赖管理

所有共享依赖在根 `Cargo.toml` 的 `[workspace.dependencies]` 中统一声明版本，各 crate 通过 `dependency.workspace = true` 引用。避免版本不一致。

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "signal"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "gzip", "json"] }
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite", "postgres", "time", "migrate"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
thiserror = "2"
clap = { version = "4", features = ["derive", "env"] }
time = { version = "0.3", features = ["serde", "formatting", "parsing"] }
jiff = { version = "0.1", features = ["serde"] }
sha2 = "0.10"
url = "2"
ulid = "1"
dotenvy = "0.15"
feed-rs = "2"
async-openai = "0.27"
governor = "0.6"
octocrab = "0.44"
```

注意：以上版本号为撰写时的最新稳定版，实际初始化 workspace 时应以 `cargo add` 查到的最新版为准。

## 6. 与宪法的对齐检查

- §2.4 优先级排序：选型优先正确性与稳定性，性能在最后考虑 ✓
- §2.5 根基兼容性：rustls + webpki-roots 保证 CA 证书兼容 ✓
- 轻量化依赖原则：不引入 ORM、不引入配置框架、不引入 anyhow ✓
- 可替换性：核心外部依赖（AI client、publisher、extractor）通过 trait 抽象隔离 ✓
