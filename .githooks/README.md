# 仓库 Git Hooks

本目录包含项目的 git hooks，与 `core.hooksPath` 绑定使用。脚本作为仓库内文件
被追踪，所以每个克隆出来的副本都需要一次性 opt-in。

## 启用

仓库根执行一次：

```bash
git config core.hooksPath .githooks
```

之后 `git commit` 会自动触发 `.githooks/pre-commit`。

## pre-commit 做什么

只在 staged 改动涉及 `*.rs` / `Cargo.toml` / `Cargo.lock` / `.cargo/config.toml`
时触发。两步：

1. `cargo fmt --all -- --check`（约 5–10 秒）
2. `cargo clippy --workspace --all-targets --jobs 1 -- -D warnings`（首次约 30
   秒~2 分钟；后续增量编译会快很多）

这与 CI 的硬门槛一致，本地通过基本等价于 CI 通过。

## 跳过策略

- **跳过 clippy（保留 fmt）**：`PRE_COMMIT_SKIP_CLIPPY=1 git commit ...`。适合
  快速迭代修复期、知道 clippy 会因半成品报警。**push 前必须再跑一次完整
  clippy**（CI 不会放过）。
- **完全跳过 hook**：`git commit --no-verify`。仅用于纯文档误触或调试场景，
  默认不应使用。

## 平台说明

- Linux / macOS：标准 bash，开箱即用。
- Windows：Git for Windows 自带的 MSYS2 bash 会自动执行该脚本；仓库克隆出来时
  `pre-commit` 可能不带可执行位。如发现 hook 不触发，执行：

  ```bash
  git update-index --chmod=+x .githooks/pre-commit
  ```

  （已在提交时设过的话，clone 会保留该位。）

## 旁路场景

如果你确实需要绕过 hook（紧急修补、CI 自己跑过、纯文档改动），用 `--no-verify`
而不是修改 hook 文件本身。这样不会污染共享配置。
