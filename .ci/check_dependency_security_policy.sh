#!/usr/bin/env bash
set -euo pipefail

# RSS-AI-News 只使用 Octocrab personal-token auth。Octocrab 0.54 的 default
# feature 会启用 jwt-rust-crypto，把 rsa crate 拉进 active graph（RUSTSEC-2023-0071，
# 修复版本为 >= 0.10.0）。本脚本禁止任何版本的 rsa 出现在 active graph，不依赖 advisory
# 数据库，在 static lane 里直接指出激活它的 feature 路径。.cargo/audit.toml 不再豁免该
# advisory，所以受影响版本的 rsa 哪怕只以 inactive 形式回到 Cargo.lock，cargo audit 也会失败。
# --workspace：默认只查根包的依赖树，漏掉其他成员（如 tools/acceptance）；
# --target all：默认只查 host 平台，漏掉其他平台的条件依赖。
set +e
output="$(cargo tree --locked --workspace --target all -e features -i rsa 2>&1)"
rc=$?
set -e

if grep -q '^rsa v' <<<"$output"; then
  printf '%s\n' 'dependency security policy failed: rsa is active in the feature graph' >&2
  printf '%s\n' "$output" >&2
  exit 1
fi

if [[ $rc -eq 0 ]] || grep -q 'did not match any packages' <<<"$output"; then
  printf '%s\n' 'dependency security policy passed: rsa has no active path'
  exit 0
fi

printf '%s\n' 'dependency security policy could not classify the cargo tree result:' >&2
printf '%s\n' "$output" >&2
exit "$rc"
