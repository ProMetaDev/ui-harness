#!/usr/bin/env bash
# 密钥扫描：在仓库的文本文件里找常见密钥形态，命中就报红，可直接当提交前门禁。
#
# 用法：
#   bash tools/scan_secrets.sh                  # 扫整个仓库
#   bash tools/scan_secrets.sh --ignore my.txt  # 换一份忽略清单
#   bash tools/scan_secrets.sh --help
#
# 关键点：
#   * 只打印掩码片段（前 4 位 + ... + 后 4 位），完整密钥永远不出现在输出里，
#     所以日志可以直接贴到 issue / CI 里。
#   * 文件清单优先用 git ls-files（只看纳入版本管理的文件，git 会顺手跳过
#     未跟踪的临时文件）；仓库里没有 git 就退回 find。
#   * secrets/ out/ .git/ target/ node_modules/ dist/ 一律跳过：都是密钥或产物，
#     扫了只会刷屏。
#   * .secretsignore 每行一个路径子串（不是 glob），空行和 # 开头的行忽略。
#   * 命中退出 1，干净退出 0，用法/环境错误退出 2。
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT" || { echo "scan_secrets: 无法进入 $UI_ROOT" >&2; exit 2; }

fail() {
  echo "scan_secrets: $*" >&2
  exit 2
}

usage() {
  cat <<'USAGE'
用法：bash tools/scan_secrets.sh [选项]

扫描仓库里的文本文件，找疑似密钥。命中退出 1，干净退出 0。

选项：
  --ignore <文件>   忽略清单；默认读仓库根的 .secretsignore
  -h, --help        显示本帮助

忽略清单格式：每行一个路径子串（不是 glob），相对路径里含该子串就跳过；
空行和 # 开头的行忽略。

内置跳过的目录：secrets/ out/ .git/ target/ node_modules/ dist/

命中的行只打印掩码片段（前 4 位 + ... + 后 4 位），不输出完整密钥。
USAGE
}

# ---------- 规则 ----------
# 规则宁少勿滥：只留区分度高的形态，避免把普通字符串、UUID、哈希报成密钥。
# 规则名会出现在输出里，方便判断是哪类泄露。
RULE_NAMES=(OPENAI_SK AWS_ACCESS_KEY PRIVATE_KEY ASSIGNED_SECRET)

# 赋值类规则要匹配单双引号；写成变量，省得在脚本的引号里再套一层
DQ='"'
SQ="'"

RULE_RES=(
  "sk-[A-Za-z0-9_-]{20,}"
  "AKIA[0-9A-Z]{16}"
  "-----BEGIN [A-Z ]*PRIVATE KEY-----"
  "(api_key|apikey|token|secret)[[:space:]]*[:=][[:space:]]*[$DQ$SQ][A-Za-z0-9_-]{16,}[$DQ$SQ]"
)

# 剪掉 Windows 换行和首尾空白
trim() {
  local s="$1"
  s="${s%$'\r'}"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

# 掩码：只留前 4 位和后 4 位；短片段整体抹掉，避免把 8 位以内的短值露出来
mask() {
  # 拆成两条：local 的多个赋值不保证按顺序展开，set -u 下 ${#s} 会先报 unbound
  local s="$1"
  local n=${#s}
  if [ "$n" -le 8 ]; then
    printf '****'
    return
  fi
  printf '%s...%s' "${s:0:4}" "${s: -4}"
}

# ---------- 参数 ----------
IGNORE_ARG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --ignore)
      [ $# -ge 2 ] || fail "--ignore 缺少文件路径"
      IGNORE_ARG="$2"; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      fail "未知参数 $1（用 --help 看用法）" ;;
  esac
done

if [ -n "$IGNORE_ARG" ]; then
  [ -f "$IGNORE_ARG" ] || fail "忽略清单不存在: $IGNORE_ARG"
  IGNORE_FILE="$IGNORE_ARG"
else
  IGNORE_FILE="$UI_ROOT/.secretsignore"
fi

# ---------- 忽略子串 ----------
IGNORE_PATTERNS=()
if [ -f "$IGNORE_FILE" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    line="$(trim "$line")"
    [ -n "$line" ] || continue
    case "$line" in
      \#*) continue ;;
    esac
    IGNORE_PATTERNS+=("$line")
  done < "$IGNORE_FILE"
fi

# 路径命中任意忽略子串就跳过
is_ignored() {
  local rel="$1" p
  if [ "${#IGNORE_PATTERNS[@]}" -eq 0 ]; then
    return 1
  fi
  for p in "${IGNORE_PATTERNS[@]}"; do
    case "$rel" in
      *"$p"*) return 0 ;;
    esac
  done
  return 1
}

# 内置跳过目录：密钥目录、构建产物、依赖树、产物副本
is_skipped_dir() {
  case "/$1/" in
    */secrets/*|*/out/*|*/.git/*|*/target/*|*/node_modules/*|*/dist/*) return 0 ;;
  esac
  return 1
}

# 仓库里的候选文件，NUL 分隔：路径带空格也不怕
list_files() {
  if command -v git >/dev/null 2>&1 && git -C "$UI_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git -C "$UI_ROOT" ls-files -z
  else
    find . -type f -print0
  fi
}

# ---------- 扫描 ----------
HITS=0
FILES_SCANNED=0
# 行级允许清单放过几条：必须打出来。放过本身不是问题，
# 悄悄放过 40 条才是问题。
ALLOWLISTED=0

scan_file() {
  local rel="$1" f="$UI_ROOT/$1" i name re rec lineno snippet line
  for i in "${!RULE_NAMES[@]}"; do
    name="${RULE_NAMES[$i]}"
    re="${RULE_RES[$i]}"
    # -o：只取命中片段，不把整行（可能含更多密钥）带出来
    # -n：带行号；-I：二进制文件静默跳过
    # 单文件时输出是「行号:片段」，片段里即使有冒号也不影响，行号在前且是纯数字
    while IFS= read -r rec; do
      [ -n "$rec" ] || continue
      lineno="${rec%%:*}"
      snippet="${rec#*:}"
      [[ "$lineno" =~ ^[0-9]+$ ]] || continue
      # 行级允许清单：同一行写着 `allowlist secret` 才生效（detect-secrets 的老约定）。
      # 为什么不走 .secretsignore：那是**整文件**忽略，而测试夹具所在的文件正是
      # 真泄露最该被看见的地方；一行一行标注，代价是显式的、可见的。
      line=$(sed -n "${lineno}p" "$f" 2>/dev/null)
      case "$line" in
        *"allowlist secret"*) ALLOWLISTED=$((ALLOWLISTED + 1)); continue ;;
      esac
      printf '%s:%s: %s: %s\n' "$rel" "$lineno" "$name" "$(mask "$snippet")"
      HITS=$((HITS + 1))
    done < <(grep -n -o -I -E -- "$re" "$f" 2>/dev/null)
  done
}

while IFS= read -r -d '' rel; do
  rel="${rel#./}"
  [ -n "$rel" ] || continue
  is_skipped_dir "$rel" && continue
  is_ignored "$rel" && continue
  f="$UI_ROOT/$rel"
  # 符号链接跳过：避免顺着链接扫到仓库外面的文件
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  if [ ! -r "$f" ]; then
    echo "scan_secrets: 警告：无读取权限，跳过 $rel" >&2
    continue
  fi
  FILES_SCANNED=$((FILES_SCANNED + 1))
  scan_file "$rel"
done < <(list_files)

# ---------- 结论 ----------
if [ "$HITS" -eq 0 ]; then
  echo "scan_secrets: 扫描 $FILES_SCANNED 个文件，未发现疑似密钥 → PASS（退出码 0；"
  echo "              另有 $ALLOWLISTED 行标了 allowlist secret，逐条见 grep）" >&2
  exit 0
fi

echo "scan_secrets: 扫描 $FILES_SCANNED 个文件，发现 $HITS 处疑似密钥 → FAIL（退出码 1；"
echo "              另有 $ALLOWLISTED 行被 allowlist secret 放过）" >&2
echo "scan_secrets: 确认后把这些路径加进 .secretsignore，或直接换成占位符" >&2
exit 1
