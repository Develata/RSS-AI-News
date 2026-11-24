# release workflow 自动创建 GitHub Release

- 日期：2026-11-24
- 作者 / Agent：Claude Code（Opus 5.5，实现）+ Codex gpt-6.2-astra（独立复审，high，一轮）
- 分支：`ci/github-release`（基于 `main` `7da74bb`）
- 当前 HEAD：见分支
- 相关 commit：`397b4d7`（初版）及其后的复审修复提交（`7da74bb..ci/github-release`）
- 相关 tag / release：N/A（未推送、未打 tag、未补建旧 Release）
- 状态：`validated`（仅本地）；复审修复后未再复审

## 工作摘要

`release.yml` 原先只推 GHCR 镜像（`v0.7.0` 起的明文约定），GitHub Release 靠手建：只有 `v0.8.0`、`v0.8.1`、`v0.9.0` 有，
其余 19 个 tag 没有。Develata 决定改为由 workflow 创建。新增最后一个 job `github-release`，正文取 release report 中标记的段落，
Latest 跟随 `latest` 镜像别名。

## 影响范围

- crate / 模块：`.github/workflows/release.yml`、`.github/scripts/release-github{,-api,-fake,-test}.sh`（新）、
  `.github/scripts/release-promotion{,-test}.sh`（新子命令 `alias-version`；`is_version` 收紧）、`.github/scripts/release-registry.sh`
  （拒绝含空白的 label）、`tools/acceptance`（static lane `release-github`、release lane `release-notes`）、`.gitattributes`
- 真相源对象：N/A
- 额外影响：release 流程契约（`docs/operations/ci-and-release.md`）；release report 结构（`docs/reports/README.md`）；
  `GITHUB_TOKEN` 首次在 release workflow 中有 `contents: write`（仅 github-release job）

## 关键变更

- `github-release`（`needs: [promote-tags]`，`contents: write` + `packages: read`，与 promote-tags **共用**
  `release-promote-<repo>` group、`queue: max`）：抽取 notes → 现读 `latest` 别名版本 → 紧挨写入前核对远端 tag 仍指向本 run 的
  commit → `release-github.sh ensure <版本> <commit> <别名版本> <notes>`。
- 正文：`<!-- release-notes:start -->` / `<!-- release-notes:end -->` 两行之间（各恰好一次、独占一行，行尾单个 CR 忽略），末尾附
  `main` 上报告的 Markdown 链接；标记段内的相对链接被拒绝。release lane 跑同一抽取，因此缺标记或有相对链接的 tag 在推镜像前就
  失败；main 上的 release lane 用 workspace 版本，所以版本号与报告须同进 main。
- Latest：别名版本 = 本版本 → 本 tag 须成为 Latest；否则 `--latest=false`，且 GitHub 当前 Latest 必须已是别名版本的 tag，不是则
  写前失败并指出先补哪个 Release。
- 顺序：全部读取与检查在第一次写之前（draft / prerelease / 同 tag 多个 Release / 读错误 / Latest 不一致 → 零写入）；至多一次写
  （create，或必要时 set-latest，已是 Latest 则不写）；写后读回失败报告为"已写入、未确认"。"不存在"只由完整分页列表判定，
  列表结构不符即读错误；创建经 `gh release create --verify-tag --target <sha>`。
- `v0.10.4` 报告补上标记，其中一个相对链接改为绝对 URL（main 上 release lane 的检查对象）；"纯 bugfix patch 可不写 report"的约定
  废止（每个 tag 都要 notes 段）。R3-1 一节按各 tag 上的旧 workflow 改正门槛描述，并补上 GitHub 30 天重跑期限。

## 独立复审（Codex，`7da74bb..397b4d7`）

- 判定 **BLOCKER**：4 BLOCKER、5 SHOULD_FIX、2 NICE、1 NEEDS_VERIFICATION。逐条核实后全部处理：
  - B1 tag 核对与写入间隔过长、tag 删除后可能从默认分支重建 → 核对移到紧挨 `ensure`，`--target <sha>`；剩余几秒窗口写明只有
    tag ruleset 能根除（2026-11-24 查询本仓库无 ruleset）。
  - B2 promote-tags 不持 Release job 的锁 → 共用 concurrency group。
  - B3 `latest=false` 时旧 tag 仍占 Latest 却报成功（已复现：exit 0）→ 写前核对 GitHub Latest = 别名版本。
  - B4 写后读回失败仍称"nothing written" → 读取全部前移，按阶段区分消息，已是 Latest 时不写。
  - S5 畸形列表被当作不存在 → jq 结构校验（`error()` 使 gh 退出 1，已实测）+ stub-gh 适配器测试。
  - S6 label 尾随空白被字段拆分折叠 → registry 适配器拒绝含空白的 label。
  - S7 CRLF 检出 → 去掉行尾 CR，`.gitattributes` 钉 `docs/reports/releases/*.md` 与 `.github/scripts/*.sh` 为 LF。
  - S8 多行参数可通过 `grep -E` 版本校验 → `is_version` / `check_tag` 先拒绝非法字符（两处脚本）。
  - S9 R3-1 旧 run 门槛与重跑期限描述过宽 → 已改正（既有文档问题）。
  - N10 报告 README 适用范围与镜像 tag 示例 → 已改；标记为保留字写入规范。
  - N11 链接行未被 GFM 自动链接、相对链接失效 → 显式 Markdown 链接，拒绝相对链接。
- 修复后未再做独立复审。

## 验证与验收

### 自动化验证

- `sh` / `/bin/dash` / `dash` + `mawk`（runner 的组合，本机经 Homebrew 装了 mawk 1.3.4）下：`release-github-test.sh` 56 例、
  `release-promotion-test.sh`（含 `alias-version` 与 label 空白用例）全部通过
- 变异核对（临时副本，dash + mawk）：去掉 Latest 不一致检查、已是 Latest 时跳过写入、写前读 Latest、CR 去除、相对链接拒绝、
  阶段消息、严格版本校验、适配器的 `--target` / `--paginate` / 列表结构校验、registry 的空白检查，各自至少一例失败。
  注：多行版本号用例在 macOS 的 BSD awk 下即使去掉守卫也会以 2 退出（awk 拒绝含换行的 `-v`），只在 mawk 下能区分
- `shellcheck -s sh .github/scripts/*.sh`、`actionlint .github/workflows/release.yml`、`git diff --check`：通过
- `cargo fmt --all -- --check`、`CARGO_PROFILE_RELEASE_STRIP=none cargo acceptance run --lane static --lane release`：通过
  （exit 0，修复后重跑；static 含 `release-github`，release 含 `release-notes`，对 `v0.10.4` 报告）
- 未跑：workspace / sqlite / postgres / docker lane（本次未改产品代码、迁移与镜像）

### 手工验收

- 真实适配器只读调用（本仓库，2026-11-24，含新的结构校验过滤器）：`find v0.9.0` → `false false`；`find v0.10.4` → 空；
  `latest` → `v0.9.0`；`gh api … --jq 'error("boom")'` → 退出 1
- 未执行：真实 `gh release create` / `gh release edit --latest`；job 在真实 tag 上运行；`packages: read` 的 `GITHUB_TOKEN`
  读别名（promote-tags 用的是 `packages: write`）

## 结果

- 可合并；首个真实 tag 时盯 github-release job（同时也是新版 release.yml 首次在真实 tag 上运行）

## 风险与后续事项

- 两个 tag 相隔几分钟推送时，共享的 FIFO 队列可能排成 `promote-tags(B)` → `github-release(A)` → `github-release(B)`：较低的 A
  因"GitHub Latest 还不是别名版本"失败（零写入），等 B 的 Release job 成功后重跑 A 的 github-release 即可。这是 B3 修复的代价，
  未放宽检查。
- 当前 GitHub Latest 是 `v0.9.0`；`latest` 别名在 `0.10.4`（2026-11-24 匿名读 GHCR：`latest` 与 `0.10.4` 的 digest 同为
  `sha256:5dc83692…`）。下一个 tag 若是最高版本（如 `v0.10.5` / `v0.11.0`）会直接成为
  Latest；若在合入后先发一个低于 `0.10.4` 的补丁，它会按 Latest 不一致失败，需先补建 `v0.10.4` 的 Release。
- 旧 tag（`v0.1.0`…`v0.7.1`、`v0.9.1`、`v0.10.0`…`v0.10.4`）的 Release 补建是对外发布，需 Develata 单独决定；补建时按版本升序、
  只给最高版本标 Latest，且不得重跑旧 release run（R3-1）。除 `v0.10.4` 外，旧报告没有 notes 标记。
- tag 核对与创建之间的几秒窗口：只有 `v*` tag ruleset（禁止更新、删除）能根除，是否启用由 Develata 决定。
- 别名被"保留在更高版本"（更高 tag 已删除）时，GitHub 的 Latest 必须仍是那个版本，否则低版本的 run 失败。

## 给下一位 Agent 的备注

- 入口：`.github/scripts/release-github.sh` 头注释（适配器契约与策略）、`docs/operations/ci-and-release.md` 的 github-release 条目
