# Boundary & Resource Hardening — 2026-09-10

后续 `review and fix` 的发现与修复见 [review.md](review.md)。本页性能/验证JSON保留审查前的测量快照；后续代码的验证以审查记录为准。

后续状态：上述实现及 review 修复已提交为 `4c99e5b` 并推送 main。
其 [远端 CI](https://github.com/Develata/RSS-AI-News/actions/runs/34492172689) 的五个 jobs 全部成功，
包括 PostgreSQL integration/migration 和 Docker runtime/debug/scheduler smoke，补齐了当时的本地验证缺口。
更晚的局部修复见 [Resource Boundary Follow-up](../../handoffs/2026-09-10-resource-boundary-follow-up.md)；
这次 CI 只证明 `4c99e5b`，不作为后续未提交代码的验证结果。

基准 HEAD：`95ae450fda35416a59325c05decbf3b577873e52`（v0.7.1）。以下为当时的工作区测量快照，记录时尚未 commit、push、发布镜像或更新实际部署。初始工作区干净。持久化 schema 与迁移文件未改动。

## A. 工程结论

主要问题是全量 RunContext 强迫单阶段命令初始化无关能力，domain/observability 边界反向耦合，以及配置深复制、结果累计和缺失网络边界。现在 flow 通过普通窄 struct 声明所需依赖；未启用能力不构造占位客户端，读操作不隐式升级数据库。保留 SQLite/PG 显式实现、原子 claim/dedup、冻结快照与可恢复 lease，没有新 framework、crate 或自制 ORM。

性能改动既包括直接消除复制和查询，也包括更严格的正确性：CLI 真正接入 Readability；doctor 校验精确迁移状态；dry-run 模拟此前未正确处理的 content-hash 碰撞链。后者不能以单纯“更快”概括。

## B. 实际修改

| Priority | Area | Before | After | Why |
|---|---|---|---|---|
| P0 | Flow依赖 | 全量RunContext/AppConfig/所有clients与repos | 7种flow deps + RunMeta | 类型签名显式缩小可访问能力 |
| P0 | CLI装配 | ingest/read-only等间接依赖AI/publish初始化 | 按命令装配；删除NullAiClient | 单功能独立启用，移除无关凭据要求 |
| P0 | Domain | SQLx traits/derives，DB编码进入业务类型 | 纯Score invariant与state；storage显式转换 | 业务层不依赖数据库实现 |
| P0 | Observability | 具体health依赖config/storage/SQLx/HTTP | 具体检查迁入runtime::doctor | 横向库只留日志/metrics/脱敏/通用结果 |
| P1 | Ingest配置/并发 | 每个source克隆category.sources，全部spawn后等permit | 引用迭代、limit先截、滚动JoinSet | O(k²)复制→O(k)遍历，存活任务≤并发数 |
| P1 | Extract摘要 | 每条claim后立即find_by_id | 失败且需要fallback才读取 | 正常路径移除一个SELECT |
| P1 | Extract装配 | CLI strategies恒为空，配置Readability不生效 | 装配策略，校验名称/重复/空列表，执行min_body_chars | 修正正常正文路径 |
| P1 | Run汇总 | 所有成功/失败结果累计Vec，最后重算 | 增量counters + 最多32失败样例 | 不随总处理量增长，提前返回也保留已完成计数 |
| P1 | AI配置/正文 | 每task深clone opts；每fallback复制/截断正文 | Arc options/template；正文原地UTF-8截断并复用 | 减少并发乘数和重复分配 |
| P1 | AI请求 | async-openai仅作死包装 | reqwest唯一请求路径，删dead APIs/deps | 移除无效依赖链 |
| P1 | Feed/HTML响应 | 已有cap，但BytesMut→Vec再复制 | 直接累积Vec后move | 保留上限，少一次完整payload复制 |
| P1 | AI/GitHub响应 | response.text/body.collect无上限 | 4MiB/16MiB上限，成功超限不可重试 | 控制异常服务响应内存，保留429/5xx分类 |
| P1 | 远端发布 | 无整体deadline，默认transport timeout为空 | 5/30/30秒transport；lease80%整体deadline | 超时可重试释放，留出收尾时间 |
| P1 | Metrics | 每TCP连接无界spawn，无deadline，分片误404 | 32 handlers、5秒、4KiB header、取消JoinSet | 连接和缓冲资源可控，修协议边界 |
| P1 | Doctor | 配置检查占位；最大migration版本掩盖漂移 | 真实检查；精确版本/checksum/success校验 | 诊断真实状态，不自动迁移/seed |
| P1 | Secrets | URL query/userinfo、provider echo、dotenv错误和Debug出口泄漏 | 源头去URL/掩码 + CLI/doctor最终出口脱敏 | 覆盖失败路径与pretty/JSON |
| P1 | DB URL | 未知scheme被当SQLite路径 | 明确接受列表；未知scheme拒绝且不回显URL | 避免误建文件与凭据泄漏 |
| P1 | Read-only维护 | replay/rebuild/reindex dry-run可能迁移/轮换/写事件 | 只读pool与精确迁移检查；dry-run无DB事件 | 读命令不推进持久状态 |
| P1 | Reindex模拟 | content-hash逐行查原DB，碰撞链预测错误 | occupied hash顺序模拟，100k行/1024行页上限 | 静态数据下与real run一致，超限明确失败 |
| P2 | AI taskgen | 读取title/body/entry-id再丢弃 | article_id单列 | 避免无用DB传输和row分配 |
| P2 | Remote报告 | record+items重建后再读items；整批Markdown.clone | 复用claim元信息、一次frozen load；move报告 | 额外SELECT 3→1，保持冻结语义 |
| P2 | Report/GitHub JSON | collect(Vec).join与Value再复制大内容 | 逐项append；Value::Array/String接管所有权 | 简化临时对象生命周期 |
| P2 | Docker | 每crate manifest/stub、build\|\|true、删fingerprint/touch | 普通locked Cargo build + cache mounts | 新crate无需维护stub列表，构建错误不吞 |
| P2 | Scheduler | Supercronic0.2.30/SHA-1；打印crontab | 0.2.49/官方asset SHA-256；不打印完整命令 | 供应链校验与启动日志秘密保护 |
| P2 | Supply chain | 无定期通用扫描/更新配置 | 每周audit、每月分组compatible更新 | 控制维护频率，不自动升级major |
| P2 | 可测量性 | 无统一轻量perf数据 | Rust ignored fixtures + Python stdlib runner | 本地可复现，CI手动上传，不设噪声时间gate |
| P3 | 文档/测试 | 地图、字段和backup定位漂移，环境测试依赖宿主proxy | 更新plan/maps/ADR/验收；隔离env fixture | 文档与当前行为一致，测试可重复 |

## C. 架构

```text
before:
Flow -> RunContext -> AppConfig + all clients + all repositories
Domain -> SQLx
Observability -> Config + Storage + SQLx + HTTP

after:
CLI composition root -> selected flow deps -> needed capabilities/repositories
IngestFlow -> IngestDeps              (no AI / publisher)
ExtractFlow -> ExtractDeps            (no publisher)
AiRunFlow -> AiDeps
PublishFlow -> PublishDeps
RebuildReportFlow -> RebuildReportDeps (no external client)
BackfillFlow / ReindexFlow -> their own deps
Domain -> pure business types/logic
Runtime::doctor -> concrete health implementations
Observability -> logging/metrics/redaction/generic health types
```

公开Rust API发生源代码级调整：RunContext/RunContextDeps删除，AiTask/AiRunOptions模板改Arc<str>，summary改failure_samples；workspace调用点与测试已迁移。TOML格式与数据库schema未变。

## D. 性能与资源

具体数值见本目录JSON与下表。计时使用同机器、同工具链、release fixture，每场景独立进程三次取中位数；不访问互联网/真实AI。peak RSS包括fixture建立。旧binary在改生产代码前保存；最终空闲条件下顺序复测新旧binary，记录SHA-256。不同构建的时间不能用于宣称编译加速。

| 场景 | 规模 | before ms | after ms | before peak KiB | after peak KiB |
|---|---:|---:|---:|---:|---:|
| ingest_sources | 1000 | 664.520 | 434.105 | 199336 | 11976 |
| feed_parse | 100000 | 294.142 | 308.810 | 6488 | 6648 |
| feed_ingest_10 | 10 | 3.943 | 3.812 | 14132 | 14132 |
| feed_ingest_100 | 100 | 14.793 | 13.544 | 14132 | 14132 |
| feed_ingest_1000 | 1000 | 137.927 | 138.538 | 14132 | 14132 |
| feed_dedup_10 | 10 | 1.992 | 2.140 | 14132 | 14132 |
| feed_dedup_100 | 100 | 9.016 | 10.613 | 14132 | 14132 |
| feed_dedup_1000 | 1000 | 91.444 | 92.247 | 14132 | 14132 |
| extract_fixture | 1000 | 878.001 | 855.126 | 8960 | 8948 |
| report_render | 30000 | 129.675 | 129.364 | 6160 | 6064 |
| ai_orchestration | 100 | 105.565 | 107.900 | 12308 | 12480 |

原始样本、工具链和binary摘要见 [perf-before.json](perf-before.json) 与 [perf-after.json](perf-after.json)。feed_parse规模为累计解析条目，report_render为30项报告的1000轮累计条目。feed_ingest/dedup的不同规模共用测试进程，因此RSS相同，不能据此推断单个规模的内存成本。

1000个源配置场景：耗时下降34.7%，peak RSS下降94.0%，与消除category深复制的结构性变化一致。其他场景不支持普遍加速的结论：report_render基本持平，feed_parse约+5%，100条dedup约+17.7%；小样本与进程调度/SQLite时延有波动，三次采样不能证明统计显著性。对此保留原始结果，不选择性报告较快的一轮。

release binary：32,138,592 → 32,293,568 bytes（+0.48%）；没有体积缩小。直接依赖/锁包减少不等于链接后二进制缩小，本轮增加了边界检查和错误处理。没有进行同条件cold build对照，因此不声称编译更快。

7条SQLite计划见 [query-plans.json](query-plans.json)：fetch/AI/publish claim使用现有state/lease索引，recent使用category与source/discovered索引，artifact按唯一键定位；若干排序仍需临时B-tree。AI候选查询仍有窗口排序；fixture不足以支持新增覆盖索引或改变事务。本轮未修改索引或SQL schema。

结构性证明：

- k个source的category深复制：O(k²)→引用迭代O(k)；task buffer从O(k)→O(concurrent_feeds)。per_source结果仍与配置源数成正比。
- extract/AI历史summary：O(total processed)→O(32)样例与固定计数；整体峰值仍包括O(batch_size)的claim、正文和AI响应。
- fallback专用SELECT：每条1次→正常路径0次；n条、fallback比例p时，额外读取n→约pn。未声称整个extract事务只有一次查询。
- remote准备每record额外SELECT：3→1；frozen items完整读取2→1。此为调用图事实，未单独测量DB延迟收益。
- content-hash dry-run：静态n行、m条hash变化、页b时，读取由ceil(n/b)+1+2m→2*(ceil(n/b)+1)。m=0时新版多一次窄扫描，这是准确顺序模拟的代价。
- body并发峰值仍乘以配置并发数；上限不等于常态RSS，更不意味着RSS与任意配置值无关。AI4MiB、GitHub16MiB、health64KiB，Feed/HTML使用配置cap。

没有实现batch insert：SQLite已得到10/100/1000条数据，当前环境无法获取PG对照。未凭单一数据库结果增加事务/冲突处理复杂度。

## E. 依赖与安全

根cargo tree唯一(name,version)节点338→319；Cargo.lock包506→488。删除async-openai及其backoff、instant、reqwest-eventsource、derive_builder等链。完整清单见 [dependency-impact.json](dependency-impact.json)。

直接依赖：domain7→5、observability18→11、AI10→7、feed10→6、extractor12→8、CLI22→20。根binary7→2（仅CLI/Tokio）。domain的serde_json移到dev；Feed/HTML的Tokio仍在dev，不能误称整个执行图没有Tokio。

Publish10→13：新增bytes/http/http-body-util，全部此前已在锁树中。其价值是用标准Limited实现16MiB响应上限、直接Bytes解析；没有引入新包或自制HTTP层。

定向修复h2 0.4.13→0.4.16与event-listener5.4.1→5.4.2，依据[RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258.html)和[RUSTSEC-2026-0221](https://rustsec.org/advisories/RUSTSEC-2026-0221.html)。不升级SQLx/reqwest/HTML stack主版本，不为DRY合并双方言。

RustSec剩余：锁文件inactive rsa的RUSTSEC-2023-0071；现有feature gate确认无执行路径，定期audit仅显式忽略该条并先执行gate。fxhash维护状态警告、spin0.9.8 yanked（SQLx-SQLite→flume的active传递依赖）、chacha20 0.10.0 yanked（当前target无active路径）保留记录，不将其伪装为“零告警”。scraper/readability的HTML解析栈和rand/getrandom等多版本仍存在；无证据证明可安全靠强制版本合并消除。

显式 workspace feature 选择未作全面调整：保留 SQLx 的 SQLite/PG、reqwest Rustls、Octocrab AWS-LC JWT backend。移除的是无效直接依赖及其不再需要的传递链。runtime 新增的 reqwest/fs2 与 dev wiremock 是具体 health 实现迁移后的归属调整，不是新的执行能力。

[duplicate-versions.json](duplicate-versions.json) 记录根默认执行图21组多版本包，包括 scraper 0.18.1/0.22.0、html5ever 0.26.0/0.29.1、getrandom 0.2.17/0.3.4/0.4.2、rand 0.8.7/0.9.4、syn 1/2、thiserror 1/2。`cargo tree -d` 也会列出同版本不同feature构建实例，不能全部解释为可删除的不同版本。此次保留有真实上游约束的重复。

安全扫描状态详见 [security.json](security.json)。Supercronic版本来自[官方v0.2.49 release](https://github.com/aptible/supercronic/releases/tag/v0.2.49)，两个架构SHA-256及原生启动结果见 [scheduler-native.json](scheduler-native.json)。Docker缓存限制依据[官方GHA cache文档](https://docs.docker.com/build/ci/github-actions/cache/)。

## F. 验证

最终验证记录见 [validation.json](validation.json)、[acceptance.json](acceptance.json) 和下表。所有P0边界变更均通过workspace编译与受影响测试；新增边界测试覆盖overflow/坏分数、URL scheme、fallback、summary cap、UTF-8、超大/chunked响应、timeout/lease释放、metrics分片/取消、秘密出口、迁移漂移、dry-run无写及碰撞链。

| 状态 | 命令/验收 | 实际结果 |
|---|---|---|
| PASS | `cargo fmt --all -- --check` | 无格式差异 |
| PASS | `cargo clippy --workspace --all-targets --all-features --jobs 1 -- -D warnings` | 无警告 |
| PASS | `cargo test --workspace --all-features --no-fail-fast -j3` | 816通过、0失败、62 ignored；另有1个隔离子进程重复执行，因此原始日志passed合计817 |
| PASS | `cargo build --workspace --all-features --locked -j3` | 所有workspace/feature编译 |
| PASS | `cargo build --release --locked -j3` | release binary生成 |
| PASS | acceptance `--lane sqlite --lane release --expected-version 0.7.1` | SQLite migration/query smoke、CLI帮助/版本、版本一致性 |
| PASS | `.ci/check_swallowed_errors.sh` | 14条既有allowlist命中；只校准移动后的行号 |
| PASS | `.ci/check_dependency_security_policy.sh` | rsa无active path |
| PASS | `cargo audit --ignore RUSTSEC-2023-0071` | 与既有inactive-rsa策略一致；维护/yanked告警另列 |
| FAIL（有明确归因） | 未忽略条目的 `cargo audit --json` | 仅inactive rsa的RUSTSEC-2023-0071；不声称完整扫描零发现 |
| PASS | 6个release perf fixture，各3样本、独立进程 | 手动执行工作区默认ignored的6个perf测试 |
| PASS | SQLite `EXPLAIN QUERY PLAN` | 7条生产SQL、10k行fixture，完整计划见JSON |
| PASS | Python脚本语法、scheduler `sh -n`、`git diff --check` | 通过；查询脚本整理前后输出逐字相同 |
| PASS | Supercronic原生amd64 smoke | 合法/非法cron、mounted/generated入口启动、SIGTERM退出；两个架构digest验证 |
| NOT RUN | PostgreSQL integration/migration/perf/query plan | 无PG服务；相关测试已编译，未执行 |
| NOT RUN | Docker runtime/debug/scheduler镜像与container smoke | Docker不可用 |
| NOT RUN | arm64实际执行 | 只做asset digest/ELF架构验证 |

62项ignored包含本轮6个已另行执行的perf fixture，以及56项原有ignored集成测试；不能将workspace命令的退出0等同于这些集成测试执行成功。CI YAML已按现有工作流结构检查，未在GitHub远端执行。

原始baseline：fmt/clippy/build/release、现有安全与吞错策略、SQLite/release acceptance通过；首次test因宿主HTTP_PROXY覆盖dotenv空值而失败。隔离相关环境后763通过、56 ignored，随后修复测试隔离，未改进程环境优先级。

环境缺口：Linux Docker入口指向不可用的Docker Desktop路径，且没有本地PostgreSQL服务。PG integration/migration/query plan、runtime/debug/scheduler镜像构建及container smoke均NOT RUN。PG测试已编译，现有CI保留真实容器执行；Supercronic仅有原生amd64 smoke，不替代镜像验收。

## G. 剩余事项

| 级别 | 剩余项 | 原因/后续 |
|---|---|---|
| P0 | 本轮检查与已运行测试中未发现未修复P0 | 不外推为未运行平台也已验证 |
| P1 验证缺口 | PG与Docker容器验收 | 当前环境无服务；发布前运行现有CI矩阵 |
| P2 | spin yanked、fxhash维护状态、inactive锁包告警 | 由上游兼容更新解决，不做大版本强推或维护fork |
| P2 | 配置兼容保留字段未执行 | ai.rate_limit、HTTP retry knobs、dedup开关、artifact文件阈值等已明确记录；后续清理需独立配置迁移 |
| P2 | Readability同步CPU解析 | body与并发已受限；尚无profiler证据要求新worker池/复杂取消架构 |
| P2 | Repository trait仍混合read/write/admin | flow边界已缩小，只读pool强制只读；本轮不增加大量转发trait |
| P2 | reindex dry-run跨页并发写一致性 | 计数等价性前提为静态数据；未新增长快照事务 |
| P2 | 新CI runner的Docker冷构建性能 | cache mounts不默认随GHA exporter持久化，需真实CI测量 |
| P3 | docs-backup历史引用 | 仍有明确引用，保留只读档案；批量删除不在本轮操作范围 |

## 候选问题复核矩阵

| 请求段 | 分类 | 处理结论 |
|---|---|---|
| 3 dependency bag | CONFIRMED | 替换为窄flow deps |
| 4 domain purity | CONFIRMED | 去SQLx，不使用optional feature隐藏耦合 |
| 5 observability | CONFIRMED | 具体health迁至runtime |
| 6 clone/allocation | CONFIRMED | 深复制/JSON/报告/配置优化；Arc等必要cheap clone保留 |
| 7 extract SELECT | CONFIRMED | lazy fallback |
| 8 unlimited summaries | CONFIRMED | counters+32样例 |
| 9 AI options | CONFIRMED | Arc共享 |
| 10 dead dependencies | CONFIRMED | 删除dead wrapper与unused direct依赖 |
| 11 Feed/HTML body | PARTIALLY CONFIRMED | cap已存在；重复copy确实存在并消除 |
| 12 URL scheme | CONFIRMED | 拒绝未知scheme |
| 13 Docker hacks | CONFIRMED | 简化locked build |
| 14 publish duplicate reads | BETTER SOLUTION EXISTS | 一次加载同时渲染/取IDs，比新增二次窄查询更直接 |
| 15 dual-dialect duplication | NOT APPLICABLE | 重复来自真实方言差异，明确保留 |
| 16 batch ingest | PARTIALLY CONFIRMED | 逐项INSERT成立；无PG收益证据，不改原子dedup |
| 17 perf harness | CONFIRMED | 新增轻量可复现测量 |
| 18 async/concurrency | PARTIALLY CONFIRMED | batch已有边界；修ingest任务数、metrics和remote timeout |
| 19 memory | PARTIALLY CONFIRMED | 部分body已有cap；补AI/GitHub/health并减少完整复制 |
| 20 SQL/index | PARTIALLY CONFIRMED | 部分排序用临时B-tree；现有索引服务筛选，没有新增索引依据 |
| 21 errors/redaction | PARTIALLY CONFIRMED | 多数已有日志/gate；修明确秘密泄漏与诊断占位 |
| 22 config | PARTIALLY CONFIRMED | 修能力gate/无效策略；兼容保留项明确文档化 |
| 23 traits | PARTIALLY CONFIRMED | capability边界有价值；未扩大为generic repository体系 |
| 24 crate structure | NOT APPLICABLE | 无新增crate需求，保留现有workspace |
| 25 unsafe | NOT APPLICABLE | Rust源无unsafe，不新增 |
| 26 security | CONFIRMED | 2项定向修复、定期扫描、告警如实保留 |
| 27 scheduler | CONFIRMED | 新版与SHA-256，native smoke已测，容器未测 |
| 28 backup docs | PARTIALLY CONFIRMED | 存在旧引用和不实自包含声明；明确权威，保留有引用档案 |
| 29 tests | CONFIRMED | 增加边界/失败/状态回归，修环境依赖测试 |
| 补查SQLite整数截断 | ALREADY FIXED | SQLx0.8.6已拒绝解码overflow；未加无用CAST |

0–2、30–35为执行/交付约束，按对应阶段和报告验证，不作为待修bug。无push、历史重写、大规模清理或schema迁移。
