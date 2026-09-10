# Boundary & Resource Hardening — review and fix

日期：2026-09-10。审查对象为基于 `95ae450` 的整批工作区改动；用户授权直接修复。未commit、push、发布或部署。

## 发现与修复

| 严重性 | 问题与复现证据 | 修复及结果 |
|---|---|---|
| P1 | `--category test reindex --target categories` 把过滤后的配置与全库sources比较；回归测试中other分类由active变成archived | reindex升级全局规则，所有target/abort遇到`--category`均在配置/数据库访问前报参数错误，exit 2；不引入半库规则版本或静默忽略参数 |
| P1 | AI在JSON解析前替换API key，`\u0073k-test`解析后重新暴露；GitHub错误消息没有遮蔽配置token，转义回显同样进入错误对象 | 两个provider适配边界均在JSON解码后遮蔽已知凭据，纯文本错误也处理；保留原有HTTP/retry分类 |
| P1 | `RunEventEmitter`仅过滤context，独立message列仍写入URL userinfo/query和Authorization原值 | 发射器统一过滤message与context，防止CLI最终显示脱敏之前已经把秘密落库 |
| P2 | remote deadline仅包住target；started事件或快照读取阻塞时，1秒lease的flow在2秒外层测试deadline仍未结束 | started事件与快照准备共用原deadline；超时按retryable处理并释放已知claim。批次测试覆盖第一条准备完成、第二条阻塞，确认两条记录释放且文章状态不前移 |
| P2 | context预览按原字符串字节数截断，却忽略再次JSON转义；实际输出7045字节，超过4096上限 | 扣除envelope开销，为预览第二次编码预留最多两倍字节并保持UTF-8边界；转义字符、中文/emoji和ASCII回归均通过 |

分类说明：deadline覆盖不全和AI转义绕过是上一轮实现遗漏；reindex过滤归档、event message/context边界及GitHub回显是审查过程中确认的既有问题。修复均在原有owner内完成，没有增加依赖或改变schema。

附带维护：删除3条已失效的redaction吞错豁免，校准AI豁免行号；发布失败释放逻辑归并，避免单条/批量/准备超时各自复制处理；修正文档地图中artifact文件后端已生效的不实描述。

## 回归证据

以下测试先在修复前失败，再在修复后通过：

- `reindex_category_filter_cannot_archive_unselected_categories`：实际观察到other源被归档。
- `invoke_redacts_json_escaped_provider_api_key`：错误Display/Debug包含解析后的API key。
- `provider_error_does_not_expose_configured_token`：错误Display/Debug包含GitHub token。
- `emit_redacts_message_as_well_as_context`：message包含敏感值，context已脱敏。
- `publish_remote_deadline_also_bounds_preparation`：2秒外层timeout到期；修复后同时验证单条与部分已准备的批次。
- `truncated_context_respects_final_serialized_byte_limit`：最终JSON为7045字节；修复后不超过4096且仍为合法JSON。

另有 `reindex_rejects_filtered_global_targets_before_opening_storage` 覆盖link_hash/content_hash/all的参数边界。

## 验证

最终结果和生产文件SHA-256见 [review-validation.json](review-validation.json)。本页使用当前最终代码的验证，不把上一轮测试数字挪作本轮证据。

| 状态 | 检查 | 结果 |
|---|---|---|
| PASS | `cargo fmt --all -- --check` | 通过 |
| PASS | `cargo clippy --workspace --all-targets --all-features --jobs 1 -- -D warnings` | 通过 |
| PASS | `cargo test --workspace --all-features --no-fail-fast -j3` | 823通过、0失败、62 ignored；原始日志含1个隔离子进程重复执行，passed合计824 |
| PASS | `cargo build --workspace --all-features --locked -j3` | 通过 |
| PASS | `cargo build --release --locked -j3` | 最终release生成 |
| PASS | SQLite/release acceptance | 当前release的migration/query/CLI版本验收，见[review-acceptance.json](review-acceptance.json) |
| PASS | 吞错策略与依赖安全策略 | 11条保留豁免；rsa仍无active path |
| PASS | `git diff --check`、审查文档本地链接 | 通过 |
| NOT RUN | PostgreSQL、Docker容器验收 | 无对应服务/可用Docker |
| NOT RUN | 性能复测 | 本轮未重跑6个ignored性能fixture，不声称新性能结果 |

62项ignored包括6个perf fixture及56项原有ignored测试。上一轮曾单独运行perf，不计入本轮823项。Cargo.lock与依赖feature本轮未改动，完整RustSec扫描沿用上轮同锁文件证据；本轮复验active-rsa门禁，没有声称重新完成零告警扫描。

## 边界与交付

- 原报告的性能数据为审查前测量快照，本轮没有重新测量性能，不将旧数字当作当前源码的重新验证。
- 发布预算从claim前开始计时；准备与远端调用受timeout限制。claim与收尾数据库写入仍采用既有失败/lease恢复机制，数据库不可用时不能保证函数整体在lease内返回。同步渲染也不能被Tokio抢占。
- 网络取消不能撤销远端已完成的提交，继续依靠既有幂等发布与恢复路径。
- PostgreSQL服务及Docker在当前环境不可用；相关测试能编译不代表平台验收已执行。
- 契约同步：[storage/reindex](../../plan/05-storage.md)、[AI](../../plan/03-ai.md)、[publish](../../plan/04-publish.md)、[observability](../../plan/07-observability.md)、[reindex验收](../../acceptance-cases/commands/reindex.md)。
