# decision-eval：Phase C 只读导出

为 typed-decision Phase C（[ADR 0010](../../docs/adr/0010-typed-decision-capability.md) §Phase C、
[ADR 0011](../../docs/adr/0011-decision-run-shadow-runner.md)）导出人工标注所需的数据。只支持 PostgreSQL。

不是产品功能：不进 `rss-ai-news` 二进制、不加子命令、不改表。CLI 打开存储时会自动迁移，
[`phase-c-export.psql`](./phase-c-export.psql) 则是纯 SQL，由 `psql` 执行。

## 安全性

- 整个脚本运行在一个 `REPEATABLE READ READ ONLY` 事务里：服务器拒绝任何写入；四个文件取自同一快照，
  即使 `decision-run` / `ai-run` 同时在写也一致。
- 每条语句 `statement_timeout = 120s`、`lock_timeout = 5s`；客户端在两次查询之间卡住超过 60 s 时
  `idle_in_transaction_session_timeout` 终止会话（打开的快照会拖住 vacuum）。这些都不是整次运行的总期限。
  只取 AccessShare 锁，不阻塞业务读写；会与需要 AccessExclusive 的维护操作（如 `VACUUM FULL`、部分 `ALTER TABLE`）互相等待。
- 前置检查：若有文章在 `decision_results` 中属于多个板块（正常不会发生：板块来自文章唯一的 feed 来源），脚本报错退出（psql
  退出码 3），不写任何文件。
- 输出只写到执行 `psql` 的当前目录（客户端本地），不写数据库所在机器的文件系统。
- 输出包含文章标题与链接，按私有数据保管，不要提交进仓库。

## 运行

`DATABASE_URL` 用部署时的连接串；不要把它打印出来或写进文件。

**主机上有 `psql`（≥ 12）**

```bash
mkdir -p ~/phase-c-out && cd ~/phase-c-out
psql "$DATABASE_URL" -X -v ON_ERROR_STOP=1 -f /path/to/RSS-AI-News/tools/decision-eval/phase-c-export.psql
```

**没有 `psql`：用与 CI 相同的 `postgres:16-alpine` 镜像当客户端**

```bash
mkdir -p ~/phase-c-out && cp /path/to/RSS-AI-News/tools/decision-eval/phase-c-export.psql ~/phase-c-out/
docker run --rm --network <数据库可达的网络> -e DATABASE_URL -v ~/phase-c-out:/out -w /out \
  postgres:16-alpine sh -c 'psql "$DATABASE_URL" -X -v ON_ERROR_STOP=1 -f phase-c-export.psql'
```

`-e DATABASE_URL` 只转发当前 shell 里**已 export** 的变量。`--network`：数据库在 compose 里时用该 compose 的网络
（`docker network ls`），连接串里的 host 写服务名；数据库直接跑在宿主机上时，Linux Docker Engine 用 `--network host`
（Docker Desktop 需 ≥ 4.34 并在设置里启用 host networking）。

可选变量：`-v per_category=200`（每板块抽样上限）、`-v seed=phase-c-1`（换 seed 即换一批样本）。
成功时最后一行输出 `wrote summary.csv policies.csv labels.csv results.csv`。

## 输出

| 文件 | 一行是 | 用途 |
|---|---|---|
| `summary.csv` | 板块 × policy_version × execution_key × state（**全量**，不只样本） | 先看有多少数据、跑了哪些身份、token 合计、截断数；`paired_other_provider_articles`（只对 succeeded 行计）是同一文章在**同一 policy_version**（即同一组问题）下另一家 provider 也成功的文章数，即 Laya / Jev 逐题配对比较能用的样本池 |
| `policies.csv` | 一个策略快照 | `canonical_json` 是问题定义（题意、选项、档位），用来解读 `answers_json` |
| `labels.csv` | 样本文章 × 问题：只取对该文章**成功过**的策略的问题（只失败过的策略没有答案可比）；跨策略内容完全相同的问题只出现一次，`policy_versions` 列出涉及的策略 | **盲标表**：不含任何模型输出。填 `label`（格式见 `label_hint`），可选填 `note` |
| `results.csv` | 样本文章的一条 `decision_results`（所有 state） | 模型侧全部信息：`answers_json`、执行身份与部署标识、`input_tokens`、截断，以及该文章最新一条已完成的生成式 AI 结果（`ai_keep_decision` / `ai_importance_score`） |

`labels.csv` 与 `results.csv` 用 `article_id` 连接；交接里提到的"一张大表"就是 `results.csv`。
把模型输出单独放一个文件，是为了标注时看不到模型的判断，避免先入为主；标注完再连接。

## 抽样与口径

- 抽样单位是**文章**：每个板块里，至少有一条 `succeeded` 判断（任一策略、任一执行身份）的文章，按
  `md5(article_id || seed)` 排序取前 `per_category` 篇。与任何模型输出无关，在这一总体上近似均匀。
  可复现的前提是同一数据快照与同样的参数；在活库上隔一段时间重跑，总体变了，样本也会变。
- 只有失败、待处理行的文章不进样本；覆盖率与失败情况看 `summary.csv`。
- 生成式 AI 结果取 `state IN ('succeeded', 'filtered')` 中最新的一条：`keep_decision = 0` 的行存为 `filtered`，
  只取 `succeeded` 会丢掉全部负例。`ai_finished_results` 是该文章已完成 AI 结果的条数；> 1 表示存在多个已完成的
  `(prompt_version, output_schema_version, model_id)` 组合。没有已完成 AI 结果的文章，`ai_*` 明细列为空、
  `ai_finished_results` 为 0（`decision-run` 不依赖 AI 阶段）。
- `retired` 文章不排除。

## 标注须知

- 模型看到的是标题 + 正文（按策略里的 `max_state_chars` 截断），所以要打开 `canonical_link` 读原文再标；只看标题标出的结果
  衡量的是另一件事。
- `label` 写法：noul 填 `yes` / `no`；choice 填选项的 label；score 填档位下标（`levels` 列给出 `0=…` 的对应）。
  拿不准就留空并在 `note` 说明，不要猜。

## 已验证 / 未验证

- 已验证（2026-11-16，本地 `postgres:16-alpine`，库由真实 `rss-ai-news migrate run` 建到版本 6，合成数据）：
  抽样上限与板块划分、跨策略问题去重、`filtered` 计入、无 AI 结果时为空、含逗号 / 引号 / 换行的标题按 CSV 正确转义、
  同一快照上重跑逐字节相同、换 seed 与 `per_category` 生效、导出前后四张表内容摘要不变、`READ ONLY` 事务拒绝 INSERT；
  `paired_other_provider_articles` 只计同策略的另一家 provider；多板块前置检查以退出码 3 中止且不写文件；
  数据库容器内的 `psql` 与经用户网络连接的 `postgres:16-alpine` 客户端输出一致。
- 未验证：生产库上的运行（数据量、耗时、真实问题定义）。没有 SQLite 版本。
- 两处抽样 CTE（`labels.csv` 与 `results.csv`）必须保持一致；改动时一起改。
