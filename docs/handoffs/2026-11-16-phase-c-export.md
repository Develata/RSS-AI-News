# Phase C：只读标注导出

- 日期：2026-11-16
- 作者 / Agent：Claude Code（Opus）
- 分支：main
- 相关 commit：本记录所在提交
- 相关 tag / release：N/A（不进产品二进制，无需发版）
- 状态：`validated`（仅本地合成数据；生产库上未运行）

## 工作摘要

Develata 确认开始 Phase C 的第一步：从生产库只读导出人工标注所需的数据。生产用 PostgreSQL，在生产机上直接对活库运行。
新增 `tools/decision-eval/phase-c-export.psql`（纯 SQL，`psql` 执行）与 README；产品代码、表、迁移均未改。

## 影响范围

- crate / 模块：无（`tools/decision-eval/` 只有 SQL 与文档，不是 workspace 成员）
- 真相源对象：N/A。读取 `decision_results`、`decision_policies`、`articles`、`feed_entries`、`article_ai_results`
- 额外影响：无

## 关键决定

- **不做成 CLI 子命令**：CLI 打开存储会自动迁移，不是只读的；子命令还会扩大公开接口，并加长 ADR 0011 §13 的卸载清单。
- **只读与一致性**：一个 `REPEATABLE READ READ ONLY` 事务，四个文件来自同一快照；每条语句 120 s、锁等待 5 s、事务内空闲 60 s。
- **盲标**：`labels.csv` 不含任何模型输出；模型侧全部信息在 `results.csv`（交接要求的"一张大表"），用 `article_id` 连接。
- **抽样**：每板块至少有一条成功判断的文章，按 `md5(article_id || seed)` 取前 200 篇，与模型输出无关；不分层。
- **AI 结果口径**：`keep_decision = 0` 的 AI 行存为 `filtered`，因此按 `state IN ('succeeded','filtered')` 取最新一条；
  只取 `succeeded` 会丢掉全部负例（已在 `article_ai_result_impl.rs` 核实）。
- 只写了 PostgreSQL 版本（按 Develata 的回答）；没有 SQLite 版本。

## 验证与验收

本地 `postgres:16-alpine`，库由真实 `rss-ai-news migrate run` 建到版本 6（12 张表 + `_sqlx_migrations`），合成数据：2 个板块、3 个策略、
Jev / Laya 两种 provider、失败与 pending 行、`filtered` AI 行、含逗号 / 引号 / 换行和中文的标题。

- 计数与连接与 fixture 一致（ai 241 篇候选取 200、math 20 篇；Jev token 合计 124920 手算核对）；
- 跨策略相同问题去重；没有 AI 结果时为空、计数 0；最新 AI 结果取到 `filtered` 行；
- 同一快照重跑逐字节相同；换 `seed` / `per_category` 生效；
- 导出前后四张表内容摘要不变；`READ ONLY` 事务拒绝 INSERT；
- `paired_other_provider_articles` 的正例（同策略、另一 provider）为 5/5；多板块前置检查以退出码 3 中止、不写文件；
- README 的两种运行方式（库容器内 `psql`、经用户网络的 `postgres:16-alpine` 客户端）输出一致。

codex-reviewer（gpt-6-astra，medium）：无 BLOCKER。采纳并修正 5 项 SHOULD_FIX（配对计数口径、排序决胜键、空闲事务超时、
AI 计数说明、Docker host 网络前提）与 1 项 NEEDS_VERIFICATION（多板块不变量改为前置检查）；另 1 项（标注表只取成功过的策略的问题）
确认为有意口径，写进 README。

未验证：生产库上的运行（数据量、耗时、真实问题定义）。

## 给下一位 Agent 的备注

- 下一步等 Develata 跑导出并完成标注；之后是评估（noul 的 AUC、`top_probability` 校准、与 AI `keep_decision` 的相关、
  Laya vs Jev、每篇 `input_tokens`、留出数据上的阈值拟合）。让判断参与过滤是骨架级变更，需先写 ADR 0012 并获批。
- 导出文件含文章标题与链接，不进仓库。

## 2026-11-16 追加：首轮生产导出与 LLM 对照（探索性）

- 生产导出（Develata 在生产 PG 上运行，数据不进仓库）：只有 1 个板块 `ai_ml`、1 个策略、1 个执行身份 `laya:multilingual:2048`，
  2408 篇成功、无失败；平均 1183 input token / 篇；正文本地截断 11%、Laya token 截断 4.9%。没有 Jev 数据，无法做 Laya vs Jev。
- 参照标注：按 Develata 要求由 8 个 Haiku 子代理对 200 篇样本标注同样两题（164 篇读到原文，36 篇只看标题），
  **不是人工标注**；目的就是比较判断模型与通用 LLM 的差距。Haiku 看不到 Laya 答案。
- 结果（n=200，括号为 bootstrap 95% CI；"只读到原文"子集结论相同）：
  - `q_marketing`：Laya 判 53% 为营销，Haiku 11%；AUC(Laya p_yes → Haiku yes) 0.68（0.59–0.77），0.5 阈值一致率 0.54。
    Laya 的营销概率与它自己的技术价值正相关（Spearman +0.41），Haiku 为 −0.35；与 deepseek 保留决定的 AUC 为 0.67（营销概率越高越常被保留）。
    推断：题目"是否主要是营销宣传，**而不是**提供可核实的新技术信息"含否定对比且未填 yes/no 释义，Laya 很可能部分按相反或"是否技术新闻"理解。未验证。
  - `q_technical_value`：Laya 从不判 0 档，68% 判 1 档；Haiku 多判 2 档。完全一致 0.36、相差 ≤1 档 0.95、QWK 0.29（0.17–0.39）、
    Spearman 0.32（0.18–0.44）。`top_probability` 与和 Haiku 的一致率无单调关系（未校准，与 ADR 0010 的预期一致）。
  - 对 deepseek 保留决定的区分力：Laya 技术价值期望 AUC 0.75（0.67–0.81），Haiku 0.80（0.74–0.86）；对 deepseek 重要性分数
    （仅保留文章，55–92）两者都几乎不相关（≈0.07 / 0.10）。
- 建议的下一步（待 Develata 决定）：重写 `q_marketing`（正向提问，填 yes / no 释义），会产生新的 policy_version，旧结果保留；
  跑一段时间后用同一导出再比一次。在此之前不宜让该题参与过滤。
