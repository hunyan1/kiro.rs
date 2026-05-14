#!/usr/bin/env bash
# 从 CHANGELOG.md 抽取指定版本段落，输出到 stdout
#
# 用法：./scripts/extract-changelog.sh v1.1.31
#
# CHANGELOG 段落示例：
#   ## [v1.1.31] - 2026-05-14
#   ...内容...
#
#   ## [v1.1.30] - 2026-05-08
#
# 该脚本会输出 v1.1.31 段落（不含上面的 `## [v1.1.31]` 标题，也不含下一个 `## [` 之后的内容）。

set -euo pipefail

if [ $# -ne 1 ]; then
  echo "Usage: $0 <tag>" >&2
  exit 2
fi

tag="$1"
# 兼容传入 v1.1.31 或 1.1.31
version="${tag#v}"
changelog="${CHANGELOG_FILE:-CHANGELOG.md}"

if [ ! -f "$changelog" ]; then
  echo "CHANGELOG file not found: $changelog" >&2
  exit 1
fi

# 用 awk 抽取目标版本段落
awk -v target="$version" '
  /^## \[v?[0-9]/ {
    # 解析当前段落 tag：取出 [...] 内的内容并去掉前导 v
    line = $0
    gsub(/^## \[/, "", line)
    sub(/\].*/, "", line)
    sub(/^v/, "", line)
    cur = line
    if (cur == target) {
      capture = 1
      next
    } else if (capture) {
      exit
    }
    next
  }
  capture { print }
' "$changelog" | sed -e 's/[[:space:]]*$//' | awk 'BEGIN{empty=0} {
  if (NF == 0) { empty++ } else { for (i=0;i<empty;i++) print ""; empty=0; print }
}'
