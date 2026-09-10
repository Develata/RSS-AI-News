;;; architecture-code.lisp — code 视图（"实际如此"）
;;;
;;; 初版节点 / 路径由 codegraph 辅助导出；本轮变更按当前 Rust 源文件与 Cargo.toml 核对。
;;; 与 architecture-plan.lisp 节点 :id 对称：plan-side 是语义抽象，code-side
;;; 是当前实现位置。两边出现 :id 不一致即漂移，登记在 architecture-diff.md。
;;;
;;; 维护方式：在 codegraph 协助下半自动撰写——
;;;   mcp__codegraph__codegraph_search <name>     反查符号
;;;   mcp__codegraph__codegraph_callers <symbol>  反查调用方
;;;   mcp__codegraph__codegraph_callees <symbol>  正查被调
;;; 文件 + 行号对应索引时刻；index 由 file watcher 异步刷新，
;;; 修改后 ~500ms 内反映。

;; ====================================================================
;; CLI 接缝
;; ====================================================================

(node :id cli-main
      :label "rss-ai-news 二进制入口"
      :layer interaction-shell
      :crate rss-ai-news
      :path "src/main.rs:4"
      :kind function
      :downstream (cli-run)
      :state active
      :notes "main() 一行：rss_ai_news_cli::run().await.into_process_exit()。")

(node :id cli-run
      :label "cli::run 主入口"
      :layer instruction-interface
      :crate cli
      :path "crates/cli/src/lib.rs"
      :kind function
      :downstream (config-loader cli-commands)
      :state active
      :notes "解析 clap Cli + 全局 flag + 子命令分派；持有 WorkerGuard 到进程结束。")

(node :id cli-commands
      :label "13 个子命令模块"
      :layer instruction-interface
      :crate cli
      :path "crates/cli/src/commands/"
      :kind module
      :state active
      :notes "ai_run / backfill / doctor / ingest / migrate / publish / publish_all /
              rebuild_report / reindex / replay / run / validate_config / recent_entries
              （13 个 command .rs，加 mod.rs 共 14 个文件）。
              对应 acceptance-cases/commands/*.md。")

;; ====================================================================
;; 运行时
;; ====================================================================

(node :id run-meta
      :label "RunMeta"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :downstream ()
      :state active
      :notes "仅 run_id 与 started_at；不持有配置、clients 或 repositories。")

(node :id ingest-deps
      :label "IngestDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-ingest)
      :downstream (run-meta feed-crate repo-feed-source repo-feed-entry repo-raw-artifact repo-run-event repo-rule-version)
      :state active
      :notes "Feed 抓取所需配置、fetcher 与 repositories；无 HTML、AI 或 publisher。")

(node :id extract-deps
      :label "ExtractDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-extract)
      :downstream (run-meta extractor-fetcher extractor-strategy repo-feed-entry repo-article repo-raw-artifact repo-run-event)
      :state active
      :notes "HTML 抓取、提取策略与 persistence；无 AI 或 publisher。")

(node :id ai-deps
      :label "AiDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-ai-run)
      :downstream (run-meta ai-crate repo-article repo-article-ai-result repo-raw-artifact repo-run-event)
      :state active
      :notes "单次运行的 AI client 与 article/result/artifact/event repositories。")

(node :id publish-deps
      :label "PublishDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-publish)
      :downstream (run-meta publish-crate repo-publish-record repo-publish-item repo-run-event)
      :state active
      :notes "冻结快照发布依赖；remote target 为 Option，local-only 不构造 GitHub client。")

(node :id backfill-deps
      :label "BackfillDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-backfill)
      :downstream (run-meta repo-feed-entry repo-article repo-article-ai-result repo-rule-version repo-run-event)
      :state active
      :notes "只依赖重置/派生任务所需 repositories，不构造 AI 或网络 client。")

(node :id reindex-deps
      :label "ReindexDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-reindex)
      :downstream (run-meta repo-feed-source repo-feed-entry repo-article repo-rule-version repo-reindex-job repo-run-event)
      :state active
      :notes "租约配置与规则/对象 repositories，不持有网络 client。")

(node :id rebuild-report-deps
      :label "RebuildReportDeps"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/context.rs"
      :kind struct
      :upstream (flow-rebuild-report)
      :downstream (repo-publish-record repo-publish-item)
      :state active
      :notes "模板与冻结快照读取依赖，不持有 publisher。")

(node :id flow-ingest
      :label "IngestFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/ingest.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (ingest-deps feed-crate repo-feed-source repo-feed-entry artifact-writer
                   run-event-emitter)
      :state active
      :notes "run() 按 source 执行；仅持有 IngestDeps。")

(node :id flow-extract
      :label "ExtractFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/extract.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (extract-deps extractor-crate repo-feed-entry repo-article artifact-writer
                   run-event-emitter)
      :state active
      :notes "run() 批次领取，失败时懒读取摘要；仅保存计数与最多 32 个失败样例。")

(node :id flow-ai-run
      :label "AiRunFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/ai_run/mod.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (ai-deps ai-crate repo-article repo-article-ai-result artifact-writer
                   run-event-emitter)
      :state active
      :notes "mod 编排 + dto / process / release；Arc<AiRunOptions> 与 Arc<str> 共享不可变配置，最多 32 个失败样例。")

(node :id flow-publish
      :label "PublishFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/publish/mod.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (publish-deps report-crate publish-crate repo-publish-record repo-publish-item
                   run-event-emitter)
      :state active
      :notes "5 阶段：init(mod) / freeze / render / store_local / publish_remote(remote)，
              按阶段拆 freeze.rs / render.rs / store_local.rs / remote.rs + dto.rs。
              远端报告仅加载一次冻结 items，同时派生 article IDs，不重复读取完整快照。")

(node :id flow-reindex
      :label "ReindexFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/reindex/mod.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (reindex-deps repo-reindex-job repo-rule-version repo-feed-entry repo-article repo-feed-source)
      :state active
      :notes "mod 编排 + 拆出 dto / dry_run / execute / abort。
              三 target × dry-run/real-run × abort 分支。")

(node :id flow-backfill
      :label "BackfillFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/backfill.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (backfill-deps repo-feed-entry repo-article-ai-result repo-rule-version)
      :state active
      :notes "extract / ai 两个方法。详见 plan/10-replay-and-backfill.md。")

(node :id flow-rebuild-report
      :label "RebuildReportFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/rebuild_report.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (rebuild-report-deps report-crate repo-publish-record repo-publish-item)
      :state active
      :notes "字节相等重建保证由 report::rebuild 实现。")

(node :id flow-recent-entries
      :label "RecentEntriesFlow"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/flows/recent_entries.rs"
      :kind struct
      :upstream (cli-commands)
      :downstream (repo-feed-source repo-feed-entry)
      :state active
      :notes "只读 projection；独立持有最小 read-only repository traits，不构造网络 clients、publisher 或 writer repositories。")

(node :id runtime-error
      :label "RuntimeError enum"
      :layer cross-cutting
      :crate runtime
      :path "crates/runtime/src/error.rs:11"
      :kind enum
      :upstream (cli-run flow-ingest flow-extract flow-ai-run flow-publish flow-reindex)
      :state active
      :notes "is_retryable() at line 44。三层错误：能力错误 → RuntimeError → AppError。")

(node :id artifact-writer
      :label "ArtifactWriter"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/artifact.rs:27"
      :kind struct
      :upstream (flow-ingest flow-extract flow-ai-run)
      :downstream (repo-raw-artifact)
      :state active
      :notes "should_write() 按 retention_policy + on_failure 决策；
              write_inline() 当前只写数据库；inline_threshold_bytes/file_storage_dir 为保留配置。")

(node :id run-event-emitter
      :label "RunEventEmitter"
      :layer cross-cutting
      :crate runtime
      :path "crates/runtime/src/events.rs:20"
      :kind struct
      :upstream (flow-ingest flow-extract flow-ai-run flow-publish flow-backfill flow-reindex)
      :downstream (repo-run-event redact-event-context)
      :state active
      :notes "emit() 脱敏 message 与 context，context 最终序列化 JSON ≤4096 bytes；
              insert 失败仅 tracing::error!，不向上抛错（'禁止静默吞错' 的唯一豁免点）。")

;; ====================================================================
;; 能力层
;; ====================================================================

(node :id feed-crate
      :label "feed crate（lib）"
      :layer capability
      :crate feed
      :path "crates/feed/src/lib.rs"
      :kind module
      :state active
      :notes "parse_feed(bytes, FeedKind) + FeedClient。")

(node :id extractor-crate
      :label "extractor crate"
      :layer capability
      :crate extractor
      :path "crates/extractor/src/"
      :kind module
      :downstream (extractor-strategy extractor-fetcher)
      :state active
      :notes "fetcher.rs（HtmlFetcher）+ strategy.rs（ContentStrategy + Readability + summary_fallback）。")

(node :id extractor-strategy
      :label "ContentStrategy trait"
      :layer capability
      :crate extractor
      :path "crates/extractor/src/strategy.rs:11"
      :kind trait
      :state active
      :notes "公开导出：ContentStrategy / ReadabilityStrategy / summary_fallback。")

(node :id extractor-fetcher
      :label "HtmlFetcher / ReqwestHtmlFetcher"
      :layer capability
      :crate extractor
      :path "crates/extractor/src/fetcher.rs"
      :kind trait
      :state active)

(node :id ai-crate
      :label "ai crate"
      :layer capability
      :crate ai
      :path "crates/ai/src/lib.rs"
      :kind module
      :state active
      :notes "AiClient（reqwest）+ prompt::render（UTF-8 安全截断）+ parse_response / ParsedResponse。")

(node :id storage-pool
      :label "StoragePool enum"
      :layer capability
      :crate storage
      :path "crates/storage/src/pool.rs"
      :kind enum
      :upstream (cli-commands)
      :downstream (repo-feed-source repo-feed-entry repo-article repo-article-ai-result
                   repo-publish-record repo-publish-item repo-raw-artifact
                   repo-run-event repo-rule-version repo-reindex-job)
      :state active
      :notes "build / build_read_only 按数据库 scheme 分派；未知 URL scheme 明确拒绝。
              Debug 不暴露连接 URL 或 credentials。")

(node :id repo-feed-source
      :label "FeedSourceRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/feed_source.rs"
      :kind trait
      :state active)

(node :id repo-feed-entry
      :label "FeedEntryRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/feed_entry.rs"
      :kind trait
      :state active)

(node :id repo-article
      :label "ArticleRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/article.rs"
      :kind trait
      :state active)

(node :id repo-article-ai-result
      :label "ArticleAiResultRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/article_ai_result.rs"
      :kind trait
      :state active)

(node :id repo-publish-record
      :label "PublishRecordRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/publish_record.rs"
      :kind trait
      :state active
      :notes "内部按双方言派发，阶段 claim/release 与终态推进保持原子性。")

(node :id repo-publish-item
      :label "PublishItemRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/publish_item.rs"
      :kind trait
      :state active)

(node :id repo-raw-artifact
      :label "RawArtifactRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/raw_artifact.rs"
      :kind trait
      :state active)

(node :id repo-run-event
      :label "RunEventRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/run_event.rs"
      :kind trait
      :state active)

(node :id repo-rule-version
      :label "RuleVersionRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/rule_version.rs"
      :kind trait
      :state active)

(node :id repo-reindex-job
      :label "ReindexJobRepository"
      :layer capability
      :crate storage
      :path "crates/storage/src/repo/reindex_job.rs"
      :kind trait
      :state active)

(node :id report-crate
      :label "report crate"
      :layer capability
      :crate report
      :path "crates/report/src/"
      :kind module
      :state active
      :notes "render / frontmatter / rebuild 三个职责，对应同名模块。")

(node :id publish-crate
      :label "publish crate"
      :layer capability
      :crate publish
      :path "crates/publish/src/"
      :kind module
      :state active
      :notes "LocalFsTarget + GithubTarget + classify（HTTP 状态 → ApiError 分类）。")

;; ====================================================================
;; 配置 + 可观测性
;; ====================================================================

(node :id config-loader
      :label "config::load + load_skip_env_checks"
      :layer object
      :crate config
      :path "crates/config/src/loader.rs"
      :kind module
      :upstream (cli-run)
      :downstream (config-validate config-sha256)
      :state active)

(node :id config-validate
      :label "validate::run_*"
      :layer object
      :crate config
      :path "crates/config/src/validate.rs"
      :kind module
      :state active
      :notes "run_general_checks / run_structural_checks / run_env_checks / run_command_checks。")

(node :id config-sha256
      :label "compute_config_sha256"
      :layer object
      :crate config
      :path "crates/config/src/version.rs"
      :kind function
      :state active
      :notes "app.toml + categories/*.toml 内容拼接后 SHA-256；env / CLI 覆盖不参与。")

(node :id observability-stack
      :label "observability crate（lib）"
      :layer cross-cutting
      :crate observability
      :path "crates/observability/src/lib.rs"
      :kind module
      :downstream (tracing-init metrics-recorder health-check redact-event-context)
      :state active
      :notes "不依赖 config/storage/domain 或 SQL/HTTP client；run_events 持久化由 runtime::RunEventEmitter 承担。")

(node :id tracing-init
      :label "tracing_init::init"
      :layer cross-cutting
      :crate observability
      :path "crates/observability/src/tracing_init.rs"
      :kind function
      :state active
      :notes "返回 Option<WorkerGuard>；调用方需持有 guard 到进程结束。")

(node :id metrics-recorder
      :label "MetricsRecorder trait + 3 实现"
      :layer cross-cutting
      :crate observability
      :path "crates/observability/src/metrics.rs"
      :kind trait
      :state active
      :notes "NullMetrics / InMemoryMetrics / PrometheusMetrics 三实现；HTTP exporter 最多 32 个 handlers、5 秒截止时间、4 KiB 请求头。")

(node :id health-check
      :label "HealthCheck trait + CheckReport"
      :layer cross-cutting
      :crate observability
      :path "crates/observability/src/health.rs"
      :kind trait
      :state active
      :notes "仅 CheckOutcome / CheckReport / HealthCheck；具体 checks 位于 runtime::doctor::health。")

(node :id doctor-health
      :label "runtime::doctor::health 具体诊断"
      :layer flow-coord
      :crate runtime
      :path "crates/runtime/src/doctor/health.rs"
      :kind module
      :upstream (cli-commands)
      :downstream (health-check config-validate storage-pool)
      :state active
      :notes "13 项具体 config/DB/migration/HTTP/disk/liveness checks。doctor 不自动 migrate 或 seed；
              migration 校验版本、checksum 和 success；HTTP ping 有时间与 body 上限。")

(node :id redact-event-context
      :label "redact::redact_event_context"
      :layer cross-cutting
      :crate observability
      :path "crates/observability/src/redact.rs"
      :kind function
      :state active
      :notes "URL userinfo 与敏感 query（包括错误文本内 URL）、Authorization header、JSON credential 键。
              无变化字符串保留借用；doctor 输出和 run_events 写入复用同一策略。")

;; ====================================================================
;; 状态机（4 个 enum）
;; ====================================================================

(node :id feed-entry-state
      :label "FeedEntryState enum"
      :layer object
      :crate domain
      :path "crates/domain/src/state.rs"
      :kind enum
      :state active
      :notes "Discovered / DedupSkipped / PendingFetch / Fetching / Extracting /
              Persisted / FallbackPersisted / Failed。")

(node :id article-state
      :label "ArticleState enum"
      :layer object
      :crate domain
      :path "crates/domain/src/state.rs"
      :kind enum
      :state active
      :notes "Persisted / AiPending / AiDone / ReadyForPublish / PublishSkipped / Published / Retired。")

(node :id ai-result-state
      :label "AiResultState enum"
      :layer object
      :crate domain
      :path "crates/domain/src/state.rs"
      :kind enum
      :state active
      :notes "Pending / Running / Succeeded / PermanentFailed / Filtered。")

(node :id publish-state
      :label "PublishState enum"
      :layer object
      :crate domain
      :path "crates/domain/src/state.rs"
      :kind enum
      :state active
      :notes "Pending / SnapshotFrozen / Rendered / StoredLocal / PublishedLocal / PublishedRemote / Failed。")

;; ====================================================================
;; 部署形态（容器与 CI）
;; ====================================================================

(node :id docker-runtime-image
      :label "Dockerfile runtime stage"
      :layer interaction-shell
      :crate rss-ai-news
      :path "docker/Dockerfile"
      :kind module
      :downstream (cli-main)
      :state active
      :notes "builder → runtime；普通 locked Cargo build + BuildKit cache mounts，无 workspace stubs；ENTRYPOINT 为 rss-ai-news。")

(node :id docker-scheduler-image
      :label "Dockerfile scheduler stage + entrypoint"
      :layer interaction-shell
      :crate rss-ai-news
      :path "docker/scheduler-entrypoint.sh"
      :kind module
      :downstream (docker-runtime-image)
      :state active
      :notes "FROM runtime + supercronic；外挂 crontab 文件优先，env 模式兜底。")

(node :id ci-workflow
      :label "GitHub Actions CI"
      :layer interaction-shell
      :crate rss-ai-news
      :path ".github/workflows/ci.yml"
      :kind module
      :state active
      :notes "5 个并行 job：lint / test / migration-smoke / test-pg / docker-build。")

(node :id acceptance-matrix-cli
      :label "Rust acceptance matrix CLI"
      :layer development-tooling
      :crate rss-ai-news-acceptance
      :path "tools/acceptance/src/"
      :kind module
      :downstream ()
      :state active
      :notes "零 product-crate 依赖；通过 Cargo/.ci/product CLI/PostgreSQL/Docker/release identity 外部边界编排 local/full profiles。")

(node :id release-workflow
      :label "GitHub Actions Release"
      :layer interaction-shell
      :crate rss-ai-news
      :path ".github/workflows/release.yml"
      :kind module
      :state active
      :notes "push tag v* 触发；同时 push runtime + scheduler 两套镜像到 GHCR。")
