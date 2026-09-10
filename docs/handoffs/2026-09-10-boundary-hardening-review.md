# Boundary Hardening 审查与修复

- 日期：2026-09-10
- 作者 / Agent：Codex
- 分支：main
- 当前 HEAD：95ae450
- 相关 commit：pending，未提交
- 相关 tag / release：基于v0.7.1，无新发布
- 状态：`validated`（本地）；PG/Docker未验收

## 工作摘要

用户要求“review and fix”。审查上一轮工作区改动，修复5类已复现的问题，未扩大架构或引入依赖。

## 影响范围与关键变更

- CLI/storage边界：reindex是全局规则升级，拒绝`--category`并在访问配置/数据库前返回exit 2，防止误归档其他分类。
- AI/publish：在JSON解析后脱敏provider回显的当前凭据，覆盖JSON转义。
- runtime/events：独立message列也脱敏；context预算覆盖最终JSON转义，修复7045字节越过4096字节上限的问题。
- runtime/publish：准备阶段与远端请求共享deadline，超时可重试释放；部分准备完成的批次不会遗漏释放。
- schema、迁移、依赖未改。plan03/04/05/07、reindex验收、地图已同步；3条失效吞错豁免移除。

## 验证与验收

- PASS：6条复现用例红→绿；另增全局reindex参数边界测试。
- PASS：fmt；严格workspace/all-targets/all-features Clippy；全量测试823通过、0失败、62 ignored（原始日志另有1项隔离子进程重复）。
- PASS：workspace all-features build、locked release build、使用最终release的SQLite/release acceptance。
- PASS：吞错策略、active-rsa依赖策略、git diff空白检查与新增文档链接。
- NOT RUN：PostgreSQL integration/migration与Docker容器验收，当前环境不可用。
- NOT RUN：性能复测；原报告数字保留为审查前快照，不作为当前源码的重新测量。

## 结果与后续

全部5类确认问题已修复，当前代码可供提交审查；未commit/push/发布/部署。发布准备与网络阶段有deadline，claim/收尾数据库写入仍依赖既有失败和lease恢复机制；数据库不可用时不能保证函数整体在lease内结束。远端取消仍不能撤销已完成提交。

完整严重性排序、复现证据、最终文件摘要和验收记录见[review report](../reports/boundary-resource-hardening/review.md)。
