;;; architecture-plan.lisp — plan 视图（"应当如此"）
;;;
;;; 节点从 docs/plan/ 章节抽取的语义抽象。这是"系统应当是这样"的索引，
;;; 不是代码自动反推。:label / :notes 用自然语言描述每个节点的意图与
;;; 上下游关系；:path 给出 plan 文档锚点（不是代码路径，代码路径见
;;; architecture-code.lisp）。
;;;
;;; 维护频率：每 minor release 一次，与 plan/ 章节同步。
;;; 真实代码路径以 codegraph / architecture-code.lisp 为准。

;; ====================================================================
;; 主链路 5 段（plan/01-04）
;; ====================================================================

(node :id ingest-flow
      :label "Feed 抓取段"
      :layer flow-coord
      :crate runtime
      :path "plan/01-feed.md"
      :kind flow
      :upstream (cli-ingest)
      :downstream (feed-fetch feed-parse dedup-link dedup-uid feed-entry-state)
      :state active
      :notes "按 categories/*.toml 中 [[sources]] 逐 source 抓取；
              一层 link_hash + 二层 uid 去重；bootstrap 写 config rule_version_id。
              5xx 进入重试预算；304 短路。")

(node :id extract-flow
      :label "正文提取段"
      :layer flow-coord
      :crate runtime
      :path "plan/02-extract.md"
      :kind flow
      :upstream (cli-extract ingest-flow)
      :downstream (html-fetcher content-strategy dedup-content article-state artifact-html)
      :state active
      :notes "策略链 Readability → SummaryFallback。
              第三层 content_hash 去重，命中则 DedupSkipped。
              HTML 留档在策略调用之前。")

(node :id ai-run-flow
      :label "AI 分析段"
      :layer flow-coord
      :crate runtime
      :path "plan/03-ai.md"
      :kind flow
      :upstream (cli-ai-run extract-flow)
      :downstream (ai-task-gen ai-client ai-parser article-state ai-result-state artifact-ai)
      :state active
      :notes "task_gen 从 Persisted articles 派生 Pending ai_result；
              process 按 lease 并发；keep × score 决定 article 走 ReadyForPublish / AiDone / PublishSkipped。
              AI-off 直通模式跳过整段。")

(node :id publish-flow
      :label "发布段（5 阶段）"
      :layer flow-coord
      :crate runtime
      :path "plan/04-publish.md"
      :kind flow
      :upstream (cli-publish ai-run-flow)
      :downstream (publish-init publish-freeze publish-render publish-store-local publish-remote publish-state)
      :state active
      :notes "init → freeze → render → store-local → publish-remote。
              snapshot 冻结后保证 rebuild-report 字节相等。
              远端 422 lost-update 重试一次；429 保态可重试。")

;; ====================================================================
;; 横向能力（plan/05-07, 11）
;; ====================================================================

(node :id storage-pool
      :label "StoragePool 双方言"
      :layer capability
      :crate storage
      :path "plan/05-storage.md"
      :kind enum
      :upstream (runtime-context)
      :downstream (repo-feed-source repo-feed-entry repo-article repo-ai-result
                   repo-publish-record repo-publish-item repo-raw-artifact
                   repo-run-event repo-rule-version repo-reindex-job)
      :state active
      :notes "enum StoragePool { Sqlite(SqlitePool), Postgres(PgPool) }。
              repo trait 在两方言下行为对齐；migrations 编号一一对应。")

(node :id config-loader
      :label "三层配置加载"
      :layer object
      :crate config
      :path "plan/06-config.md"
      :kind module
      :upstream (cli-main)
      :downstream (config-validate config-sha256 cli-overrides)
      :state active
      :notes "EnvConfig + AppConfig + CategoryConfig；
              RSSHub 占位符运行时展开；密钥红action。")

(node :id observability-stack
      :label "可观测性三出口"
      :layer cross-cutting
      :crate observability
      :path "plan/07-observability.md"
      :kind module
      :upstream (runtime-context cli-main)
      :downstream (tracing-init metrics-recorder health-check redact-event-context
                   run-event-emitter)
      :state active
      :notes "tracing 日志 + Prometheus metrics + run_events 表三出口共享同一 redaction 过滤器。")

(node :id error-model
      :label "三层错误模型"
      :layer cross-cutting
      :crate runtime
      :path "plan/11-error-and-recovery.md"
      :kind enum
      :upstream (cli-main runtime-flows)
      :downstream (feed-error extractor-error ai-error storage-error publish-error report-error)
      :state active
      :notes "RuntimeError → AppError → exit code。
              ClassifiedError trait + 重试预算 + 三层 enforce '禁止静默吞错'。")

;; ====================================================================
;; 4 状态机（plan/08）
;; ====================================================================

(node :id feed-entry-state
      :label "FeedEntryState"
      :layer object
      :crate domain
      :path "plan/08-state-machines.md"
      :kind state-machine
      :upstream (ingest-flow extract-flow)
      :downstream ()
      :state active
      :notes "Discovered → DedupSkipped | PendingFetch → Fetching → Extracting → Persisted / FallbackPersisted / Failed。
              backfill --target extract 把 Failed/FallbackPersisted 重置回 PendingFetch。")

(node :id article-state
      :label "ArticleState"
      :layer object
      :crate domain
      :path "plan/08-state-machines.md"
      :kind state-machine
      :upstream (extract-flow ai-run-flow publish-flow)
      :downstream ()
      :state active
      :notes "Persisted → AiPending → AiDone | ReadyForPublish | PublishSkipped → Published → Retired。
              AI-off 直通：Persisted 可直接进入 freeze 候选。")

(node :id ai-result-state
      :label "AiResultState"
      :layer object
      :crate domain
      :path "plan/08-state-machines.md"
      :kind state-machine
      :upstream (ai-run-flow)
      :downstream ()
      :state active
      :notes "Pending → Running → Succeeded / PermanentFailed / Filtered。
              5xx release retryable；非 JSON / 越界 score → PermanentFailed。")

(node :id publish-state
      :label "PublishState"
      :layer object
      :crate domain
      :path "plan/08-state-machines.md"
      :kind state-machine
      :upstream (publish-flow)
      :downstream ()
      :state active
      :notes "Pending → SnapshotFrozen → Rendered → StoredLocal → PublishedLocal | PublishedRemote → Failed（任一阶段）。
              422 lost-update 在 StoredLocal 状态下自动重试一次。")

;; ====================================================================
;; 跨能力机制（plan/10）
;; ====================================================================

(node :id replay-command
      :label "replay --kind={feed,html,ai}"
      :layer instruction-interface
      :crate cli
      :path "plan/10-replay-and-backfill.md"
      :kind function
      :upstream (cli-main)
      :downstream (repo-raw-artifact feed-parser content-strategy ai-parser)
      :state active
      :notes "只读重做：读 raw_artifacts → 在内存中重解析 → 输出 diff。
              已知限制：文件后端 artifact 当前不支持。")

(node :id backfill-command
      :label "backfill --target={extract,ai}"
      :layer instruction-interface
      :crate cli
      :path "plan/10-replay-and-backfill.md"
      :kind function
      :upstream (cli-main)
      :downstream (repo-feed-entry repo-article-ai-result repo-rule-version)
      :state active
      :notes "extract：重置窗内 Failed → PendingFetch。
              ai：新建 prompt_version 行，对窗内 article 插入新版本的 Pending ai_result。")

(node :id reindex-command
      :label "reindex --target={link_hash,content_hash,categories,all} | --abort"
      :layer instruction-interface
      :crate cli
      :path "plan/05-storage.md"
      :kind function
      :upstream (cli-main)
      :downstream (repo-reindex-job repo-rule-version repo-feed-entry repo-article repo-feed-source)
      :state active
      :notes "版本化规则升级：原子写 rule_versions(pending) + reindex_jobs(pending)；
              claim → running → checkpoint → 完成时事务内 active 切换。
              partial unique index 保证同 target 只能一个 active job。")

(node :id rebuild-report-command
      :label "rebuild-report --publish-record-id"
      :layer instruction-interface
      :crate cli
      :path "plan/10-replay-and-backfill.md"
      :kind function
      :upstream (cli-main)
      :downstream (repo-publish-record report-render publish-target)
      :state active
      :notes "用当前模板 + 冻结 snapshot 重新渲染。
              snapshot 不变 → 字节相等；模板变 → 字节差异即影响范围。")

(node :id recent-entries-command
      :label "recent-entries read-only projection"
      :layer instruction-interface
      :crate cli
      :path "plan/09-cli-and-runtime.md"
      :kind function
      :upstream (cli-main)
      :downstream (flow-recent-entries repo-feed-source repo-feed-entry)
      :state active
      :notes "按 category/discovered_after 导出有界 entry + source health；published_after 仅在 consumer 显式提供时启用。")

;; ====================================================================
;; CLI surface（plan/09）
;; ====================================================================

(node :id cli-main
      :label "rss-ai-news binary entry"
      :layer interaction-shell
      :crate rss-ai-news
      :path "plan/09-cli-and-runtime.md"
      :kind function
      :upstream ()
      :downstream (cli-ingest cli-extract cli-ai-run cli-publish cli-publish-all
                   cli-run cli-migrate cli-validate-config cli-doctor
                   replay-command backfill-command reindex-command rebuild-report-command
                   recent-entries-command
                   config-loader observability-stack runtime-context)
      :state active
      :notes "main.rs 仅一行：rss_ai_news_cli::run().await.into_process_exit()。")

(node :id runtime-context
      :label "RunContext（接缝点）"
      :layer flow-coord
      :crate runtime
      :path "plan/09-cli-and-runtime.md"
      :kind struct
      :upstream (cli-main)
      :downstream (storage-pool feed-client html-fetcher content-strategy
                   ai-client publish-target-local publish-target-remote
                   run-event-emitter)
      :state active
      :notes "承载 6 capability clients + 10 Repository traits + run 元数据。
              是 CLI 壳与 Flow 编排的唯一接缝。")

;; ====================================================================
;; 部署形态（plan/12）
;; ====================================================================

(node :id docker-runtime-image
      :label "ghcr.io/.../rss-ai-news:<tag>"
      :layer interaction-shell
      :crate rss-ai-news
      :path "plan/12-deployment.md"
      :kind module
      :upstream ()
      :downstream (cli-main)
      :state active
      :notes "Docker multi-stage：deps → builder → runtime。
              ENTRYPOINT 直接是 rss-ai-news 二进制。")

(node :id docker-scheduler-image
      :label "ghcr.io/.../rss-ai-news-scheduler:<tag>"
      :layer interaction-shell
      :crate rss-ai-news
      :path "plan/12-deployment.md"
      :kind module
      :upstream ()
      :downstream (docker-runtime-image)
      :state active
      :notes "scheduler stage FROM runtime + supercronic + entrypoint。
              不内置 cron 的兜底（详见 plan/13-non-goals.md）。")
