;;; modules.lisp — 12 crate 模块清单
;;;
;;; 格式说明见 map/README.md。本文件只描述 crate 节点 + crate 间依赖关系。
;;; 单个 crate 内部的关键符号 / Flow / 状态机见 architecture-plan.lisp 与
;;; architecture-code.lisp。
;;;
;;; 依赖关系（:downstream）取自各 crate 的 Cargo.toml [dependencies] 中的
;;; 工作区成员声明，已 dedup workspace.aliases 与 dev-deps 重复。
;;; 校对脚本可选：../../scripts/map-deps.sh （< 100 行）。

(crate :id rss-ai-news
       :label "二进制入口（根 crate）"
       :layer interaction-shell
       :path "src/main.rs"
       :downstream (cli)
       :state active
       :notes "仅 main.rs，把控制权交给 cli::run。详见 plan/09-cli-and-runtime.md")

(crate :id cli
       :label "CLI surface：clap derive 子命令"
       :layer instruction-interface
       :path "crates/cli/"
       :downstream (domain config runtime storage feed extractor ai publish observability)
       :state active
       :notes "12 个子命令（ingest / ai-run / publish / publish-all / run / migrate / validate-config / doctor / replay / backfill / reindex / rebuild-report）。
               入口 crates/cli/src/lib.rs::run。args.rs 维护 clap derive 结构。
               注意：cli 不直接拉 report，渲染发生在 runtime/flows/publish/ 中调 report crate。
               注意：没有独立的 `extract` 子命令 —— 抓正文与抓 feed 合并在 `ingest`、`run`、`replay --kind=html`、`backfill --target=extract` 等入口里。")

(crate :id domain
       :label "本体对象 + 状态机 + DTO"
       :layer object
       :path "crates/domain/"
       :downstream ()
       :state active
       :notes "8 本体对象 + 4 状态机集中定义。
               4 状态机：FeedEntryState / ArticleState / AiResultState / PublishState。
               见 plan/00-overview.md §2 与 plan/08-state-machines.md。
               domain 是叶子 crate，下游为空，被几乎所有上层 crate 依赖。")

(crate :id config
       :label "配置加载 + 校验"
       :layer object
       :path "crates/config/"
       :downstream (domain)
       :state active
       :notes "三层加载：.env (EnvConfig) + app.toml (AppConfig) + categories/*.toml (CategoryConfig)。
               compute_config_sha256 提供版本指纹（版本行轮换在 storage 层
               rotate_active_config，W16 见 plan/16-config-versioning.md）。
               validate.rs 把 ConfigError + DiagnosticReport 一次性吐给 CLI。
               详见 plan/06-config.md。")

(crate :id runtime
       :label "流程协调层：Flow 编排 + RunContext"
       :layer flow-coord
       :path "crates/runtime/"
       :downstream (domain config storage feed extractor ai report publish observability)
       :state active
       :notes "8 个 Flow 模块对应主链路 + reindex + backfill + rebuild_report。
               RunContext 是接缝点：承载 6 capability clients + 10 Repository traits。
               events.rs::RunEventEmitter 强制 redaction。
               artifact.rs::ArtifactWriter 控制 raw_artifacts 留档。
               详见 plan/09-cli-and-runtime.md。")

(crate :id storage
       :label "持久层：sqlx 双方言 + Repository"
       :layer capability
       :path "crates/storage/"
       :downstream (config domain)
       :state active
       :notes "StoragePool enum 统一封装 SqlitePool + PgPool。
               13 个 Repository trait + 实现，claim+lease 模式。
               migrations/sqlite/ + migrations/postgres/ 编号一一对应。
               详见 plan/05-storage.md。")

(crate :id feed
       :label "Feed 抓取 + 解析"
       :layer capability
       :path "crates/feed/"
       :downstream (domain)
       :state active
       :notes "HTTP client（含 If-Modified-Since / ETag）+ 4 FeedKind parser (RSS/Atom/JSON/RSSHub)。
               输出 FeedEntryMeta。详见 plan/01-feed.md。")

(crate :id extractor
       :label "正文提取策略链"
       :layer capability
       :path "crates/extractor/"
       :downstream (domain)
       :state active
       :notes "HtmlFetcher + ContentStrategy trait（Readability、SummaryFallback）。
               策略链顺序由 [extractor].strategy_order 配置。详见 plan/02-extract.md。")

(crate :id ai
       :label "AI 客户端 + prompt + 解析"
       :layer capability
       :path "crates/ai/"
       :downstream (domain)
       :state active
       :notes "OpenAI 兼容 client（reqwest）。prompt.rs 处理 placeholder + UTF-8 安全截断。
               parser.rs 强制 JSON schema 校验（keep / score / summary）。
               详见 plan/03-ai.md。")

(crate :id report
       :label "Markdown 渲染 + rebuild"
       :layer capability
       :path "crates/report/"
       :downstream (domain storage)
       :state active
       :notes "frontmatter / 摘要 / 模板 placeholder。
               rebuild.rs 读 publish_records snapshot 字段做字节相等重建。
               详见 plan/04-publish.md。")

(crate :id publish
       :label "发布目标：本地 + GitHub"
       :layer capability
       :path "crates/publish/"
       :downstream (domain)
       :state active
       :notes "LocalFsTarget（路径防穿越）+ GithubTarget（含 422 lost-update 重试）。
               classify.rs 把 HTTP 状态映射到 PermanentApiError / RetryableApiError / RateLimit / AuthFailed。
               详见 plan/04-publish.md。")

(crate :id observability
       :label "横向：tracing / metrics / health / events redaction"
       :layer cross-cutting
       :path "crates/observability/"
       :downstream (config storage domain)
       :state active
       :notes "tracing_init：subscriber 单例 + WorkerGuard 生命周期。
               metrics：MetricsRecorder trait + NullMetrics / InMemoryMetrics / PrometheusMetrics。
               redact：URL userinfo / Bearer / JSON 键名匹配（api_key / token / secret / password / access_key）。
               health：HealthCheck trait + doctor 子命令。
               详见 plan/07-observability.md。")
