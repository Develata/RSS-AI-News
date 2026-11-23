# 2026-11-21 审计修复与两起线上事故（中文路径 400 / AI 任务生成饥饿）

- 日期：2026-11-22
- 作者 / Agent：Claude Code（Opus 5.5，集成）+ 各包 Opus 子代理（实现）+ Codex gpt-6.2-astra（独立审计 / 复审）
- 分支：`audit/2026-11-fixes`（基于 `main` f420d95）
- 当前 HEAD：见 `git log -1`
- 相关 commit：`f420d95..HEAD`，每个非合并提交以 `[ID]` 标注对应发现（A*/B*/C*/D*、R1-*/R2-*/R3-*、R3E/R3F/R3G-*）
- 相关 tag / release：N/A（未推送、未发布、未部署）
- 状态：`validated`（仅本地）

## 工作摘要

对 v0.10.4 做全仓审计（Claude 初审 + Codex 盲审 + 对抗核查），按用户批准的设计修复全部经核实的发现，
并修复 2026-11-21 的两起线上事故：单报告 GitHub Contents API 对中文路径返回 400（路径未逐段百分号编码），
以及四个分类在 08:30 发布前没有任何 AI 任务（task_gen / AI claim 按最小 id 服务、无视发布窗口）。
修复后又经三轮 Codex 复审；每轮都还有新发现，**没有一轮给出"通过"**。最后一轮（H1–H3，`26b2282..88d544f`）
只做了一次只出报告的审查，其 2 个 BLOCKER、4 个 SHOULD_FIX 未修，记入下方遗留风险。

## 影响范围

- crate / 模块：extractor、feed、storage（全部 repo）、runtime（extract / ingest / ai_run / backfill / reindex /
  decision_run / publish / replay / rebuild_report / doctor）、publish、report、config、cli（ingest / ai_run / publish /
  publish_all / rebuild_report / reindex / replay / run）、tools/acceptance、.github、docker
- 真相源对象：FeedSource、FeedEntry、Article、ArticleAiResult、PublishRecord、PublishItem、RuleVersion、ReindexJob、DecisionResult
- 额外影响：
  - migration `0008_publish_stage_budget_and_item_article_index`（`stage_attempt_count`、`next_attempt_at`、
    `idx_publish_items_article`）、`0009_ai_share_state_and_article_origin_index`（`ai_fresh_share_state`、
    `idx_articles_origin_feed_entry`），两方言 up/down，已 pin
  - 新 ADR 0012（按阶段发布预算 + 快照归属 + 歧义写入核对）
  - CLI 行为：默认报告日期按 target_timezone；`run` 在 ingest 失败后继续 AI / publish；extract 批次超预算时缩批；
    新的 summary 字段与退出码（extract / ai 自身存储失败 → 1）
  - 新依赖：`encoding_rs`、`jiff`（tzdb bundle）、`unicode-normalization`、`httpdate`、`sha1`（均为小型、多数已在锁文件中）
  - CI：发布前强制同 SHA 的 PG lane 与 cargo audit；按版本推送后"对齐到最高已发布版本"的别名晋升；action SHA pin；
    dependabot github-actions / docker；权限与超时；工具链与部署版本一致性检查；scheduler 真实执行 smoke

## 关键变更

### 用户批准的契约变更（constitution §7.1，2026-11-21）

- 发布重试预算按阶段计（`stage_attempt_count`），`attempt_count` 保留为累计审计
- 选稿排除已在其他非 failed 快照中的文章；freeze 事务内复查；failed 释放归属
- 默认报告日期按有效 target_timezone
- `run` 在 ingest 失败后仍在数据库可用时跑 AI 与 publish，退出码反映 ingest 失败

### 线上事故

- 事故 1：Contents GET/PUT 与 Git ref 路由逐段编码，`ref` 作为编码后的查询参数；失败保留操作、状态、固定词表原因与
  GitHub request id；发布客户端禁用透明重试；歧义写入先只读核对 HEAD，核对未完成则保留阶段与归属、退款预算
- 事故 2：task_gen 与 AI claim 给发布窗内文章有界份额（窗口内先到期者优先、article id 打平），旧积压保留非零份额；
  batch=1 时按 `ai_fresh_share_state` 跨 run 轮换；窗口内 C3 对账计入 fresh 份额

### 其余（按领域）

- 抽取：CJK 按 floor(字数/2) 计词；按 BOM / Content-Type / meta 预扫描解码（不猜未声明编码）；存储瞬时错误走预算重试；
  article 写入与 entry 推进同一事务；429 可重试并遵守 Retry-After；Referer 关闭；Atom alternate 链接；`--category` 约束 extract
- 存储：SQLite lease / TTL 按时刻比较（共享 helper）；SQLite 扩展忙 / 锁码可重试；PG AI claim `FOR UPDATE OF aar`；
  304 保留 validator；fetch 回写带端点保护
- 发布：本地原子写与权限保留、Unicode 大小写 / 规范化不敏感的路径碰撞；generated_at 固定；force 版本唯一且不 active；
  规则版本登记下沉 runtime；限流等待持久化
- reindex / decision：可续跑（规则指纹）、批内事务与续租、PG 保存点、decision keyset 游标、NUL 安全前缀
- 分层：replay 下沉为只读 `ReplayFlow` + 存储投影

## 验证与验收

### 自动化验证（集成分支最终 HEAD）

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过
- 以下三条在 `88d544f`（全部包合并后）一次通过，无重试：
- `CARGO_PROFILE_RELEASE_STRIP=none cargo acceptance run --lane static --lane workspace --lane sqlite --lane release --expected-version 0.10.4`：通过（38 步）
- `DATABASE_URL=<一次性 PG 16 容器> cargo acceptance run --lane postgres`：通过（8 步，含 storage `--ignored` PG 测试与 CLI migrate run/check）
- `cargo acceptance run --lane docker`：通过（15 步，三镜像构建 + scheduler 真实执行带空格参数的 job）
- 之后的提交只改本 handoff 与 `docs/map/architecture-code.lisp`
- 第四轮合并后（`4416f6c`）：static / workspace / sqlite / release 通过（38 步），docker 通过（15 步）；postgres lane 的
  7 个非 storage 步骤（含真实 PG 16 上的 CLI migrate run/check）通过；其 storage 步骤 3 次因本机端口冲突失败（每次不同的测试
  二进制，均为 `expected to read 1414811696 bytes` / 非 PG 协议应答，127.0.0.1 上有 VS Code / Electron / node / claude 监听
  55xxx–59xxx），直接运行 `cargo test -p rss-ai-news-storage --locked --no-fail-fast -- --ignored --test-threads=1`：95 passed，0 failed

### 手工验收

- release.yml 的晋升、门禁与标签核对：未执行（需真实 tag；脚本已用本地 registry 与假 registry 验证）
- 真实 GitHub / RSSHub / AI 请求：未执行

## 结果

- 全部门禁通过，但**最后一次审查未通过**（见遗留风险前两条）；是否合并由 Develata 审阅后决定。未推送。
- 事故 2 的修复保证新文章有份额，不保证覆盖：默认批大小下每分类每次 ai-run 只生成约 10 个窗口内任务；
  积压量远大于此时，不调整调度频率或 task_gen 容量，08:30 前无 AI 任务的现象仍可能复现。
- 首次真实发布前先在 tag 上观察新 release.yml。

### 升级影响（对现有部署）

- 未知时区（含 `Etc/Unknown`）→ 配置错误退出 78；RSSHub 展开结果必须是 http(s) 且带 host
- lease 连一整波 extract 都放不下 → 跳过 extract、退出 1；extract / ai-run 自身存储失败 → 退出 1
- 暂停 / 归档源的待抓取条目不再被 extract 领取
- 第一次写命令自动应用迁移 0008、0009；版本号仍是 0.10.4
- 部署前建议先用新二进制对生产配置跑一次 `validate-config`

## 风险与后续事项

第四轮（R4，2026-11-22，用户要求"先修再落地"）修了上一轮审查的 2 BLOCKER + 4 SHOULD_FIX + 2 NICE（提交带 `[R4-N]`）。
随后一次只出报告的 Codex 复核：3 项 CLOSED、5 项 PARTIAL，并在**新加的恢复路径**上报告（未修，未经本会话独立复现）：
- BLOCKER：恢复时 HEAD 已命中、但回执落库遇可重试数据库错误，记录只返回 Failed、留下 lease；lease 到期后若阶段预算已满会被
  清扫为 `failed` 并释放归属。修复方向：该分支走 owner 守卫的 deferred release，保留恢复标记。
- BLOCKER：歧义写入后、恢复前修改了 `github_path_prefix`，恢复会核对新路径的 404 并再写一份。修复方向：记录歧义写入当时的
  最终路径与 blob SHA，恢复按原值核对（目标身份目前不含 path_prefix，B4 已知限制）。
- SHOULD_FIX：批内一条恢复记录被限流后，同批后续请求仍继续发；ADR 人工核对可能拿到被同路径覆盖的本地文件、且首次 SELECT 未
  排除仍有有效 lease 的行；registry 适配器在无凭据时接受任意主机的匿名 404（工作流本身总传凭据）；fresh 游标用 0 表示"从头"，
  显式 id=0 的导入数据可卡住 batch=1。
- NICE：全批均经恢复命中时批级 `commit_sha` 为 None；docker lane 等于截止时刻仍判成功。

既有：
- 记录若反复以"未核实"或"限流等待"结束，永不进入 failed 并持有文章（ADR 0012 §3/§3a）
- 限流等待上限 1 h、extract Retry-After 上限 1 天：超出的服务器期限会被提前重试
- 旧端点的 200 响应在过期判定前仍可能写入 entries
- 并发 batch=1 worker 的轮换是尽力而为；SQLite 亚毫秒窗口边界
- release 脚本改为以认证的精确 manifest HEAD 判定 tag 缺失（R4-2）；首次真实 GHCR 运行仍需确认 token 交换
- doctor deep scan 仍比较累计 attempt_count；doctor 不读 RSSHub 展开标记
- 刻意未做：未声明编码页的 windows-1252 猜测、extract 排除集合改造、去除 aws-lc JWT、合并 html5ever、release LTO、arm64 镜像
- 遗留在中间提交上的 bisect 风险：部分包的中间提交单独不能通过 clippy / static lane（各包报告已列出）

## 给下一位 Agent 的备注

- 入口：`docs/adr/0012-*.md`、`docs/plan/03-ai.md §5.1/§5.2`、`docs/plan/04-publish.md`、`docs/plan/02-extract.md §5`
- 提交消息里的 `[ID]` 对应会话内的审计 / 处置清单，该清单未入库（含生产细节，仓库公开）；每个提交本身已写明修复内容
- 事故 1 相关提交：c83c1e8、06fa4d0、d7c1e87、4ab654f、8aab7d0、7cfd822、15751b0、9a01405；锁定测试在
  `crates/publish/tests/github_{contents_path,ref_route,write_once,batch_idempotency,classify}_tests.rs`
- 事故 2 相关提交：4ca3347、31b2d3b、79b7919、3ce4ece、9577bba、b48657a、13301b0；锁定测试为
  `crates/runtime/tests/ai_run_tests.rs` 中的 `fresh_*` / `batch_size_*` / `older_backlog_*` 用例
- 两起事故的修复分散在本分支多个提交中，不是两个独立的最小补丁；如需先单独热修，可按上面的提交挑选

## 2026-11-23 追加：R5 —— 第四轮复核遗留的两个 BLOCKER 及两轮复审发现

- 作者 / Agent：Claude Code（Opus 5.5，实现与集成）+ Codex gpt-6.2-astra（独立复审，max，三轮）
- 分支：`fix/r5-recovery-blockers`（基于 `main` `cb5f562`，即 PR #10 合并后）
- 相关 commit：`7b2d931` [R5-1]、`eeaa719` [R5-2]、`4e4fb02` [R5-3..R5-5]、`5c1ab54` [R5-6..R5-9]、`2f36cc9` [R5-10]
  （`cb5f562..HEAD`）
- 相关 tag / release：N/A（未推送、未发布、未部署）
- 状态：`validated`（仅本地）；第三轮独立复审仍判 BLOCKER，按 Develata 决定只修其中的缺陷后停止（未再复审）

### 工作摘要

修上文"风险与后续事项"中第四轮只出报告复核留下的两个 BLOCKER：恢复命中后回执落库失败被 sweep 成 `failed`（R5-1），以及
歧义写入后改 `github_path_prefix` 导致恢复去新路径判缺失、再写一份（R5-2，migration 0010，Develata 按宪法 §7.1 批准）。
第一轮独立复审（`cb5f562..eeaa719`）判两条主场景成立，另给 3 个 BLOCKER → R5-3/4/5（Develata 决定全修；跨批次路径占用按
Develata 批准的"数据库持久检查"实现）。第二轮（`cb5f562..4e4fb02`）又给 4 个 BLOCKER → R5-6/7/8/9（Develata 决定全修）。
第三轮（`cb5f562..5c1ab54`）再给 4 个 BLOCKER；Develata 决定只修其中确属本分支实现缺陷的部分（R5-10），其余记录在案、不再追加轮次。

### 关键变更

- R5-1：远端终态推进（拿到回执之后）返回任何存储错误 → deferred（保态、退还、`remote_write_unverified`），下一次比对补回执。
- R5-2：migration 0010（`unverified_write_path` / `unverified_write_blob_sha`，两方言 up/down，已 pin）；恢复按记录的确切
  路径与 blob 在固定 HEAD 上核对（`PublishTarget::remote_blob` / `find_remote_blob`）。
- R5-3：每个写请求发出前带守卫登记恢复标记与路径 / blob（`record_remote_write`，保留 lease）。
- R5-4：在记录的路径上确认缺失、但本次要写别处 → 不写、保留记录；ADR 给出普通记录的人工解除语句。
- R5-5：另一分类已发布于 / 未决于同一目的地 → 不写、退还、`remote_path_conflict`（单条与批量、跨运行）。
- R5-6：恢复标记不变量（恢复中一律保留；新记录只有 4xx 拒绝或未发出才清空；5xx / 超时保留）；新的
  `release_retryable_failure_keeping_mark`；持久化 kind 为恢复 kind，事件带真实 `error_kind` 与 `recovery_mark_kept`。
- R5-7：登记与占用检查同一事务（SQLite 写先行取写锁；PG `READ COMMITTED` + 目的地 advisory lock）。
- R5-8：占用比较只折叠 ASCII 大小写（SQLite `lower()`、PG `translate()`，非 ASCII 两方言一致）；存储的身份与回执不变。
- R5-9：`WriteGuard` 在 PUT / PATCH 之前、依赖的读取之后按 lease owner 复核（422 重建再问）；批量任一成员闸口租约已失则整批不发。
- R5-10：恢复中的记录本次写入被拒或未发出时写回 claim 时那一对；"已发出"以 `WriteGuard` 放行为准；租约复核改为单条语句；
  0010 之前的恢复行占用其目标的所有路径；事件只在释放落库后报告 `recovery_mark_kept`；并发测试校验败方回滚。
- 另：批量重复 record id 拒绝；freeze SQL 清空记录的写入；ADR 人工查询带新列；登记文案改为"意图已登记"；测试按确切路径与
  blob 匹配、同分类用例写同一路径。
- 文档：ADR 0012 §3（R5-1…R5-9，R5-10 的修正并入其中；残留限制、人工步骤）、plan/04 §4.2 与错误表、plan/05 迁移表、plan/08、plan/11、plan/15、
  acceptance 04。

### 验证与验收（HEAD `2f36cc9`）

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过
- `cargo test --workspace --locked --no-fail-fast`：1457 passed，0 failed（exit 0）
- `cargo test -p rss-ai-news-storage --locked --no-fail-fast -- --ignored --test-threads=1`（testcontainers，真实 PG 16）：
  98 passed，0 failed（exit 0）
- 变异核对：R5-1 用例在修复前失败；R5-2…R5-10 各自的用例在去掉对应机制时失败（含两方言并发用例；均已还原）
- 验收 lanes（每条都带 `CARGO_PROFILE_RELEASE_STRIP=none`，完整日志留存）：static / workspace / sqlite / release 一次运行通过（exit 0）；
  postgres（一次性 PG 16 容器）通过（exit 0）；docker 首次在 scheduler 镜像构建时 `apt-get` 连不上 `deb.debian.org`（网络超时，
  本分支未改 `docker/`）失败，单独重跑通过（exit 0）
- 手工：真实 GitHub 请求未执行；release.yml 未在真实 tag 上执行

### 独立复审

- 第一轮（`cb5f562..eeaa719`）：**BLOCKER**（3 BLOCKER、5 SHOULD_FIX、2 NICE、1 NEEDS_VERIFICATION），已修 3 BLOCKER 与其中 4 项。
- 第二轮（`cb5f562..4e4fb02`）：**BLOCKER**（4 BLOCKER、6 SHOULD_FIX、1 NICE），已修 4 BLOCKER 与其中 4 项 SHOULD_FIX、1 NICE。
- 第三轮（`cb5f562..5c1ab54`）：**BLOCKER**（4 BLOCKER、6 SHOULD_FIX、1 NEEDS_VERIFICATION）。按 Develata 决定修了其中两个实现缺陷
  （429 后保留了新 blob 而非原 blob；未发出也按已发出处理）、逐行租约复核、0010 之前行的占用漏洞、事件标志、并发测试与 ADR 步骤；
  "复核之后、请求到达之前被接手"的窗口无法用 lease 关闭（需目的地围栏令牌），已在 ADR 写明；其余 SHOULD_FIX 记入 ADR 0012。
  修复后未再做独立复审。

### 风险与后续事项（未修，均已写入 ADR 0012 或此处）

- R5-9 复核之后、请求到达 GitHub 之前被接手的窗口（需目的地围栏令牌，GitHub 不提供）。
- 崩溃发生在本阶段最后一次预算时，lease 过期后 sweep 仍转 `failed`（需为耗尽行提供只读核对入口，另行设计）。
- R5-4 暂停的记录需要运维处理（恢复原路径配置，或按 ADR 守卫语句核实后置 `failed`）；暂停期间持有文章归属。
- 回执串无法区分分支与路径的边界（`release` + `2026/x.md` 与 `release/2026` + `x.md`），R5-5 可能误判占用。
- 非 GitHub 目标的默认 trait 行为：`find_published` 的 `Ok(None)` 被当作缺失、默认 `publish_many` 丢逐项回执。
- 已收到的 429 响应体中断时被合成为 503；目标漂移守卫逐字节比较身份（只改 owner / repo 大小写会让记录按漂移失败）；
  公开的 `with_client` 可带入会重发的 Octocrab；含 `.` 段的路径在占用比较中不同键（GitHub 行为未核实）；0010 的 down 未钉哈希。
- 0010 之前的恢复行在恢复结束前挡住其它分类对同一目标的写入（保守规则）。
- 私库权限丢失时 Contents 404 被当作缺失；PG 提交结果未知时回执已落库却报 Failed；R5-1 回执未存 commit sha；
  终态推进收到重复文章 ID 时两方言行为不同（基线已有）。

### 给下一位 Agent 的备注

- 入口：ADR 0012 §3 中 R5-1…R5-9 段落（R5-10 并入 R5-5 / R5-6 / R5-9 段）；`crates/runtime/src/flows/publish/remote.rs`（`check_earlier_write`、
  `destination_changed`、`gate_remote_write`、`mark_after`、`LeaseGuard`）；`crates/storage/src/repo/publish_record_impl.rs`
  （`record_remote_write`）；`crates/publish/src/github.rs`（`publish_with` / `publish_many_with` 中的 guard 位置）。
- 写前闸顺序（单条与批量相同）：恢复比对 → R5-4 → [R5-5 + R5-3 同一事务] → R5-9 复核 → 发送；批量中闸口在逐条处理之后。
