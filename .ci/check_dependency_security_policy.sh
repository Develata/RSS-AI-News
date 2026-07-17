#!/usr/bin/env bash
set -euo pipefail

# RSS-AI-News 只使用 Octocrab personal-token auth。Octocrab 0.54 的 default
# feature 会启用 jwt-rust-crypto，并把存在 RUSTSEC-2023-0071 且尚无修复版的
# rsa crate 拉进 active graph。Cargo.lock 可以保留 inactive rsa；禁止的是可执行依赖路径。
set +e
output="$(cargo tree --locked -e features -i rsa 2>&1)"
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
