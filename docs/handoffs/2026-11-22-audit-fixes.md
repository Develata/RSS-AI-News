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
修复后又经三轮 Codex 复审收敛；最后一轮之外的发现记入下方遗留风险。

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

### 手工验收

- release.yml 的晋升、门禁与标签核对：未执行（需真实 tag；脚本已用本地 registry 与假 registry 验证）
- 真实 GitHub / RSSHub / AI 请求：未执行

## 结果

- 可合并到 main，前提是 Develata 审阅；未推送。首次真实发布前先在 tag 上观察新 release.yml。

## 风险与后续事项

- 记录若反复以"未核实"或"限流等待"结束，永不进入 failed 并持有文章（ADR 0012 §3/§3a）
- 限流等待上限 1 h、extract Retry-After 上限 1 天：超出的服务器期限会被提前重试
- 旧端点的 200 响应在过期判定前仍可能写入 entries
- 并发 batch=1 worker 的轮换是尽力而为；SQLite 亚毫秒窗口边界
- release 脚本的"tag 缺失"判定仍基于 stderr 文本；首次真实 GHCR 运行需确认
- doctor deep scan 仍比较累计 attempt_count；doctor 不读 RSSHub 展开标记
- 刻意未做：未声明编码页的 windows-1252 猜测、extract 排除集合改造、去除 aws-lc JWT、合并 html5ever、release LTO、arm64 镜像
- 遗留在中间提交上的 bisect 风险：部分包的中间提交单独不能通过 clippy / static lane（各包报告已列出）

## 给下一位 Agent 的备注

- 入口：`docs/adr/0012-*.md`、`docs/plan/03-ai.md §5.1/§5.2`、`docs/plan/04-publish.md`、`docs/plan/02-extract.md §5`
- 发现与处置记录在会话 scratchpad（不入库）；提交消息里的 `[ID]` 可回溯到对应发现
