# release workflow 自动创建 GitHub Release

- 日期：2026-11-24
- 作者 / Agent：Claude Code（Opus 5.5）
- 分支：`ci/github-release`（基于 `main` `7da74bb`）
- 当前 HEAD：见分支
- 相关 commit：见 `7da74bb..ci/github-release`
- 相关 tag / release：N/A（未推送、未打 tag、未补建旧 Release）
- 状态：`validated`（仅本地）

## 工作摘要

`release.yml` 原先只推 GHCR 镜像（`v0.7.0` 起的明文约定），GitHub Release 靠手建：只有 `v0.8.0`、`v0.8.1`、`v0.9.0` 有，
其余 19 个 tag 没有。Develata 决定改为由 workflow 创建。新增最后一个 job `github-release`，正文取 release report 中标记的段落，
Latest 跟随 `latest` 镜像别名。

## 影响范围

- crate / 模块：`.github/workflows/release.yml`、`.github/scripts/release-github{,-api,-fake,-test}.sh`（新）、
  `.github/scripts/release-promotion{,-test}.sh`（新子命令 `alias-version`）、`tools/acceptance`（static lane `release-github`、
  release lane `release-notes`）
- 真相源对象：N/A
- 额外影响：release 流程契约（`docs/operations/ci-and-release.md`）；release report 结构（`docs/reports/README.md`）；
  `GITHUB_TOKEN` 首次在 release workflow 中有 `contents: write`（仅 github-release job）

## 关键变更

- `github-release`（`needs: [promote-tags]`，`contents: write` + `packages: read`，跨 tag 串行 `release-github-<repo>`、
  `queue: max`）：核对远端 tag 仍指向本 run 的 commit → 抽取 notes → 现读 `latest` 别名版本 → `release-github.sh ensure`。
- 正文：`<!-- release-notes:start -->` / `<!-- release-notes:end -->` 两行之间（各恰好一次、独占一行），末尾附 `main` 上报告的链接。
  release lane 跑同一抽取，因此缺标记的 tag 在推镜像前就失败；main 上的 release lane 用 workspace 版本，所以版本号与报告须同进 main。
- Latest：别名版本 = 本版本才 `--latest`，否则显式 `--latest=false`；不沿用 promote-tags 的输出（可能已过期）。
- 幂等与失败面：已发布的 Release 不改标题与正文，只在应标时补 Latest；draft / prerelease / 同 tag 多个 Release → 失败、零写入；
  "不存在"只由完整分页列表判定（不看 404）；创建只经 `gh release create --verify-tag`（不会新建 tag）；写后读回状态与 Latest。
- `v0.10.4` 报告补上标记（main 上 release lane 的检查对象）；"纯 bugfix patch 可不写 report"的约定废止（每个 tag 都要 notes 段）。

## 验证与验收

### 自动化验证

- `sh` / `/bin/dash` / `dash` + `mawk`（runner 的组合，本机经 Homebrew 装了 mawk 1.3.4）下：`release-github-test.sh` 28 例、
  `release-promotion-test.sh`（含 4 例 `alias-version`）全部通过
- 变异核对（临时副本）：去掉 Latest 判定、重复 Release 检查、状态读回、Latest 读回、draft 拦截、首行修剪、标记顺序检查，
  各自至少一例失败
- `shellcheck -s sh`（新旧脚本）、`actionlint`（`.github/actionlint.yaml` 已忽略 `queue` 键）：通过
- `cargo fmt --all -- --check`：通过
- `CARGO_PROFILE_RELEASE_STRIP=none cargo acceptance run --lane static --lane release`：通过（exit 0；static 含新步骤
  `release-github`，release 含新步骤 `release-notes`，对 `v0.10.4` 报告）
- 未跑：workspace / sqlite / postgres / docker lane（本次未改产品代码、迁移与镜像）

### 手工验收

- 真实适配器只读调用（本仓库，2026-11-24）：`find v0.9.0` → `false false`；`find v0.10.4` → 空；`latest` → `v0.9.0`
- 未执行：真实 `gh release create` / `gh release edit --latest`；job 在真实 tag 上运行；`packages: read` 的 `GITHUB_TOKEN`
  读别名（promote-tags 用的是 `packages: write`）

## 结果

- 可合并；首个真实 tag 时盯 github-release job（同时也是新版 release.yml 首次在真实 tag 上运行）

## 风险与后续事项

- 旧 tag（`v0.1.0`…`v0.7.1`、`v0.9.1`、`v0.10.0`…`v0.10.4`）的 Release 补建是对外发布，需 Develata 单独决定；补建时按版本升序、
  只给最高版本标 Latest，且不得重跑旧 release run（R3-1）。除 `v0.10.4` 外，旧报告没有 notes 标记。
- 别名被"保留在更高版本"（更高 tag 已删除）时，本 tag 不标 Latest，GitHub 的 Latest 留在原处。

## 给下一位 Agent 的备注

- 入口：`.github/scripts/release-github.sh` 头注释（适配器契约与策略）、`docs/operations/ci-and-release.md` 的 github-release 条目
