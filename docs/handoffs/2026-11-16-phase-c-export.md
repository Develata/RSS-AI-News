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

## 2026-11-16 追加：本地 A/B（题目语言与写法）与 GLiNER2.5 对照（探索性）

数据与脚本都在会话 scratchpad，不进仓库；以下只记结论与可复现要点。

- 样本：本地 SQLite 抓取 6 个源共 193 篇（英文 117：DeepMind 99、Google AI 18；中文 76：IT之家、雷锋网、InfoQ、量子位）。
  参照为 Haiku 与 Sonnet 各自独立标注（同一细则、读 Laya 实际收到的前 6000 字），**不是人工标注**。两者一致：营销 κ 0.72、技术价值 QWK 0.88。
- Laya（`multilingual`，2048）5 道题：中文原题、英文直译、正向提问加 yes/no 释义；技术价值中文、英文。对 Sonnet：
  营销 AUC 0.56 / 0.62 / 0.65，技术价值 Spearman 0.22 / 0.06。换语言基本不改变 Laya 的判断（中英题 Spearman 0.80），只是和 LLM 对不上。
  Laya 从不判技术价值最低档（193 篇里 0 篇）。
- 原因（已核实来源）：Laya 模型卡写明 `english` / `multilingual` 是零样本基座，官方基准准确率 0.362 / 0.342（随机 0.318），
  定位是 "a fast base to specialise, not a zero-shot decision engine"。只有微调过的 `typed-decisions` 到 0.766。
- Jev（`jev-1.13.0`，同 5 道题、同截断）：193 篇 0 失败，共 294,642 input token（第三方报价约 $0.04 / 百万，约 1 美分），平均 0.53 s。
  对 Sonnet：营销 AUC 0.89 / 0.90 / 0.91（三种问法），技术价值 Spearman 0.80、QWK 0.74（英文题 0.78 / 0.70）；中文子集反而更高
  （营销 0.94–0.98，QWK 0.79）。档位分布 47 / 88 / 49 / 9，与 LLM 接近。**是目前唯一接近 LLM 间一致度的判断模型。**
- GLiNER2.5（Fastino，Apache 2.0，本地 CPU 推理，`gliner2` 2.1.0）同样数据、两种写法（只给题目和档位名 / 附 LLM 细则）。对 Sonnet：
  - `GLiNER2.5-multi-Decide`（287M，全量）：营销 AUC 0.74（中文 0.88）；技术价值 Spearman 0.31，但 argmax 偏向最高档。
  - `GLiNER2.5-Decide`（340M，仅英文 117 篇）：技术价值 Spearman 0.52（Laya 在同一子集为 −0.06）；
    营销只给题目时 AUC 0.60，附细则后反向（0.28），说明对 label 描述很敏感。
  - 两者都比 Laya 好，但离 LLM 之间的一致度仍远，且概率未校准；要用于过滤仍需微调或阈值拟合。
- 运维发现：
  - Laya 前的 1Panel WAF 会拦正文含 `curl … http://127.0.0.1…` 的请求（HTTP 403 拦截页）。客户端把 403 当 `Unauthorized`，
    整轮停止并把该行退回 pending，下次运行又先领到同一篇，**生产上会因一篇文章卡住**。
    已修：403 改归 `HttpStatus`（计次、退出码 1，最多卡 `max_attempts` 次运行），见 ADR 0011 §10 的 2026-11-16 修订。
  - 远端 Laya 处理 4000 字以上中文正文时先超时、后 502（推断为内存不足）。本地 arm64 自建镜像（laya-server v0.1.2）单并发峰值 2.07 GB，
    193 篇无失败，输出与远端一致（概率差 ≤ 1e-4）。
