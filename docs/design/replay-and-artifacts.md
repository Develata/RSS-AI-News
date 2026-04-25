# 回放与 Artifact 设计

## 1. 定位

本文档定义 Rust 版的 raw artifact 存储契约和 replay 能力设计。它是宪法"所有外部输入都必须允许 replay 模式"和"replay 作为正式能力"的实现级契约。

与之配套：
- 存储表结构见 [storage-schema §4.7 raw_artifacts](./storage-schema.md)
- CLI 语义见 [cli-semantics §4.5](./cli-semantics.md)
- DTO 定义见 [internal-dto-contracts §6](./internal-dto-contracts.md)

## 2. Artifact 种类

### 2.1 三类 Artifact

| kind | 内容 | 来源阶段 | artifact_key 格式 |
|---|---|---|---|
| `feed_payload` | feed 原始 XML/JSON 响应体 | ingest | `{source_id}` |
| `html_payload` | 详情页原始 HTML | ingest/extract | `{feed_entry_id}` |
| `ai_raw_response` | AI API 原始响应文本 | ai-run | `{article_ai_result_id}` |

### 2.2 `artifact_key` 设计

`artifact_key` 是同 `kind` 下的唯一业务标识。设计原则：

- 必须包含足够信息让 replay 定位到来源对象
- 必须是确定性的，且不携带时间戳——同一来源对象的多次抓取覆盖写入，只保留最新一份
- `UNIQUE(kind, artifact_key)` 配合 UPSERT，磁盘占用与来源对象数线性相关，可控
- 历史时序需求由 `run_events` 承担，不由 `raw_artifacts` 承担

> 设计取舍：曾考虑在 key 中拼接 `fetch_timestamp` 以保留每次抓取的快照，但与 [storage-schema §6.4](./storage-schema.md) 的 "同 key 只保留最新内容，不允许追加历史" 语义冲突，且会让磁盘占用随抓取次数线性增长。最终选择覆盖式，对调试罕见旧抓取的需求由 `retention_policy='always'` + 增量备份方案兜底。

### 2.3 存储策略

#### 内联 vs 文件

| 条件 | 存储方式 | 字段 |
|---|---|---|
| `byte_size <= inline_threshold_bytes` | 直接存表 | `storage_kind='inline'`, `inline_body` 非空 |
| `byte_size > inline_threshold_bytes` | 外部文件 | `storage_kind='file'`, `file_path` 非空 |

`inline_threshold_bytes` 默认 64 KB（见 [config-schema §4](./config-schema.md)）。

#### 文件存储路径

```text
{artifact.file_storage_dir}/{kind}/{YYYY}/{MM}/{DD}/{artifact_key_safe}.{ext}
```

- `artifact_key_safe`：将 `:` 替换为 `_`，防止文件系统问题
- `ext`：`feed_payload` → `.xml` / `.json`；`html_payload` → `.html`；`ai_raw_response` → `.json`

#### 编码

- `content_encoding = "utf8"`：原始文本
- `content_encoding = "gzip"`：gzip 压缩（大 payload 推荐）
- `content_encoding = "base64"`：二进制内容的文本安全编码

首版统一使用 `utf8`，后续版本可按 payload 大小自动选择 gzip。

## 3. 保留策略

### 3.1 五种策略

| retention_policy | 行为 | 适用场景 |
|---|---|---|
| `always` | 始终保留，不设 TTL | 调试环境、关键数据 |
| `on_failure` | 仅在关联操作失败时保留 | 生产环境默认 |
| `sampled` | 按概率采样保留 | 生产环境审计 |
| `debug_only` | 仅在 `log_level=debug/trace` 时保留 | 开发环境 |
| `off` | **完全不写入** `raw_artifacts`；该配置只影响写入路径，DB 列永不出现该值 | 纯离线测试 / 磁盘紧张场景 |

### 3.2 策略执行

写入时：
1. 根据 `config.artifact.retention_policy` 和当前操作结果决定是否保留
2. `on_failure`：操作成功 → 不写入 artifact；操作失败 → 写入
3. `sampled`：按 `config.artifact.sample_rate` 概率决定
4. `debug_only`：检查当前日志级别

清理时：
1. 后台任务定期扫描 `expires_at < NOW()` 的行
2. 如果 `storage_kind = 'file'`，先删除文件，再删除行
3. 删除事件写入 `run_events`

### 3.3 TTL 计算

```text
expires_at = created_at + config.artifact.ttl_days
```

`retention_policy = "always"` 时 `expires_at = NULL`，永不过期。

## 4. Replay 能力

### 4.1 设计目标

- 脱离外网可执行：所有输入从 `raw_artifacts` 读取
- 验证解析逻辑：用新版解析器重新处理旧数据
- 对比差异：与当前数据库状态对比，发现回归
- 调试故障：重现失败场景

### 4.2 三种回放模式

#### Feed Replay

```text
输入：raw_artifacts WHERE kind='feed_payload' AND artifact_key=:key
流程：
  1. 读取 artifact body
  2. 用当前版本的 feed-rs 解析
  3. 用当前版本的 link_normalizer 规范化
  4. 输出解析出的 FeedEntryMeta 列表
  5. （可选）与 feed_entries 表中已有条目对比
```

#### HTML Replay

```text
输入：raw_artifacts WHERE kind='html_payload' AND artifact_key=:key
流程：
  1. 读取 artifact body
  2. 用当前版本的 extractor 提取正文
  3. 计算 content_hash
  4. 输出 ExtractedArticle
  5. （可选）与 articles 表中对应文章对比
```

#### AI Replay

```text
输入：raw_artifacts WHERE kind='ai_raw_response' AND artifact_key=:key
流程：
  1. 读取 artifact body（原始 AI 响应文本）
  2. 用当前版本的输出解析器重新解析
  3. 输出 AiOutput
  4. （可选）与 article_ai_results 表中对应结果对比
```

### 4.3 Replay 输出格式

#### 默认（人类可读）

```text
=== Feed Replay: source_id=3 (latest snapshot) ===

Entries parsed: 25
  [1] "OpenAI Announces GPT-5" → https://openai.com/blog/gpt-5
      link_hash: a1b2c3...
      published_at: 2025-01-15T06:00:00Z
  [2] "Anthropic Claude 4 Release" → https://anthropic.com/news/claude-4
      link_hash: d4e5f6...
      published_at: 2025-01-14T22:00:00Z
  ...
```

#### Diff 模式（`--diff`）

```text
=== Diff: feed_payload source_id=3 ===

Entry "OpenAI Announces GPT-5":
  link_hash:  a1b2c3... (unchanged)
  title:      "OpenAI Announces GPT-5" → "OpenAI Announces GPT-5!" (changed)

Entry "New article not in DB":
  status: NOT FOUND in feed_entries (would be new discovery)
```

#### JSON 模式（`--output-format json`）

```json
{
  "replay_kind": "feed",
  "artifact_key": "3",
  "entries": [
    {
      "feed_entry_uid": "...",
      "title_raw": "...",
      "normalized_link": "...",
      "link_hash": "...",
      "db_status": "exists",
      "diff": null
    }
  ],
  "errors": []
}
```

## 5. Backfill 与 Replay 的关系

| 维度 | Replay | Backfill |
|---|---|---|
| 目的 | 验证、调试 | 补跑历史数据 |
| 数据来源 | `raw_artifacts` | 数据库 + 外网 |
| 写入 | 不写入（只读） | 写入数据库 |
| 网络 | 不需要 | 可能需要 |
| 版本 | 使用当前版本解析 | 使用当前版本处理，bump 版本号 |

Backfill 和 Replay 共享解析逻辑，但 Backfill 会实际修改数据库状态，而 Replay 是纯只读操作。

## 6. Artifact 写入时机

### 6.1 Feed Payload

```text
触发：feed crate 完成 HTTP 拉取、收到响应 body 后
条件：retention_policy 允许
写入：runtime 层调用 storage::artifact_repository.upsert()
时序：在 feed 解析之前写入（保证解析失败也有 artifact）
```

### 6.2 HTML Payload

```text
触发：extractor crate 完成详情页 HTTP 拉取后
条件：retention_policy 允许
写入：runtime 层调用 storage::artifact_repository.upsert()
时序：在正文提取之前写入
```

### 6.3 AI Raw Response

```text
触发：ai crate 收到 AI API 响应后
条件：retention_policy 允许（AI 响应建议 on_failure 或 always）
写入：runtime 层调用 storage::artifact_repository.upsert()
时序：在输出解析之前写入
```

### 6.4 写入时序的关键约束

**Artifact 写入必须在解析/处理之前完成。** 理由：如果解析过程 panic 或崩溃，artifact 已经落盘，可以通过 replay 重现问题。如果先解析后写 artifact，崩溃场景下 artifact 丢失。

## 7. 与 `rebuild-report` 的区别

`rebuild-report` 不属于 replay 系统。它从 `publish_items`（冻结快照）重建 Markdown，不涉及 `raw_artifacts`。

| 维度 | Replay | Rebuild-report |
|---|---|---|
| 数据来源 | `raw_artifacts` | `publish_items` |
| 目的 | 重新解析原始输入 | 重新渲染已冻结的快照 |
| AI 调用 | 不需要 | 不需要 |
| 输出 | 解析结果 + diff | Markdown 文件 |

## 8. 与宪法的对齐检查

- §4.3 核心不变量 7：所有外部输入允许 replay ✓
- §3.2 replay 作为正式 CLI 命令 ✓
- §5.1 失败路径：artifact 在解析前写入，保证崩溃可追溯 ✓
- §3.4 单一真相源：`raw_artifacts` 是原始输入的唯一真相源 ✓
- §6.2 退出路径：按 TTL 清理，清理事件写 `run_events` ✓
- 配置可控：四种保留策略覆盖全关到全保留 ✓
