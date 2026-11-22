# ADR 0012: 发布按阶段计重试预算 + 文章的快照归属

- 日期：2026-11-21
- 状态：`accepted`（2026-11-21）
- 决策者：Develata（2026-11-21 按宪法 §7.1 批准本组设计：按阶段预算经 migration 0008、跨快照文章排他、
  多报告重试幂等、默认报告日期按目标时区）
- 来源：RSS-AI-News 审计报告 2026-11-21（main `f420d95`），§2 P1 第 5、6、7 项与 §2 表格"多报告重试会重复建 commit"；
  Codex 盲审与对抗核查均判 BLOCKER / SHOULD_FIX，复核后机制全部成立

## Context

发布是阶段驱动状态机（ADR 0002）：`pending → snapshot_frozen → rendered → stored_local → published_*`，
每个阶段各自 claim、各自释放 lease。审计确认了两类跨记录不变量缺失：

1. **重试预算四阶段共用。** 每次 claim `attempt_count + 1` 并过滤 `attempt_count < publish_max_attempts`，成功不清零。
   一份无故障的远端发布本身就要 4 次 claim，因此 `publish_max_attempts ≤ 3`（校验只要求 `> 0`）时远端阶段永远领不到；
   默认 5 时整条链只容忍 1 次失败，而且这 1 次额度被最先失败的阶段用掉。
2. **同一文章可进入两份活动快照。** 选稿只看 `articles.state = 'ready_for_publish'` 和时间窗；文章要到发布终态才升
   `published`。前一份记录卡在 freeze 之后时，下一份 freeze 会再选中同一批文章；两份都可能写出文件或推到 GitHub，
   之后后一份的数据库事务才因文章状态冲突回滚。AI-off 模式下还有 S9：先前快照失败后遗留的 `ready_for_publish`
   文章要求从 `persisted` 晋升，0 行即整批回滚，且 freeze 不释放 lease，每轮重复。
3. **多报告 GitHub 发布重试不幂等。** ref 更新成功但响应丢失时，重试以新 HEAD 为父再建一次提交。

约束：正确性优先于性能（宪法 §2.4）；双方言（ADR 0005）；迁移不可变、需 up + down；不引入新的常驻组件。

## Decision

### 1. 按阶段计预算（migration 0008）

- `publish_records` 增加 `stage_attempt_count`（`NOT NULL DEFAULT 0`）。
- 每次 claim 同时 `attempt_count + 1`、`stage_attempt_count + 1`；claim 过滤、retryable release 折叠
  （`stage_attempt_count >= max` → `failed`）与耗尽 sweep 一律用 `stage_attempt_count` 对比 `retry.publish_max_attempts`。
- 每次阶段推进成功（freeze 写快照、render / store_local / remote 的 advance，含终态 advance）把 `stage_attempt_count` 清零。
- `attempt_count` 保持累计语义（plan/11 §4.1），只作审计。
- 空快照（`SnapshotEmpty`）与 freeze 冲突释放（见 2）两个计数都退还。
- **存量行**：0008 之前的行 `stage_attempt_count = 0`，即当前阶段重新获得完整预算；终态行不受影响（`failed`
  不会因迁移复活）。这是有意的保守选择：旧的累计值无法拆分到阶段，给满预算最多多重试几次，不会把可恢复的行误判为耗尽。
- `publish_max_attempts = N` 的含义变为"每阶段最多 N 次尝试"。

### 2. 文章的快照归属

- **活动快照**：`publish_items` 中出现该文章、且所属 `publish_records.state <> 'failed'` 的记录。
  一篇文章同一时刻至多属于一份活动快照；记录转 `failed` 即释放归属。
- 三条选稿 SQL（AI 路径共用一条；AI-off 的 SQLite / PG 各一条）排除已被活动快照持有的文章。
  0008 同时增加 `idx_publish_items_article` 支撑这一查询。
- freeze 事务内复查（选稿在事务外）：
  1. 首条语句是带 owner/state 守卫的 record 推进（SQLite 由此持有写锁，串行化并发 freeze，避免读后升级写的 busy 失败）；
  2. PostgreSQL 按 id 顺序 `SELECT … FOR UPDATE` 锁住快照内文章——READ COMMITTED 下仅靠 `NOT EXISTS` 复查无法串行化，
     两个 freeze 都会通过检查；加锁后后到者等待，再看见先到者已提交的 items；固定顺序避免死锁。
     该推理依赖 READ COMMITTED（每条语句新快照），因此 freeze 事务的第一条语句显式
     `SET TRANSACTION ISOLATION LEVEL READ COMMITTED`（R1-P-S4）：数据库或角色默认若是 REPEATABLE READ，复查会沿用等锁前的
     快照，两个 freeze 都能提交（PostgreSQL 16 实测）；
  3. 被其它活动快照持有 → 回滚，`ArticleOwnershipConflict`；
  4. AI-off 文章按事务内读到的状态区分：`persisted` 晋升；已是 `ready_for_publish`（未被持有的遗留）保持原状冻结；
     其它状态 → 回滚，`ArticleStateConflict`；
  5. 写 publish_items，提交。
- 冲突时 runtime 立即释放 lease（`release_freeze_conflict`：记录回 `pending`、退还本次 claim、写 `last_error_kind`），
  不发生外部写入；下一次运行重新选稿时自然排除该文章。输掉竞争不是一次发布尝试，因此不消耗阶段预算——
  否则 `publish_max_attempts = 1` 时一次竞争就会让记录 `failed`。
- **存量重复归属的写前拦截（2026-11-22，R1-P-B1）**：0008 之前的库里可能已有两份未失败快照持有同一文章（迁移只建索引，
  不改数据；之后的 freeze 都会事务内复查，不会再产生新的重复）。因此 store_local 与 remote（单条与批量）在 claim 之后、
  任何外部写入（本地文件 / GitHub）之前，查询本记录快照中是否有文章同时属于另一份**胜出**的未失败记录：
  已 `published_*` 的记录无论 id 大小都胜出（其文章已是 `published`），其余情况 id 较小者胜出。
  这是**同一个排序**（先已发布、再按 id 升序，2026-11-22 R3F-P-B1）：它既决定本记录是否落败（有排在自己之前的持有者），
  也决定报告的胜方（排第一的持有者），因此三方及以上共享时所有败方报告的是同一个胜方（例如 9101、9102 与已发布的 9103
  共享文章时，9101 与 9102 都报 9103）。
  败方不写任何外部产物：立即释放 lease、两个计数都退还、**保持当前阶段态**、`last_error_kind =
  article_owned_by_active_snapshot`、`last_error` 列出文章与胜方记录 id，阶段结果为 `ArticleConflict`（exit 1）；
  批量发布中只剔除败方，其余照常提交。胜方转 `failed` 后归属释放，败方下次运行即可继续；胜方已发布时败方无法完成，
  由运维把败方置为 `failed`（冻结快照保留）。

### 3. 多报告重试幂等

建 tree / commit 之前，按刚取到的 HEAD commit sha 查询批内每个路径的 blob sha，与本地按 git 规则计算的 blob sha
（SHA-1 over `blob <len>\0` + 内容）比对。**全部**一致才认定上一次尝试已落地，直接以该 HEAD commit 记回执（每份报告一条，
按输入顺序）；任一缺失 / 不一致照常提交；查询出错按原分类失败，绝不当作"已发布"。保留 422 non-fast-forward 重试。

**同一次尝试内的歧义写入核对（2026-11-22，R1-P-B2）**：写请求（批量的 `PATCH ref`、单报告的 `PUT contents`）已发出、
但结果未知——传输失败 / 响应不可读（分类为 503）或 5xx 应答——时，释放前先做只读核对：`GET ref` 取分支 HEAD commit，
再按该 commit 查每个路径的 blob（`find_published`，与上文同一比对）。全部一致 → 以 HEAD commit 记成功回执；不一致 → 原写入错误不变，按原分类释放。
4xx（含 409、422）表示写入被拒，不核对。runtime 层的远端 deadline（lease 的 80%）
切断调用时同样核对，核对自己的时限为 deadline 之后再加 lease 的 10%（其余 10% 留给数据库收尾），lease 已失时由带 owner
守卫的推进拒绝。
**核对未完成（2026-11-22，R3F-B2）**：核对本身出错（传输失败、5xx、限流）或用完时限时，既不能认定已落地也不能认定未落地，
结果为 `RemoteWriteUnverified`（`error_kind = remote_write_unverified`，可重试）。runtime 以 `release_deferred_retry` 释放：
**保持 `stored_local`**、清 lease、两个计数都退还（与空快照 / 归属冲突同一做法）、写 `last_error*`；核对遇到限流时把该限流的
期限（同样钳制 1 h）写入 `next_attempt_at`，否则置 NULL。因此即使是本阶段最后一次预算，记录也不会转 `failed`、不会释放
文章归属。
发布客户端不做透明重试（R3F-B1），写请求只发一次，首次的歧义结果必然进入本核对。
**下一次 claim 先恢复（2026-11-22，R4-1）**：claim 的投影带回 `last_error_kind`。远端阶段领到
`last_error_kind = remote_write_unverified` 的记录时，在**任何写请求之前**先做同一个只读固定 HEAD 比对
（`find_published`，只针对这一份报告；批量中每条待恢复记录各自单独比对，不与其它记录合并，因为比对是全有或全无的）：
- HEAD 已含该报告 → 以该 HEAD commit 记回执，不发写请求（批量中该记录不进入本批写入）；
- 确认缺失 → 照常写入，**照常计预算**（此后的失败按普通分类释放，歧义写入照常进入本节核对）；
- 比对再次出错 / 被限流 / 用完远端 deadline → `EarlierWriteUnverified`（`error_kind` 仍为 `remote_write_unverified`，
  事件里的 `operation` / `http_status` / `request_id` 取自失败的比对请求）→ `release_deferred_retry`：保持 `stored_local`、
  退还 claim、保留文章归属、保留该 `last_error_kind`（下一次 claim 继续先比对），比对遇限流时写入其期限。
比对给出结论之前，本次尝试的其它可重试失败同样不计预算、保留该 kind：准备阶段超时（`EarlierWriteUnverified`）、
快照读取或归属检查的可重试存储错误（`release_deferred_retry`，`last_error` 写明原因，事件带 `cause_kind`）；
归属冲突的败方释放（本就退还 claim）也写 `remote_write_unverified` 而不是 `article_owned_by_active_snapshot`
（事件 `error_kind` 仍为后者，并带 `earlier_write_unverified: true`）。
**远端已有、回执未落库（2026-11-22，R5-1）**：远端阶段拿到回执（写入成功、歧义核对命中、单报告幂等命中或上一条的恢复比对命中）
之后，带 owner 守卫的终态推进（`stored_local → published_remote` 与文章晋升，同一事务）若返回存储错误，记录**不再**留着 lease
等过期：lease 过期后会被回收，阶段预算已满时被 sweep 转 `failed`、释放文章归属，而这些文章已在远端。改为与上一条相同的
`release_deferred_retry`：保持 `stored_local`、退还本次 claim、`last_error_kind = remote_write_unverified`、`last_error` 以
"github holds the report but its receipt was not recorded" 开头，事件 `error_kind = remote_write_unverified`、`cause_kind` 为存储错误的
kind。下一次 claim 走上一条的恢复比对，命中即记回执、不发写请求。**可重试与不可重试的存储错误一样处理**：远端已有报告时，
`failed` 从来不是正确终态；代价是持续失败的推进每次运行重复一次只读比对与一次推进（与上一条"比对一直不出结论"同一代价）。
推进返回的 `ArticleStateConflict`（文章不在 `ready_for_publish`）不在此列，仍按原样处理。
**恢复按记录的写入核对（2026-11-22，R5-2，migration 0010，Develata 按宪法 §7.1 批准）**：此前恢复比对用当前配置重渲染报告、
重新拼路径；歧义写入之后、恢复之前改了 `github_path_prefix`（目标身份 `owner/repo:branch` 不含前缀）、路径模板或报告模板时，
比对会去新路径找、得到 404 判为"确认缺失"，再写一份。现在：
- `publish_records` 增加 `unverified_write_path`、`unverified_write_blob_sha`（可空）：记录以 `remote_write_unverified` 释放时
  那次写入的**最终路径**（含前缀）与 **git blob id**。`PublishTarget::remote_blob(report)` 给出这一对（GitHub：`join_path` +
  `git_blob_sha`，纯计算；默认 `None`），`PublishTarget::find_remote_blob(blob)` 在固定 HEAD commit 上只读比对**原样**的路径与
  blob（不再按当前前缀拼接），命中即以该 commit 记回执，`remote_target` 指向记录的路径。
- 写入规则（`release_deferred_retry` 的 `UnverifiedWriteUpdate`，每个调用点显式选择）：新的歧义写入核对未完成
  （`RemoteWriteUnverified`）→ 记录本次写入；恢复比对未出结论（`EarlierWriteUnverified`、比对前的可重试失败）与持有恢复标记的
  归属败方 → 保留原记录；R5-1 回执未落库 → 记录该回执对应的 blob；限流 deferral（此时没有未定写入）→ 清空。阶段推进与计预算的
  retryable release 清空（后者只发生在比对已确认缺失或本不在恢复中时）；永久失败保留，供人工核对。只在 `last_error_kind =
  remote_write_unverified` 时读取。
- 恢复比对在记录的路径上确认缺失之后，只有当本次要写的**最终路径与记录的路径相同**时才写入（见下文 R5-4）；该写入之前先按
  R5-3 记录新的一对，替换已确认缺失的旧值。
- 存量（0010 之前已带恢复标记、没有记录）与不记录 blob 的目标：退回按当前渲染比对（`find_published`），行为同 R4-1。
  `find_remote_blob` 的默认实现返回可重试错误，使"有记录但目标查不了"的恢复保持未决，而不是当作缺失。
**代价**：记录的是写入时的路径；恢复命中时记录被标为发布在旧路径上（这正是那次写入的去处），之后的报告才按新配置。

**远端写入的最后几道闸（2026-11-22，R5-3 / R5-4 / R5-5，Develata 批准；无新迁移）**。对 R5-2 的独立复审又给出三个反例：
写入与"记录这次写入"之间崩溃；目的地改变后，先前尚在途中的请求晚到；改前缀后一个分类的新路径正是另一分类的旧路径。
远端阶段（单条与批量）在发出任何写请求之前，按固定顺序过闸：R4-1 恢复比对（只读）→ R5-4 → R5-5 → R5-3 → 发送。
- **R5-4 目的地改变即暂停**：恢复比对在记录的路径上确认缺失，但本次要写的最终路径与之不同（前缀或路径模板改了；或目标不给
  路径）时，不写：结果为 `EarlierWriteUnverified { verification: RemoteDestinationChanged { recorded, current } }`，按 deferred
  释放（保态、退还、保留恢复标记与记录的写入），事件带 `recorded_path` / `current_path`。原因：固定 HEAD 上的缺失不能排除先前那次
  请求稍后才落地，往别处再写一份就可能留下两份。路径相同则照常写——GitHub 单文件按 `sha` 更新（无 `sha` 时已有文件的 PUT
  被拒），批量的 ref 更新只接受快进，同一路径上至多留下一份内容（晚到的写入使这次 PUT 被拒时，记录按"其它 4xx"转 `failed`，
  需按 §Consequences 人工核对；文件本身仍只有一份）。暂停期间每次运行仍先做同一个比对，先前的写入一旦出现即记回执。
  **运维处理**：要么恢复原前缀 / 路径模板（目的地相同，下一次运行即写入原路径），要么在核实记录的路径上确无该 blob 之后，按
  §Consequences 的人工步骤把记录置为 `failed`（其文章随之释放，进入之后的报告，按新配置发布）。
- **R5-5 他人占用的目的地**：若**另一个分类**的记录已占用本次写入的确切最终路径——它在该路径上的写入仍未决（`stored_local`、
  `remote_write_unverified`、记录的路径相同、目标身份相同），或它已 `published_remote` 于该路径（回执 `remote_target` 相同）——
  则不写，以 `release_deferred_retry` 释放（保态、退还、清空记录的写入，`last_error_kind = remote_path_conflict`，事件带
  `path` 与 `holder_record_id`），每次运行重复同一检查，直到运维改开路径模板 / 前缀。按确切路径比较：git 路径区分大小写，只有
  确切相同的路径才会被覆盖。同一分类不在此列（重跑、`--force`、不含日期的模板本就覆盖自己的文件）。批量中这一检查在逐条处理
  之后进行，因此本批刚恢复或仍未决的记录也算在内；检查查询数据库，跨运行同样生效。`PublishTarget::receipt_target(path)` 给出
  该路径的回执形式（GitHub：`github://owner/repo/branch/path`；默认 `None`，不检查）。
- **R5-3 先记录再发送**：每个写请求（单报告的 `PUT contents`、批量的 `PATCH ref`；批量中每条成员）发出之前，带 owner 与阶段守卫
  写入 `last_error_kind = remote_write_unverified` 与本次写入的路径、blob（`mark_remote_write_pending`，保留 lease 与计数）；
  lease 已失或写库失败则不发送。写入的结果照常覆盖它：成功推进清空，拒绝 / 限流按原规则释放并清空，歧义写入照记。于是写入之后、
  记录结果之前崩溃，lease 过期回收后下一次 claim 会以恢复比对在**原路径**上找到它，而不是按当前配置再写一份。
  **仍未解决（残留）**：崩溃的这次尝试已计预算；若它是本阶段最后一次预算，lease 过期后 sweep 仍会把记录转 `failed`（未核对、
  归属释放），见下文"残留限制"。关闭这一窗口需要为预算耗尽的行提供一次只读核对入口，另行设计。
- **R5-4 不覆盖的边界**：同一次尝试内，写请求返回 5xx / 超时、随即的只读核对在 HEAD 上未找到 → 按原写入错误计预算释放（清空
  记录与恢复标记，R3F-S16）。先前请求在其后落地的论证同样适用于这里；由于下一次尝试写的是同一路径（目的地未变），GitHub 的
  `sha` / 快进约束仍保证该路径至多一份，故不扩展 R5-4。
**代价（未解决）**：每次尝试都以"写入歧义 + 核对未完成"结束、此后比对也一直不出结论的记录（例如 GitHub 长期对读写都不可达）
不会耗尽预算、一直持有文章归属；它每次运行最多再发一次只读比对。因此 `publish_max_attempts = 1` 或默认预算的最后一次尝试里
"PATCH 已落地、响应丢失"也会记为 `published_remote`，而不是 `failed` 并释放文章归属。限流（`GitHubRateLimit`）不核对，
避免在限流窗口内追加请求。比对**本身**的任何错误都不出结论，包括永久类的（401 / 认证 403 / 分支 404 等）：与 R3F-B2 的
核对一样，它们也走 deferred，因此 token 失效或分支被删时，待恢复的记录会一直保持 `stored_local`、持有归属而不转 `failed`；
唯一的信号是 `publish_failed` 事件里该比对请求的 `operation` / `http_status`（以及每次运行重复出现的 `remote_write_unverified`），
需运维处理（恢复凭据 / 分支，或按 §Consequences 的人工核对步骤）。比对之前的永久错误不在此列：目标漂移（`remote_target_mismatch`）
与永久的渲染错误仍直接 `failed`（运维改了配置或模板，同样需人工核对远端）。
**残留限制**：进程在写请求之后、核对之前崩溃（或在恢复比对途中崩溃），且该阶段预算已耗尽时，过期清扫仍会把记录转 `failed`
（未核对、归属释放）；预算未耗尽时，写请求之前已按 R5-3 记录恢复标记与路径 / blob，下一次运行的恢复比对在**原路径**上补回执
（0010 之前的行与不记录 blob 的目标退回多报告幂等比对 / 单报告幂等，§3 下文，它们只在路径与内容未变时成立）。
R5-1 的 deferred 释放与推进写同一个数据库：释放本身也失败时（例如数据库整段不可用），记录同样留着 lease，落入这条残留限制。

### 3a. 限流等待写入记录（2026-11-22，R1-P-S12，同属未发布的 migration 0008）

- `publish_records.next_attempt_at`（SQLite `TEXT` / PG `TIMESTAMPTZ`，可空）：远端阶段遇 `GitHubRateLimit` 时，
  release 写入 GitHub 要求的最早重试时刻——所有适用期限中**最晚**的一个（2026-11-22，R3F-S12：`retry-after`；
  配额耗尽 `x-ratelimit-remaining: 0`、或没有可用 `retry-after` 时再加 `x-ratelimit-reset`；都没有 → now + 60 s；见 plan/04 §7），
  并钳制到 `[now, now + 1 h]`——记录在等待期间仍持有文章归属，过远的时刻（或恶意头）不能把它长期挂起。
  **有意的取舍**：GitHub 要求超过 1 h 时（如两小时的 `retry-after`），记录在 1 h 后即可再领取，那次请求大概率再被限流，
  再写一次期限；用多一次请求换取归属不被长期挂起。其它 retryable release 把它写回 NULL，每次阶段推进（含 freeze）清零。
- **限流不计阶段预算（R3F-S12；2026-11-22 R4-4 扩展到所有限流）**：任何 `GitHubRateLimit` 尝试都用 `release_deferred_retry`
  释放（与 §3 核对未完成相同）：保持阶段态、退还本次 claim，因此最后一次预算遇到限流也不会转 `failed`、不会释放文章归属。
  期限在未来 → 写入 `next_attempt_at`，节流由它负责；期限已到或已过（`Retry-After: 0`，或正数期限在释放前刚好过去）→
  不写等待（NULL），下一次运行即可再领取。**有意的取舍**：已过期限的限流因此可以不计预算地反复重试；节流来自运行节奏本身
  （每次运行每条记录至多一次尝试），且发布客户端不在一次 claim 内重发 429（R3F-B1）。GitHub 拒绝请求不是一次失败的发布尝试，
  按预算把记录转 `failed` 会释放可能仍在等待发布的文章归属（R3F-S12 的本意）。
- 四个 claim 入口（按状态扫描的 SQLite / PG SQL 与两个按 ID 的构造器）都要求 `next_attempt_at IS NULL OR <= now`；
  SQLite 照 lease 的做法按 `julianday` 比较（写入向上、now 向下取整到毫秒），不会提前领取。
- 等待期内的运行领不到记录：`NothingToClaim`（verdict `skipped`，exit 0），不发任何 GitHub 请求、不耗预算；
  `publish_failed` 事件带 `retry_not_before`。耗尽 sweep 不看该列（耗尽行本就不会被领取）。
- 存量行为 NULL；down 迁移删除该列。

### 4. 默认报告日期按目标时区

未给 `--date` 时，`publish` / `publish-all` 取运行时刻在有效目标时区（`--timezone` / `[publish].target_timezone`）下的日期。
时区经 jiff 按 IANA 库解析（有系统 zoneinfo 用系统库，否则用内置库），validate-config、doctor 与运行时用同一查询；
未知时区在任何写入之前报错，不回退 UTC。三处共用 `config::validate::parse_target_timezone`（2026-11-22，R1-P-S14）：
不 trim，且拒绝 jiff 的特殊 `Etc/Unknown`（`TimeZone::is_unknown()`，任意大小写都能查到、按 UTC 运算）。

## Alternatives considered

- **保留共享预算，校验 `publish_max_attempts ≥ 4`。** 改动最小，但只修掉"小预算必然失败"这一表象：每阶段可用的失败次数
  仍取决于前面阶段花掉多少，默认 5 时整链仍只容忍 1 次失败；而且把一个阶段机的内部细节（阶段数）泄漏成配置下限，
  阶段数一变配置就要跟着改。未采用。
- **只在冲突时检测（conflict-only），不在选稿时排除。** 即沿用现状：重复入选，等到发布终态推进文章时再以
  `ArticleStateConflict` 回滚。问题是冲突发现得太晚——两份快照都可能已经写出本地文件或推到 GitHub，回滚只回滚数据库。
  归属必须在任何外部写入之前确定，因此选稿排除 + freeze 事务内复查。未采用。
- **用唯一约束表达归属。** 归属依赖另一张表的状态（`state <> 'failed'`），无法用 `publish_items` 上的普通或部分唯一索引表达；
  另建归属表会引入第二个真相源。未采用。
- **多报告幂等改为查整棵 tree（`git/trees?recursive=1`）。** 大仓库响应可能超过 16 MiB 上限且被截断，截断的列表不能作为
  "全部存在"的证据；按路径查询 N 次（N = 本批分类数，通常个位数）更稳。未采用。

## Consequences

### 正面后果
- `publish_max_attempts` 的语义与阶段数解耦；小预算的无故障运行可以完成。
- 同一文章不会同时出现在两份活动快照里，也就不会先外部写入、后数据库回滚。
- AI-off 遗留文章不再卡住整批 freeze；freeze 冲突不再占着 lease 等到过期。
- 多报告重试不会在 ref 已更新时再叠一个提交。
- 默认报告日期与目标时区一致（如上海早上 8 点前不再落到前一天）。

### 负面后果 / 代价
- 选稿多一个 `NOT EXISTS` 子查询（有索引）；多报告批次在建 commit 前多 N 次 `GET contents`。
- 迁移 0008 改变 `publish_max_attempts` 的含义：同一配置值下可用的总尝试次数变多（最多 4 × N）。
- **已知限制（未在本 ADR 解决）**：各阶段都按本次运行 `init` 返回的 `publish_record_id` 领取，没有任何入口会自动续跑或
  终结一份停在中间阶段、且 lease 已清、阶段预算未耗尽的**更早日期**的记录。归属规则下这样的记录会一直持有其文章，
  使它们进不了之后的报告，直到运维对该日期重跑 `publish --date <那天>`（续跑或失败后释放）或人工把记录置为 `failed`。
  此前同样的记录也不会被续跑，但文章仍可能被重复选入；现在改为"不重复、但需要人工收尾"。
- **已知限制：`--force` 记录无法定向续跑（R1-P-S5，2026-11-22 记录，暂不新增 CLI 入口）**。force 记录的 render version
  是一次性的 `pending` trace 行。若它在 freeze 之后、预算未耗尽时停住（例如远端短暂失败），普通 `publish --date <那天>`
  取 active render 版本、再次 `--force` 又新建 trace 行，两者的幂等键都不是原记录，原记录不会被续跑，而它持有的文章被
  归属规则排除在之后的报告之外（同日即可发生）。上文"按日期重跑"的办法对 force 记录无效。**人工处理**（2026-11-22 改为原子守卫，R3F-B4；
  守卫改为比对核对时读到的确切行版本，R4-5）：
  1. 找出候选并**记下每条的 `state`、`render_version`、`updated_at` 原值**：`SELECT id, state, render_version, updated_at,
     last_error_kind, remote_target, local_path, unverified_write_path, unverified_write_blob_sha, lease_owner,
     lease_expires_at FROM publish_records
     WHERE state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local') AND render_version IN (SELECT id FROM rule_versions
     WHERE kind = 'render' AND version_tag LIKE 'force-%')`。
  2. **先核实远端回执**：`stored_local` 记录（尤其 `last_error_kind = remote_write_unverified` / `remote_timeout`）的远端写入
     可能已落地而回执未落库。在目标分支 HEAD 上比对报告路径的 blob 与本地文件（`git hash-object <local_path>`，GitHub
     `GET contents/<path>?ref=<branch>` 的 `sha`）。行上有 `unverified_write_path` / `unverified_write_blob_sha`（R5-2）时，
     以它们为准：那是那次写入的确切路径与 blob。**已存在则不要置 `failed`**：其文章已在远端发布，置 `failed` 会释放归属、
     让它们重新入选；保留记录，等待定向续跑入口（见后续行动）。
  3. 只对确认未发布、且没有进程正在处理（无有效 lease）的记录，在事务中执行下面的守卫式 UPDATE。守卫在同一条语句里
     复核"仍是 force 记录、无有效 lease"，并且行的 `state`、`render_version`、`updated_at` **仍等于第 1 步读到的值**：
     核对之后 cron 对该行的任何领取、释放或推进都会改写 `updated_at`（例如把 `rendered` 推进到 `stored_local`、远端写入后
     释放 lease；或领取后以 deferred retry 退回同一个 `stored_local`——state 与 render_version 都不变，只有 `updated_at` 不同），
     这些情况下第 2 步的远端核对已经过时，语句更新 0 行。`RETURNING id` 必须**恰好返回一行**才 `COMMIT`；
     **0 行 → `ROLLBACK`，从第 1 步重新读取，并重新做第 2 步的远端核对**（行已被处理过，远端可能已写入）。
     - SQLite（3.35+，lease 按时刻比较，与 reclaim 相同的 `julianday` 谓词；无法解析的 lease 文本使谓词为 NULL、不更新；
       `updated_at` 是 TEXT，按第 1 步输出的原文逐字比较）：
       `BEGIN IMMEDIATE; UPDATE publish_records SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL,
       last_error_kind = 'operator_failed', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = <id>
       AND state = '<第 1 步的 state>' AND render_version = <第 1 步的 render_version>
       AND updated_at = '<第 1 步的 updated_at 原文>' AND render_version IN (SELECT id FROM
       rule_versions WHERE kind = 'render' AND version_tag LIKE 'force-%') AND (lease_expires_at IS NULL OR
       julianday(lease_expires_at) < julianday('now')) RETURNING id;` → 恰好一行 `COMMIT;`，否则 `ROLLBACK;`
     - PostgreSQL（`updated_at` 为 `timestamptz`，psql 输出带时区偏移与微秒，原样抄回即精确相等）：
       `BEGIN; UPDATE publish_records SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL,
       last_error_kind = 'operator_failed', updated_at = now() WHERE id = <id> AND state = '<第 1 步的 state>'
       AND render_version = <第 1 步的 render_version> AND updated_at = '<第 1 步的 updated_at>'::timestamptz
       AND render_version IN (SELECT id FROM rule_versions WHERE kind = 'render' AND version_tag
       LIKE 'force-%') AND (lease_expires_at IS NULL OR lease_expires_at < now()) RETURNING id;` → 恰好一行 `COMMIT;`，
       否则 `ROLLBACK;`
     第 3 步的 state 必须是第 1 步读到的那一个（四个非终态之一），不要改回 `state IN (…)`：那样会接受核对之后才到达的状态。
  4. 再按需重新 `publish --date <那天> --force`。
  按记录 ID（或原 render version）定向续跑的入口（如 `publish --record <id>`，沿用原目标与快照）列为后续行动。
- **已知限制：旧版 `stored_local` 记录的本地字节（R1-P-S10，B8）**。`f420d95` 及更早版本渲染时用调用方时间生成本地文件，
  另一次采样写入 `rendered_at`；升级后远端阶段与 rebuild-report 从 `rendered_at` 重渲染，因此模板含 `{generated_at}` 时，
  升级前已 `stored_local` 的记录推到 GitHub 的字节可能与本地文件不同（相差的只是该时间戳），0008 也没有原时间可供恢复。
  字节一致保证只适用于新版首次渲染的记录；不做自动比对。
- **已知限制：存量 Contents 回执只比对 blob sha（R1-P-S3，后续）**。幂等 / 歧义核对按 Contents API 的 `sha` 比对，
  不检查目标树条目的 `mode`（`100644` vs `100755`）与类型（Contents API 会解引用指向仓库内普通文件的 symlink）。
  在固定 commit 上经 Trees API 核对条目类型、mode 与 sha 列为后续行动；目前一个 200 但无 `sha` 的响应已按协议错误处理（R1-P-S1）。
- 新依赖：`jiff`（IANA 时区；内置 tzdb）、`sha1`（git blob id，lockfile 中已有同版本）。

### 后续行动
- 评估为"更早日期的中间态记录"增加有界的自动收尾（例如 maintenance 中按 report_date 过期终结），需另行设计与审批。
- 按记录 ID 定向续跑 force 记录的 CLI 入口（R1-P-S5，见上文已知限制）。
- 旧版遗留的 active force trace 修复（R1-P-S6 前半，后续）：B10 之前的版本在空库首次 `--force` 时会把 `force-…` render 行
  建成 `active`，0008 之后普通发布仍会选中它。识别并恢复明确的 `default` 规则需要迁移或启动期修复，另行设计。人工修复
  （同一事务内）：`UPDATE rule_versions SET status = 'superseded' WHERE kind = 'render' AND status = 'active' AND version_tag
  LIKE 'force-%';` 然后 `UPDATE rule_versions SET status = 'active' WHERE kind = 'render' AND version_tag = 'default';`
  （若无 `default` 行，下一次发布会自行登记）。引导现已拒绝盖上非 active 的版本（R1-P-S6 后半）。
- 在固定 commit 上经 Trees API 核对条目类型与 mode（R1-P-S3）。
- 本地写入改为基于目录句柄（`openat` / `renameat`）以消除输出根内并发写者的 TOCTOU（R1-P-B4，见 plan/04 §4.1）。
- ~~单报告批次走 Contents API，不在本 ADR 的幂等范围内。~~ 已补（2026-11-22，R1-P-S2）：单报告路径在 `GET contents?ref=<branch>`
  取到的 blob sha 与本地 git blob sha 相同时，先按固定 HEAD commit 复核（`find_published`），一致即以该 HEAD commit 记回执、
  不发 PUT（也就不需要写权限）；HEAD 已移动且不含该 blob 时照常 PUT，`sha` 取自固定 HEAD 复核（现有 blob 或缺失，R3F-S2）。这覆盖了"批量回执部分落库后中断、重跑只剩一条报告"
  的恢复。

## Links

- 代码：`migrations/{sqlite,postgres}/0008_publish_stage_budget_and_item_article_index.*.sql`、
  `0010_publish_unverified_write_blob.*.sql`（R5-2）；`crates/publish/src/target.rs`（`remote_blob`、`find_remote_blob`）；
  `crates/storage/src/repo/publish_record_sql.rs`、`publish_item.rs`；`crates/runtime/src/flows/publish/freeze.rs`、
  `report_date.rs`；`crates/publish/src/github.rs`（`head_contains_blobs`、`git_blob_sha`）
- 计划：[plan/04 §2、§4.2](../plan/04-publish.md)、[plan/08 §6.1](../plan/08-state-machines.md)、
  [plan/11 §4.1](../plan/11-error-and-recovery.md)、[plan/15](../plan/15-retry-exhaustion-and-reclaim.md)、
  [plan/05 迁移表](../plan/05-storage.md)
- 验收：[acceptance-cases/pipelines/04-publish-local-and-github.md](../acceptance-cases/pipelines/04-publish-local-and-github.md)
- 相关 ADR：[0002](./0002-stage-driven-lease-claim.md)、[0003](./0003-publish-snapshot-immutable.md)、[0005](./0005-storage-pool-dual-dialect.md)
