# CI 与 Release 流程

## CI workflow

文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)。

触发：push 到 `main` 或 PR target `main`。

```text
concurrency: ci-${{ workflow }}-${{ ref }}   # 同分支后续 push 自动取消前一次
```

三个并行 job（无 `needs:` 依赖链）。job 按共享的编译产物分组：前两个调用 Rust 验收矩阵的 lane，
CI 与本地 pre-tag 矩阵执行同一份检查（见 [./acceptance-matrix.md](./acceptance-matrix.md)）。runner 不 fail-fast，
同一 job 内前一个 lane 失败后后续 lane 仍执行并报告：

| Job | 命令 / 验证项 | 本地复现 |
|---|---|---|
| `test` | `cargo acceptance run --lane static --lane workspace`：fmt、Clippy `-D warnings`、rustdoc `-D warnings`、swallowed-error、dependency policy、tooling boundary、acceptance-case 状态；`cargo build/test --workspace --locked`（SQLite） | 同命令，或 `cargo test -p <crate>` |
| `test-pg` | `cargo acceptance run --lane postgres --lane sqlite --lane release`（PG service + `DATABASE_URL`）：storage `--ignored`（只跑 PG / Docker 用例；普通 storage 用例已在 workspace lane 跑过，且不读 `DATABASE_URL`）+ CLI migrate run/check；三条 lane 共用一次 release build：SQLite migrate run/check、recent-entries 与 CLI JSON 输出契约（product smoke 会剥离继承的 `DATABASE_URL`，仍走 SQLite）；版本 / lockfile / README / binary identity | `DATABASE_URL=… cargo acceptance run --lane postgres --lane sqlite --lane release`（见 [./postgres-deployment.md](./postgres-deployment.md)） |
| `docker-build` | `docker/build-push-action` 多 stage 构建（不 push）+ 容器 smoke；保留该形式以维护 release 复用的 GHA buildx cache（`scope=runtime`） | `cargo acceptance run --lane docker`，或 `docker build -f docker/Dockerfile .` |

缓存：

- Rust job 用 `Swatinem/rust-cache`（`shared-key` = `test` / `test-pg`），只有 `main` 保存缓存（PR ref 下的缓存 `main`
  读不到）。release `verify-*` 只读这些 key，因此 release.yml 的 `CARGO_*` / `RUST_*` env 必须与 ci.yml 一致（它们进 key 的
  environment hash）。environment hash 保持开启：rust-cache v2 关闭它会连 Cargo.lock hash 一起去掉，key 永不变化、缓存
  永不刷新。代价是 runner 镜像升级其预装 stable Rust 时（如 1.98.1 → 1.99.0），全部 Rust 缓存失效一次。
- Docker builder 用 cargo-chef：依赖编译是按 manifest-only recipe（Cargo.lock + manifests，workspace 成员版本被屏蔽）
  缓存的普通 layer，经 `cache-to: type=gha` 持久化（与 rust-cache 一致，只有 `main` 写入；PR 只读，release 的 tag
  run 只读 `main` 的 `scope=runtime`）；源码改动只重编 workspace crates。`RUN --mount=type=cache`
  的内容不会被 `type=gha` 导出，所以不用它承载依赖缓存。
- GitHub 会驱逐 7 天未访问的缓存；停更超过一周后的首个 run 必然冷启动（约多 10–15 runner-min），这是预期行为。

历史（2026-05-03 → 2026-10-16，109 runs / 1006 runner-min）：`test-pg` 在 55/70 次 CI 中是关键路径；2026-09-10
Dockerfile 改用 cache mount 后，改代码的 docker job 中位数由 2.2 min 升到 5.3 min（依赖全量重编）；上述 cargo-chef layer 用于消除这一回退（本地已验证，CI 实测待首次 warm run 确认）。
旧基线（2026-09-11 前 5 次运行中位数，五 job 布局）：整次 CI 326s，`test-pg` ≈282s，其中 storage PG 测试 181s、
release build 52s。PG 测试保持 `--test-threads=1`：本地实测 1 线程 108s、4 线程 99s，收益不足以承担连接争用风险。

权限与超时：`ci.yml` 顶层 `permissions: contents: read`（CI 不写仓库或 registry；`audit.yml` / `maintenance.yml` 同样只读，
`release.yml` 只给两个镜像 job `packages: write`）。每个 job 都设 `timeout-minutes`（默认 360 min），约为实测耗时的 3 倍：
`test` 30（实测 8–11 min）、`test-pg` 30（6–10.5 min）、`docker-build` 25（3.5–8 min）、audit / security 10、perf 60；
release 的 verify-* 与 `publish-image` 30、`verify-audit` 与 `promote-tags` 10。

CI 通过是 PR 合入 `main` 的硬门槛。文档变更也不豁免（hooks 触发 `cargo fmt`）。

## 依赖安全审计（cargo audit）

两个 workflow 共用 composite action [`.github/actions/cargo-audit`](../../.github/actions/cargo-audit/action.yml)：先装固定版本、
校验 SHA-256 的 cargo-audit 预编译二进制，再跑 `.ci/check_dependency_security_policy.sh`，最后跑不带参数的 `cargo audit`。

- [`.github/workflows/audit.yml`](../../.github/workflows/audit.yml)：PR target `main` 与 push 到 `main`，但只在改动可能影响依赖图或
  审计链的路径时触发（`Cargo.lock`、任意 `Cargo.toml`、`.cargo/**`、`rust-toolchain.toml`、依赖策略脚本、该 action、`audit.yml`
  与 `maintenance.yml`）。用途是让锁文件回退在合入前就报红（v0.10.0 的 PR #2 就是这样漏过去的）。未改这些路径的 PR 不跑它，所以
  针对未变依赖图的新 advisory 不会让无关 PR 变红。过滤按文件路径判断，受 GitHub 的 diff 上限约束（超大 diff 可能漏触发或无条件触发）；
  `main` 没有 branch protection，它与 CI 一样是约定的合入门槛，不是 required check。
- [`.github/workflows/maintenance.yml`](../../.github/workflows/maintenance.yml) 的 `security` job：每周一定时（也可手动触发），
  负责发现针对未变依赖图的新 advisory。

接受的 advisory 统一列在 [`.cargo/audit.toml`](../../.cargo/audit.toml)，每条写明理由与复查条件；不要在命令行加 `--ignore`。
失败条件是未接受的 vulnerability；unmaintained、yanked 等 warning 只报告、不失败（cargo-audit 默认行为，与改动前一致）。
依赖策略脚本用 `cargo tree --workspace --target all` 检查任何版本的 rsa 在所有成员、所有平台上都没有活跃路径。
`.cargo/audit.toml` 不再豁免 RUSTSEC-2023-0071（rsa，修复版本为 >= 0.10.0）；受影响版本的 rsa 哪怕只以 inactive 形式回到
`Cargo.lock`，`cargo audit` 也会失败。
本地复现：在仓库根目录运行 `bash .ci/check_dependency_security_policy.sh && cargo audit`（cargo-audit 从当前目录读取该配置）。

## Action 与镜像固定（供应链）

- 所有第三方 action（`actions/*`、`docker/*`、`Swatinem/rust-cache`、`dtolnay/rust-toolchain`）都按完整 commit SHA 引用，
  行尾注释写明对应版本（`# v6.1.0` 等）；`dtolnay/rust-toolchain` 没有版本 tag，固定为某个 `master` commit，注释写日期，
  工具链版本仍由 `toolchain:` 输入给出。带 `packages: write` 的 release job 因此不会执行被移动的 tag 指向的新代码。
- `docker/Dockerfile` 的基础镜像按 index digest 固定（`rust:<toolchain>-slim-bookworm@sha256:…`、`debian:bookworm-slim@sha256:…`）。
- [`.github/dependabot.yml`](../../.github/dependabot.yml) 覆盖 `cargo`、`github-actions` 与 `docker`（`/docker`），三者同样每月、
  最多 2 个 PR、单组合并、不自动 rebase、忽略 major。`rust` 镜像的版本更新被忽略（工具链升级要同时改全部 pin，见
  static lane 的 `toolchain-pins` 检查）。Docker Official Images 只为仍在支持列表中的 tag 跟随基础镜像重建；`1.97.0-slim-bookworm`
  这类 patch tag 在更新的 Rust 发布后即退出该列表（2026-11-22 核对 `library/rust`：只剩 1.100.x），之后不会再有新 digest，
  Dependabot 也就不会为它提 digest PR。builder 镜像的安全更新因此随**工具链升级**一起做（同一次受控变更里改 channel、
  workflow、Dockerfile 的版本与 digest）；它只是构建阶段，不进入运行时镜像。`debian:bookworm-slim` 运行时镜像的 digest 更新
  照常由 Dependabot 提出（R3-13）。
- composite action `.github/actions/cargo-audit` 不引用第三方 action；它下载的 cargo-audit 二进制按 SHA-256 校验。

## Release workflow

文件：[`.github/workflows/release.yml`](../../.github/workflows/release.yml)。

触发：push tag `v*`（如 `v0.4.0` / `v0.4.0-rc.1`）。**不**触发于 branch push / PR。

```yaml
permissions:
  contents: read    # 顶层只读；只有 publish-image / promote-tags 两个 job 声明 packages: write，
                    # 只有 github-release 声明 contents: write（另加 packages: read）
env:
  RUST_TOOLCHAIN: "1.97.0"     # 与 ci.yml 保持一致
  IMAGE_NAME: ghcr.io/${{ github.repository }}   # 自动 lowercase
```

Jobs：

- `verify-test` —— `cargo acceptance run --lane static --lane workspace`；
- `verify-release` —— `cargo acceptance run --lane sqlite --lane release --expected-version "${GITHUB_REF_NAME#v}"`：tag 版本必须与
  workspace / lockfile / README / binary 一致。两者合起来即 `local` profile 的四条 lane，在 tag commit 上通过；
  它们只读 ci.yml 在 `main` 上为同一 commit 保存的缓存（`test-*` / `test-pg-*`），未命中只是变慢。
- `verify-postgres` —— `cargo acceptance run --lane postgres`，带与 ci.yml `test-pg` 相同的 PostgreSQL 16 service 与
  `DATABASE_URL`，读 `test-pg-*` 缓存；
- `verify-audit` —— 复用 composite action `.github/actions/cargo-audit`（依赖策略脚本 + `cargo audit`），在 tag 的依赖图上
  拦下自上次 `main` 运行以来新发布的 advisory；
- `publish-image`（`needs: [verify-test, verify-release, verify-postgres, verify-audit]`）—— 构建并推送 runtime + scheduler
  两套镜像，**只**打版本 tag（`X.Y.Z` / `X.Y.Z-scheduler`）；任一 verify 失败则不推镜像。版本 tag 不可变（R3-7）：先用
  `release-promotion.sh existing` 读 registry，已存在且 `org.opencontainers.image.revision` 等于本次 commit 的版本 tag 直接复用其
  digest、不重建（重建会因 `org.opencontainers.image.created` 变化得到新 digest），只补建缺失的一侧；已存在但来自别的 commit
  （或版本 label 不符）则失败、不覆盖。job 级 `concurrency: release-publish-<repo>-<tag>`，同一版本的重跑串行。
  第一次推送之前（持有该版本的锁时）先用 `release-promotion.sh tag-commit` 核对 `git ls-remote origin refs/tags/vV
  'refs/tags/vV^{}'`：远端 tag 解引用后的 commit 必须等于本次 run 的 `GITHUB_SHA`，tag 缺失或已指向别的 commit 即停止、不推任何
  镜像（R3G-B4）。原因：重跑沿用原始 `GITHUB_SHA`，`vV` 若已移到新 commit，旧 run 会先占用不可变的版本 tag，使新 commit 的
  正常 run 无法发布。核对与推送之间仍有一个很短的窗口（tag 恰在其间移动），无法在 workflow 内消除；tag 保护规则才能根除。
- `promote-tags`（`needs: [publish-image]`）—— 用
  [`.github/scripts/release-promotion.sh`](../../.github/scripts/release-promotion.sh) `reconcile` 把别名**对齐到最高的已发布
  stable 版本**，与触发它的是哪个 tag 无关（R3-2 / R3-3 / R3-4 / R3-22）：
  - 输入：`git ls-remote --tags`（annotated tag 取 `^{}` 行的 commit）。只认 `vX.Y.Z`，每段为 `0` 或无前导零的数字，位数不限（与 Rust 验收的
    `validate_version` 同一规则）；比较时逐段先比位数、再按数字串比较，不经浮点数，超过 2^53 也精确（R3G-S8）。
  - "已发布"：`V` 与 `V-scheduler` 两个版本 tag 都在 registry 中，版本 label 分别**恰为** `V` 与 `V-scheduler`（metadata-action
    生成的值，不再剥掉 `-scheduler` 后比较，因此两个 tag 互相指错的镜像不算发布，`existing` 也不会复用，R3G-B2），revision label 都
    = 该 tag 的 commit；只有一侧镜像
    的版本（发布未完成或失败）跳过，取下一个更低的版本。digest 一律从 registry 按版本 tag 读，不信任上一 job 的输出。
  - 目标：`latest[-scheduler]` → 全局最高已发布版本；`X.Y[-scheduler]` → 触发版本所在线与 `latest` 所在线各自的最高已发布版本。
    因此较低版本的 run 也会把 `latest` / `2.0` 推到更高版本（排队中被挤掉的高版本 run 不再造成漏晋升）。
  - 不回退：别名当前的 `org.opencontainers.image.version` label 高于目标时保持不动（例如更高的 tag 被删除后重跑旧 tag）。
    别名同样按角色校验 label（runtime 别名恰为 `C`、scheduler 别名恰为 `C-scheduler`）；保持不动的较高一对还须 revision 相同，
    且 digest 分别等于 `C` / `C-scheduler` 精确版本 tag 的 digest（精确 tag 缺失也算失败），否则 job 失败、零写入（R3G-B2）。
    先读后写（R3G-B3）：本次涉及的**全部**别名（每条线的 runtime 与 scheduler）在第一次写入前读完并校验；存在但没有合法版本
    label 的别名、任何非"tag 不存在"的读取错误、或一对别名按计划会停在不同版本，都使 job 失败且**零写入**（fail closed）。
  - 写入：每条线先写 runtime 别名再写 scheduler 别名，`docker buildx imagetools create --prefer-index=false`（单一来源按原样
    复制，别名 digest = 版本 tag digest；不加该参数时普通 manifest 会被包成新 index、digest 改变），写后逐个读回比对 digest。
    两次写之间被取消 / 超时 / 第二次写失败时，别名可能暂时一新一旧（registry 写入无法原子化）；**重跑该 promote-tags job**
    会幂等补齐（R3G-S3）。每次 reconcile 只处理三组别名：`latest`、触发版本所在的 `X.Y` 线、`latest` 所在的 `X.Y` 线，所以
    别的 release 的 run 只在受影响的别名属于这三组时才能代为补齐：`latest` 与其所在线可由任一 release 的 run 补齐，其他
    `X.Y` 线必须重跑**该 `X.Y` 线**某个 release 的 promote-tags。最后若 runtime 与 scheduler 别名停在不同版本，job 失败。
    中断后若较高的 `vC` tag 又被删除，低版本的重跑会看到一侧别名停在 `C`（高于目标，保持不动）、另一侧仍旧，于是按"会停在不同
    版本"零写入失败，不能自动补齐。恢复前提是 `C` / `C-scheduler` 两个精确版本镜像仍在 registry：把 `vC` 按原 commit 重新推送
    （`git push origin <commit>:refs/tags/vC`；commit 不变，版本 tag 的 revision 校验仍成立），推送会触发 `vC` 的新 release run，
    其 publish-image 复用已有版本 tag、promote-tags 补齐别名；或由维护者用
    `docker buildx imagetools create --prefer-index=false -t <image>:<alias>-scheduler <image>@<C-scheduler 的 digest>` 把落后的
    一侧对齐到 `C`，再重跑该别名所在 `X.Y` 线（`latest` 时任一）现行 release 的 promote-tags 复核。
  - 串行：job 级 `concurrency: release-promote-<repo>`、`cancel-in-progress: false`、`queue: max`（最多 100 个等待中的 job，按
    FIFO；默认的单个 pending 名额会让后到的 run 取消正在等待的 run）。github-release job 共用这个 group（见下），每个 run 在其中
    排两个 job。`queue` 属于 GitHub 的 job concurrency schema，actionlint
    1.7.12 尚不认识，见 `.github/actionlint.yaml`。`latest` 的正确性不依赖排队：每次 reconcile 都把它对齐到最高已发布版本。
    排队溢出或被人工取消的 run 若是某条 `X.Y` 线的唯一晋升，则必须重跑**该 `X.Y` 线**某个 release 的 promote-tags；别的线的
    run 不会处理它（R3G-S3）。
  - Buildx CLI 由 `setup-buildx-action` 固定为 `v0.38.2`（两个镜像 job 相同）。
  - 测试：static lane 的 `release-promotion` 步骤用文件后端的假 registry（`release-registry-fake.sh`）离线覆盖上述规则，含低版本 run
    晋升最高版本、删除更高 tag 不回退、别名 label 校验、读错误零写入、scheduler 写失败后重跑补齐、重跑复用版本 tag。真实适配器
    `release-registry.sh`（curl + imagetools + jq）的"缺失"判定**只**看精确 tag 的 manifest 存在性请求
    `HEAD /v2/<name>/manifests/<tag>`（OCI distribution-spec v1.1.1 "Checking if content exists in the registry"：存在 → `200`，
    不存在 → `404`），按 HTTP 状态码判定，不匹配报错文本（R4-2）：`404` → tag 不存在（退出 3）；`200` → 用响应头
    `Docker-Content-Digest` 固定 digest，之后的 label 读取一律走 `<image>@<digest>`（`imagetools inspect --format '{{json .Image}}'`），
    其任何失败（含 `not found`）都是读错误；其他状态（`401` / `403` / `429` / `5xx` / `3xx`）、缺少或格式不对的 digest 头、token
    请求失败、curl 传输错误也都是读错误（退出 1，job 失败、零写入）。不再用 `imagetools inspect --raw` 判定缺失：Buildx v0.38.2
    `Resolver.Get` 先解析 tag（HEAD）、再按返回的 digest GET manifest，第二个请求的 `404` 同样报 `not found`，原实现会把"tag 存在、
    按 digest 读失败"误判为缺失，`existing` 输出空 digest、随后 `push: true` 覆盖已有版本。认证：首个 HEAD 不带凭据；registry 回
    `401` + `WWW-Authenticate: Bearer realm=…,service=…,scope=…` 时，按 distribution token 认证流程以 Basic 凭据
    `REGISTRY_USERNAME` / `REGISTRY_PASSWORD`（`release.yml` 在 `existing` 与 `reconcile` 两步传入 `github.actor` 与
    `GITHUB_TOKEN`，即 `docker/login-action` 用的同一凭据）向 https realm 取 token，再带 bearer token 重发 HEAD。被要求认证却没有
    这两个变量、realm 不是 https、或质询不是 Bearer 时直接失败（匿名对私有包的回答不能证明缺失）；同理，设置了这两个变量时，
未经质询的 `404` 也是读错误，"缺失"只认带 token 的 HEAD 的 `404`（无凭据时才接受无认证 registry，如本地 `registry:2`，的直接回答）。凭据与 token 经 `curl -K -`（stdin）
    传入，不进 argv，也不写进日志或 step summary。`Accept` 列出全部四种 manifest / index 类型：本地 `registry:2` 实测，只接受 Docker
    类型时 HEAD 一个 OCI index 返回 `404`。选择 curl 而非读取 `~/.docker/config.json`：后者依赖 runner 有无 credential helper，
    step `env` 传凭据更直接。
    测试：同一测试脚本用 stub `curl` + stub `docker` 覆盖：无认证 / Bearer 认证下的存在与 `404` 缺失；精确 tag `200` 后按 digest
    读 `not found` → 读错误；`401` / `403` / `429` / `500` / `503` / `301`、无凭据质询、凭据错误、无 token、非 https realm、Basic
    质询、设置凭据时未经质询的 `404`、缺少或畸形 digest、curl 退出 3（不得被当成缺失）均为读错误；凭据与 token 不出现在 curl argv 与消息中。实测（Buildx
    v0.38.1，dash 与 macOS sh）：一次性的本地替身 registry 让 tag HEAD 返回 `200` + digest、按 digest GET 返回 `404`，旧实现退出 3
    （误报缺失）、现实现退出 1；本地 `registry:2` 上带 provenance 的 index 与普通 manifest 退出 0 且 label 正确，缺失 tag 退出 3，
    删去镜像 config blob 后退出 1，`copy` 后读回 digest 一致。GHCR 匿名只读探测确认：匿名 HEAD（已有 tag、不存在的 tag、
    不存在的仓库）一律回 `401` + Bearer 质询（realm `https://ghcr.io/token`）；经 `curl -K -` 传入匿名 token 后，已有 tag 回 `200`
    且带 `docker-content-digest`，不存在的 tag 回 `404`。真实 curl 经 `-K -` 发出的 Basic 与 Bearer 头已在本地替身服务器上核对。
    用 `GITHUB_TOKEN` 的 Basic 凭据换 token、以及包尚不存在时的回答，要到首个真实 tag 才能确认。
- `github-release`（`needs: [promote-tags]`）—— 用
  [`.github/scripts/release-github.sh`](../../.github/scripts/release-github.sh) 创建该 tag 的 GitHub Release。排在镜像与别名
  之后：它失败时已推送的镜像与别名保持原样，单独重跑这个 job 即可补齐，不会回头改镜像。
  - 正文：`docs/reports/releases/vX.Y.Z.md` 中 `<!-- release-notes:start -->` 与 `<!-- release-notes:end -->` 两行之间的内容
    （各恰好一次、独占一行、start 在前；行尾单个 CR 忽略，CRLF 检出读法相同；去掉首尾空行），末尾追加一个指向 `main` 上该报告的
    Markdown 链接（tag 后补录的发布证据在那里）。标记段内的链接必须是绝对 `https` URL（或 `#锚点`、`mailto:`）：相对路径在
    Release 页面会按 Releases 的 URL 解析而失效，所以 `](相对路径)` 与 `[ref]: 相对路径` 都会失败。两行标记是保留字：代码块里的
    同样两行也会被识别。正文取自 tag 的 checkout，随 tag 冻结；release lane 的 `release-notes` 步骤在 `verify-release` 里跑同一个
    抽取，缺标记、标记重复或顺序颠倒、标记之间只有空行、相对链接，都会让 tag 在推镜像之前失败。标题为 `vX.Y.Z`。
  - Latest：与 `latest` 镜像别名同一策略（最高的已发布 stable 版本）。job 用 `release-promotion.sh alias-version` 现读 `latest`
    别名的版本 label：等于本 tag 的版本 → 本 tag 必须成为 Latest（`--latest`）；否则显式 `--latest=false`，且 GitHub 当前的
    Latest 必须已经是别名所在版本的 tag，否则说明更高版本的 Release 缺失、未发布或未标 Latest，job 在写入前失败并指出先补哪个
    （重跑那个 tag 的 github-release；其 run 早于本 job 时手建）。不用 GitHub 按日期与版本自动判断的默认值（给旧版本线发补丁时
    它可能抢走 Latest），也不沿用 promote-tags 的输出（两个 tag 相继发布时会过期）。本 job 与 promote-tags 共用
    `concurrency: release-promote-<repo>`（`cancel-in-progress: false`、`queue: max`）：任何 tag 的别名晋升都不会插进本 job 的
    "读别名 → 写 Latest"之间。别名只升不降，高版本的 Release job 总在其晋升之后运行，所以 Latest 最终停在最高版本；高版本的
    Release job 失败时 Latest 暂时落后，之后任何低版本的 run 都会按上面的检查失败而不是掩盖它。
  - 顺序与幂等：所有读取与检查都在第一次写之前——列出全部 Release（分页，含 draft），按 tag 名匹配；"不存在"只由完整列表判定，
    不看 `404`（`GET /releases/tags/{tag}` 只返回已发布的 Release，对 draft 同样回 `404`），列表任一页失败或结构不符（非数组、
    条目缺少字符串 `tag_name` / 布尔 `draft`、`prerelease`）都是读错误；draft、prerelease、同一 tag 有多个 Release（draft 可以
    同名）、读错误、上述 Latest 不一致 → job 失败、**零写入**，由维护者处理后重跑。然后至多一次写：没有 Release → `gh release
    create --verify-tag --target <本 run 的 commit>`；已发布且应标 Latest、但尚未是 Latest → `gh release edit --latest`；已发布的
    Release 的标题与正文**原样保留**（重跑或手建的 Release）。最后读回：Release 存在、已发布、非 prerelease，GitHub 的 Latest 等于
    预期；写之后的读回失败报告为"已写入、未确认"，重跑会先读状态再补齐或确认，不会删除已发布的 Release。
  - tag 归属：写之前（读完别名之后、紧挨着 `ensure`）用 `release-promotion.sh tag-commit` 核对远端 tag 仍指向本 run 的 commit，
    与 publish-image 相同。gh 的 `--verify-tag` 与创建是两次请求；tag 若恰在其间被删除，GitHub 会按 `--target` 重新创建，此时指向
    本 run 的 commit 而非默认分支。核对与创建之间仍有几秒的窗口（tag 恰在其间移动或删除），workflow 内无法消除，只有禁止更新与
    删除 `v*` tag 的 tag 保护规则（ruleset）能根除。2026-11-24 查询 `GET /repos/{repo}/rulesets` 为空：本仓库尚未启用；启用与否
    由维护者决定（publish-image 的同类窗口同样受益）。
  - 权限：`contents: write`（创建 Release）与 `packages: read`（读别名，同 promote-tags 经 `imagetools inspect`，所以同样
    固定 Buildx 并登录 GHCR）。用 `GITHUB_TOKEN` 创建的 Release 不触发其他 workflow；本 workflow 只由 tag push 触发，不受影响。
  - 测试：static lane 的 `release-github` 步骤用文件后端的假 GitHub（`release-github-fake.sh`）离线覆盖正文抽取（各种标记错误、
    CRLF、相对链接）、新建 / 已发布 / draft / prerelease / 重复 Release、Latest 不一致、写前读错误零写入、写后读错误报"未确认"、
    创建与 Latest 的读回；同一脚本用 stub `gh`（以 jq 执行真实的 `--jq` 过滤器）覆盖真实适配器 `release-github-api.sh`：第二页
    命中、后续页失败、畸形列表、参数拒绝，以及 `--paginate`、`--verify-tag`、`--target`、`--latest=<bool>` 的确切 argv。
    `release-promotion` 步骤覆盖 `alias-version`（含经真实 registry 适配器读到带空白的 label 时失败）。真实 gh 的只读调用已对本仓库
    实测（列表匹配、缺失为空、读 Latest）；创建与标 Latest 要到首个真实 tag 才能确认。

  四个 verify job 都检出 tag 指向的同一 SHA，因此 postgres lane 与 audit 不再依赖"tag 前人工确认 exact-SHA CI"这一步来保证。

### 修复前 release run 的重跑边界（R3-1）

GitHub 重跑一个 workflow run 时沿用**原始**的 commit、ref 与该 commit 上的 workflow 文件
（[Re-running workflows and jobs](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs)），
不会采用 `main` 上的新 `release.yml`。`v0.10.4` 及更早 tag 的 release run 用的是旧 workflow，门槛按版本不同（读各 tag 上的
`release.yml`）：`v0.7.x` / `v0.8.x` 只有 `publish-image`，`v0.9.x` 先跑一个 `verify` job，`v0.10.0`…`v0.10.4` 先跑
`verify-test` 与 `verify-release`。它们都直接把 `latest` / `X.Y`（及 `-scheduler`）推到本 tag，不经过 PostgreSQL lane、
`cargo audit` 门槛，也不读别名当前的版本 label、不进新的晋升队列。只要本仓库的 workflow 仍对 GHCR package 有写权限，重跑这些
旧 run 就可能覆盖较新的生产别名。GitHub 只允许在首次运行后 30 天内重跑（同上文档），所以风险随时间自然消失：截至 2026-11-24
仍可重跑的旧 run 只有 `v0.10.0`（首次运行 2026-10-31）与 `v0.10.1`…`v0.10.4`（2026-11-17 至 11-20），最晚一个在 2026-12-20
前后过期。约定：

- **不要重跑** `v0.10.4` 及更早 tag 的 release run（包括其中单个 job），只重跑使用本文件现行 `release.yml` 的 run。
- GHCR 的 Actions 写权限按**仓库**授予（package 设置 → Manage Actions access），无法收窄到单个 workflow 文件：保持只有本仓库
  有 write 角色、不给其他仓库授权；`GITHUB_TOKEN` 的 `packages: write` 只在现行 `release.yml` 的两个镜像 job 上声明。
- 30 天期限之前，想从技术上排除旧 run 被重跑，只能删除这些 run（`gh run delete <run-id>`）。这会同时删除其日志与发布证据
  链接，属于不可逆的批量删除，需由维护者单独决定；本仓库目前**不**这样做，靠上面的约定等期限自然到期。
- 若旧 run 已被误重跑：重跑一个现行 release 的 `promote-tags` job，且该 release 须属于被旧 run 拉低的那条 `X.Y` 线（它同时
  补齐 `latest`）；该线若没有现行 release，`latest` 可由任一现行 release 补齐，那条 `X.Y` 线则只能等该线的下一个 release 或由
  维护者手工对齐（R3G-S3）。reconcile 只认别名的版本 label 与已发布版本，会把被旧 run 拉低的别名重新对齐到最高已发布版本。旧 run 还会重建并覆盖它自己的 `X.Y.Z` / `X.Y.Z-scheduler`（digest 因重建
  而改变，revision label 不变），这无法自动撤回，需在该版本的 release report 中记录新 digest。

## 镜像 tag 规则

版本 tag 由 `docker/metadata-action` 计算，`latest` / `X.Y` 由 `promote-tags` 在两镜像推送后晋升：

| 推送 tag | 生成 / 对齐的镜像 tag |
|---|---|
| `v0.7.0`（且为最高已发布 stable 版本） | `0.7.0`、`0.7`、`latest` |
| `v0.6.3`（`v0.7.0` 已发布，`0.6` 线内最高） | `0.6.3`、`0.6`；`latest` / `0.7` 保持（已在 `0.7.0`） |
| 重跑 `v0.7.0`（`v0.7.1` 已发布） | 复用 `0.7.0`（不重建）；`latest` / `0.7` 保持在 `0.7.1` |
| `v1.4.1` 在 `v2.0.0` 镜像推完之后晋升 | `1.4` → `1.4.1`，`latest` / `2.0` → `2.0.0` |
| 重跑 `v1.4.3`（`v1.4.4` 曾发布、其 git tag 已删除） | `latest` / `1.4` 保持在 `1.4.4`（label 更高，不回退） |
| `v0.7.0-rc.1` | 不推镜像：verify gate 只接受 `X.Y.Z` |

`develata` lowercase 后即 GHCR 命名空间（owner）。runtime 使用上述 tag；scheduler 位于**同一 package**，
对应 `<tag>-scheduler`（例如 `ghcr.io/develata/rss-ai-news:0.7.0-scheduler`）。workflow 不生成
major-only alias（如 `0` / `1`）。

## 发版流程

```bash
# 1. 完成 feature/fix、将 workspace package version 对齐候选 tag（同时改 README 的当前版本、
#    docker/docker-compose.scheduler.yml 的 RSS_AI_NEWS_VERSION 默认值，以及 docker-and-scheduler.md、
#    postgres-deployment.md、plan/12 与 README 示例命令中的镜像版本——release lane 的 deployment-versions
#    检查逐一比对），跑完 security scan/review 并确认 main exact SHA 的三个 CI jobs 全绿

git checkout main && git pull

# 2. 决定版本号：bug fix → vX.Y.Z+1；feature → vX.Y+1.0；breaking → vX+1.0.0
#    本项目只发 stable tag，不创建 prerelease tag；release.yml 的 verify gate 只接受 X.Y.Z，
#    推送 prerelease tag 会在 verify 失败、不推镜像（下方 rc 行仅说明 metadata-action 的映射规则）

# 3. 在 tag 前写好 release report，使 release notes 随 tag 一起冻结。面向用户的部分（核心交付、已知限制、
#    升级与行为变更）放在 `<!-- release-notes:start -->` 与 `<!-- release-notes:end -->` 两行之间，github-release
#    job 用它作 GitHub Release 正文。release lane 默认按 workspace 版本检查这份报告，所以版本号改动与报告要在
#    同一个 commit / PR 里进 main，否则 main 的 test-pg job 会失败
$EDITOR docs/reports/releases/v0.7.0.md
git add docs/reports/releases/v0.7.0.md
git commit -m "docs(reports): prepare v0.7.0 release notes"
git push origin main
# 等上述 exact SHA CI 全绿

# 4. 打 annotated tag（带说明）
git tag -a v0.7.0 -m "Release v0.7.0: <一句话>"

# 5. push tag（release workflow 先跑 verify-*；通过后才写 GHCR。在 exact-SHA CI 完成后再推，
#    verify 才能复用 CI 刚保存的缓存）
git push origin v0.7.0

# 6. 等 release workflow 完成
run_id=$(gh run list --workflow release.yml --branch v0.7.0 --json databaseId --jq '.[0].databaseId')
gh run watch "$run_id" --exit-status

# 7. 校验两个 immutable image tags，并执行 pull / --version / --help smoke
docker manifest inspect ghcr.io/develata/rss-ai-news:0.7.0
docker manifest inspect ghcr.io/develata/rss-ai-news:0.7.0-scheduler
docker pull ghcr.io/develata/rss-ai-news:0.7.0
docker run --rm ghcr.io/develata/rss-ai-news:0.7.0 --version
docker run --rm ghcr.io/develata/rss-ai-news:0.7.0 --help
docker pull ghcr.io/develata/rss-ai-news:0.7.0-scheduler
docker run --rm --entrypoint /usr/local/bin/rss-ai-news \
  ghcr.io/develata/rss-ai-news:0.7.0-scheduler --version

# 8. 读回 GitHub Release（github-release job 创建；不要事先手建）
gh release view v0.7.0 --json isDraft,isPrerelease,url
gh api repos/Develata/RSS-AI-News/releases/latest --jq .tag_name   # 本版是最高版本时应为 v0.7.0
```

workflow 发布 GHCR images 并创建 GitHub Release（自 github-release job 合入后的第一个 tag 起；更早的 tag 见
[已知发布事件](#已知发布事件)）。release 完成后把 run id、manifest digest、smoke 结果与 Release 链接回填到 report，
作为 post-release evidence。

## 镜像版本提升

部署侧：

```bash
# 1. 在 .env 里把 RSS_AI_NEWS_VERSION 改成新版本（compose 对版本、挂载、网络只有这一处选择）
# 2. 拉新版
docker compose -f docker-compose.scheduler.yml pull

# 3. 若有 schema 变化，先用同一服务一次性 migrate（不动正在运行的 scheduler 容器）；
#    entrypoint 换成 CLI，版本与随后启动的 scheduler 相同
docker compose -f docker-compose.scheduler.yml run --rm --no-deps \
  --entrypoint /usr/local/bin/rss-ai-news \
  rss-ai-news-scheduler --config-dir /app/configs migrate run

# 4. 升级 scheduler
docker compose -f docker-compose.scheduler.yml up -d --force-recreate
```

不要用单独的 `docker run <image>:X.Y.Z … migrate run` 配合 compose 部署：那里的镜像 tag 由宿主 shell 展开，`--env-file .env`
只把变量传进容器，只改 `.env` 时迁移与启动会用不同版本的二进制（R3-11）。

## 已知发布事件

| Tag | 日期 | report |
|---|---|---|
| v0.1.0 | 2026-05-22 | [../reports/releases/v0.1.0.md](../reports/releases/v0.1.0.md) |
| v0.1.1 | 2026-05-20 | patch（未单独 report） |
| v0.1.2 | 2026-05-21 | patch（未单独 report） |
| v0.2.0 | 2026-05-22 | [../reports/releases/v0.2.0.md](../reports/releases/v0.2.0.md) |
| v0.3.0 | 2026-05-22 | [../reports/releases/v0.3.0.md](../reports/releases/v0.3.0.md) |
| v0.3.1 | 2026-05-25 | patch（未单独 report） |
| v0.4.0 | 2026-05-25 | [../reports/releases/v0.4.0.md](../reports/releases/v0.4.0.md) |
| v0.5.0 | 2026-06-12 | [../reports/releases/v0.5.0.md](../reports/releases/v0.5.0.md) |
| v0.6.0 | 2026-06-17 | [../reports/releases/v0.6.0.md](../reports/releases/v0.6.0.md) |
| v0.6.1 | 2026-06-19 | patch（未单独 report） |
| v0.6.2 | 2026-06-19 | patch（未单独 report） |
| v0.7.0 | 2026-07-17 | [../reports/releases/v0.7.0.md](../reports/releases/v0.7.0.md) |
| v0.7.1 | 2026-07-18 | [../reports/releases/v0.7.1.md](../reports/releases/v0.7.1.md) |
| v0.8.0 | 2026-09-10 | [../reports/releases/v0.8.0.md](../reports/releases/v0.8.0.md) |
| v0.8.1 | 2026-09-11 | [../reports/releases/v0.8.1.md](../reports/releases/v0.8.1.md) |
| v0.9.0 | 2026-09-26 | [../reports/releases/v0.9.0.md](../reports/releases/v0.9.0.md) |
| v0.9.1 | 2026-10-16 | [../reports/releases/v0.9.1.md](../reports/releases/v0.9.1.md) |
| v0.10.0 | 2026-10-31 | [../reports/releases/v0.10.0.md](../reports/releases/v0.10.0.md) |
| v0.10.1 | 2026-11-17 | [../reports/releases/v0.10.1.md](../reports/releases/v0.10.1.md) |
| v0.10.2 | 2026-11-17 | [../reports/releases/v0.10.2.md](../reports/releases/v0.10.2.md) |
| v0.10.3 | 2026-11-18 | [../reports/releases/v0.10.3.md](../reports/releases/v0.10.3.md) |
| v0.10.4 | 2026-11-21 | [../reports/releases/v0.10.4.md](../reports/releases/v0.10.4.md) |

> 当前约定：每个 tag 都写 report，含 `release-notes` 标记段：release lane 的 `release-notes` 步骤要求它存在，
> github-release job 用它作 GitHub Release 正文（此前"纯 bugfix patch 可不写"的约定随该 job 废止）。
>
> GitHub Release：`v0.8.0`、`v0.8.1`、`v0.9.0` 是手工创建的；上表其余 tag 没有 Release（`v0.7.0` 起曾约定 workflow 不创建
> Release）。github-release job 合入后的 tag 由 workflow 创建；旧 tag 的补建属于对外发布，由维护者单独决定，不重跑旧 run
> （见上文 R3-1）。

## 相关文档

- workflow 文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`.github/workflows/release.yml`](../../.github/workflows/release.yml)、[`.github/workflows/audit.yml`](../../.github/workflows/audit.yml)、[`.github/workflows/maintenance.yml`](../../.github/workflows/maintenance.yml)
- 部署：[./docker-and-scheduler.md](./docker-and-scheduler.md)
- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
