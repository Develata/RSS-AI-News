#!/usr/bin/env bash
# F15-15 W9-F6：error-and-observability §3.3 Enforcement 第 2 层。
#
# 扫描两种典型的"静默吞错"写法并在 CI 阶段拒绝：
#   Pattern A — `if let Ok(...) = fallible(...)`（忽略 Err 分支）
#   Pattern B — `.ok();`（行末，Result 静默丢弃为 ()）
#
# 仅扫描 production / test 源码（crates/ + src/）。`.ci/swallowed-error-allowlist.txt`
# 维护已审核豁免清单；该 allowlist 唯一允许来源是"日志/可观测性写入失败"
# 类的兜底（详见 docs/design/error-and-observability.md §3.3 末段）。
#
# 调用：
#   bash .ci/check_swallowed_errors.sh        — 静默通过 / 失败列出未豁免命中
#   bash .ci/check_swallowed_errors.sh --list — 打印当前 allowlist 摘要 + 退出 0
#
# 退出码：
#   0 — 全部命中都已豁免（或无命中）
#   1 — 出现未豁免命中（CI fail）
#   2 — 工具自身错误（ripgrep 缺失等）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ALLOWLIST=".ci/swallowed-error-allowlist.txt"

if ! command -v rg >/dev/null 2>&1; then
    echo "ERROR: ripgrep (rg) not found in PATH" >&2
    exit 2
fi

if [[ "${1:-}" == "--list" ]]; then
    echo "Allowlist ($ALLOWLIST):"
    if [[ -f "$ALLOWLIST" ]]; then
        grep -Ev '^\s*(#|$)' "$ALLOWLIST" || true
    else
        echo "  (none)"
    fi
    exit 0
fi

# rg 输出格式: <file>:<line>:<content>。`--no-heading` 强制启用 file 前缀，
# `-P` 启用 PCRE 让 \s / \( 等转义生效。
# Windows 上 rg 用反斜杠输出路径；统一替成正斜杠让 allowlist 跨平台一致。
matches=$(
    {
        rg -nP --no-heading 'if\s+let\s+Ok\([^)]*\)\s*=' crates/ src/ 2>/dev/null || true
        rg -nP --no-heading '\.ok\(\)\s*;\s*$' crates/ src/ 2>/dev/null || true
    } | tr '\\' '/' | sort -u
)

# allowlist 每条格式 `<file>:<line>:<reason>`；空行 / `#` 注释跳过。
# `cut -d: -f1,2` 提取 `file:line` 作为匹配 key。
allowed_keys=""
if [[ -f "$ALLOWLIST" ]]; then
    allowed_keys=$(grep -Ev '^\s*(#|$)' "$ALLOWLIST" | cut -d: -f1,2 | sort -u)
fi

# 未豁免命中：逐行比较 file:line 前缀。
unallowed=""
if [[ -n "$matches" ]]; then
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        key=$(printf '%s' "$line" | cut -d: -f1,2)
        if ! printf '%s\n' "$allowed_keys" | grep -qFx -- "$key"; then
            unallowed="${unallowed}${line}"$'\n'
        fi
    done <<<"$matches"
fi

if [[ -n "$unallowed" ]]; then
    echo "ERROR: 检测到非允许的 swallowed-error 写法（详见 docs/design/error-and-observability.md §3.3）:" >&2
    echo "" >&2
    printf '%s' "$unallowed" >&2
    echo "" >&2
    echo "若确实合法（仅限日志/可观测性 sink 失败），请在 $ALLOWLIST 添加条目（格式: file:line:reason），并在 PR 中说明并经设计 owner 批准。" >&2
    exit 1
fi

allowed_count=0
if [[ -n "$allowed_keys" ]]; then
    allowed_count=$(printf '%s\n' "$allowed_keys" | wc -l | tr -d ' ')
fi
echo "swallowed-error 扫描通过（allowlist 命中 ${allowed_count} 条）"
