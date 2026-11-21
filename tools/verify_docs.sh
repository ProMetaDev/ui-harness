#!/usr/bin/env bash
# 文档与代码的一致性守卫。
#
# 为什么要有这个脚本：Phase 1 把隐私行为连改了三轮，文档只跟着改过一次，于是
# README / SECURITY.md / docs 里留下了"没有 OS 级沙箱""日志里没有密钥明文"这种
# **当时对、后来错**的句子。对一个把"能验证的才说，不能验证的明确标注"写进
# SECURITY.md 的项目，假话比缺文档更糟——用户会按文档去处置 `.ui/`（打包、外发、
# `rm -rf`），而实际脚印不是那样。
#
# 这里钉住四类最容易被改错的事实：
#   1 代码里的目录常量与保留期默认值 == 文档写的数字
#   2 代码读的开关名 == 文档提到的开关名（两个方向都查）
#   3 文档里的 `file.rs:行号` 锚点还指得对
#   4 已被证伪的绝对化说法不再出现
# 外加：文档里写的 `ui 子命令` 必须是真存在的子命令。
#
# 它不判断"写得好不好"，只判断"还说不说得出"。判断不了的地方它闭嘴——
# 所以这一层绿了不等于文档完整，只等于文档没有和代码当面吵架。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

PASS=0
FAIL=0
ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}
bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
  [ -n "${2:-}" ] && printf '       %s\n' "$2"
}

# 对外承诺的那两份文档是重点；其余 docs/*.md 也要过一遍禁用句
DOC_CONTRACT=(SECURITY.md docs/07-security.md)
DOC_ALL=(SECURITY.md README.md docs/*.md)

in_docs() { grep -qF -- "$1" "${DOC_CONTRACT[@]}" 2>/dev/null; }

echo "=== 1. 代码常量 == 文档数字 ==="
d_out=$(grep -hoP 'pub const REL_DIR: &str = "\K[^"]+' ui-core/src/spill.rs)
d_pri=$(grep -hoP 'pub const PRIVATE_DIR: &str = "\K[^"]+' ui-core/src/spill.rs)
days=$(grep -hoP 'pub const DEFAULT_MAX_AGE_DAYS: u64 = \K[0-9]+' ui-core/src/spill.rs)
files=$(grep -hoP 'pub const DEFAULT_MAX_FILES: usize = \K[0-9]+' ui-core/src/spill.rs)
mib=$(($(grep -hoP 'pub const DEFAULT_MAX_BYTES: u64 = \K[0-9]+(?= \* 1024 \* 1024)' ui-core/src/spill.rs)))

if [ -z "${d_out:-}" ] || [ -z "${d_pri:-}" ] || [ -z "${days:-}" ]; then
  bad "读不到 spill.rs 的常量" "这一层的前提是常量按约定写法存在"
else
  for d in "$d_out/" "$d_pri/"; do
    if grep -qF -- "$d" SECURITY.md; then
      ok "SECURITY.md 提到了 $d"
    else
      bad "SECURITY.md 没提 $d" "内核会在工作区留下这个目录，脚印清单必须写全"
    fi
  done
  tier="$days 天 / $files 个 / $mib MiB"
  if grep -qF -- "$tier" SECURITY.md; then
    ok "SECURITY.md 的保留期写的就是代码那三档（$tier）"
  else
    bad "SECURITY.md 的保留期与代码不符" "代码是 $tier；改默认值要连文档一起改"
  fi
fi

# 唯一读取例外这件事只有一处真值：PROTECTED_READ_EXCEPTIONS
ex_count=$(grep -hoP 'PROTECTED_READ_EXCEPTIONS: &\[&str\] = &\[\K[^\]]*' ui-core/src/protected.rs \
  | grep -oP '"\K[^"]+' | wc -l)
ex_one=$(grep -hoP 'PROTECTED_READ_EXCEPTIONS: &\[&str\] = &\[\K[^]]*' ui-core/src/protected.rs)
if [ "$ex_count" = "1" ] && echo "$ex_one" | grep -qF -- "$d_out"; then
  ok "代码里的读取例外确实只有 $d_out/ —— 文档里「唯一的读取例外」这句还成立"
else
  bad "读取例外不止一个或换名了" "文档里「唯一的读取例外」这句要跟着改（现在是：$ex_one）"
fi

echo "=== 2. 开关名：代码 ↔ 文档 双向 ==="
code_knobs=$(grep -rhoP '"\KUI_(REDACT|PRIVATE|OBSERVE|SANDBOX[A-Z_]*|SPILL[A-Z_]*)\b' \
  ui-core/src ui-cli/src 2>/dev/null | grep -oP 'UI_[A-Z_]+' | sort -u)
# 代码里读的那些，隐私/沙箱这两族的必须对用户说明——不说明就等于"隐藏的开关"
for k in $code_knobs; do
  if in_docs "$k"; then
    ok "开关 $k 在契约文档里"
  else
    bad "$k 代码里读，但 SECURITY.md / docs/07-security.md 没提" "用户看不见就会以为不存在"
  fi
done
# 文档里点名引用的开关（不是全集，是"用户会照着敲"的那几个）
for f in no-redact private dry-run tool-chars; do
  if grep -qP "\"$f\"" ui-cli/src/main.rs; then
    ok "--$f 代码里还认"
  else
    bad "文档提到 --$f，代码已经不再解析它" "开关改名要连文档一起改"
  fi
done
# 反方向：文档提到的这一族开关，代码里必须有
doc_knobs=$(grep -rhoP '\bUI_(REDACT|PRIVATE|OBSERVE|SANDBOX[A-Z_]*|SPILL[A-Z_]*)\b' \
  "${DOC_ALL[@]}" 2>/dev/null | sort -u)
for k in $doc_knobs; do
  if grep -rqF -- "\"$k\"" ui-core/src ui-cli/src; then
    ok "文档提到的 $k 代码里真的读"
  else
    bad "文档写了 $k，代码里没有这个开关" "这是凭空承诺，删掉或写进内核"
  fi
done

echo "=== 3. 文档里的 file:行 锚点 ==="
anchors=$(grep -rhoP '\b[a-z_]+\.rs:[0-9]+' "${DOC_ALL[@]}" 2>/dev/null | sort -u)
if [ -z "$anchors" ]; then
  ok "文档没有行号锚点（这一层没东西可查）"
fi
for a in $anchors; do
  f="${a%%:*}"
  n="${a##*:}"
  p=$(find ui-core/src ui-cli/src -name "$f" -print -quit 2>/dev/null)
  if [ -z "$p" ]; then
    bad "锚点 $a 找不到文件 $f" "文件改名或删了，锚点要跟着改"
    continue
  fi
  total=$(wc -l <"$p")
  if [ "$n" -le "$total" ]; then
    ok "锚点 $a 仍在文件范围内（$p 共 $total 行）"
  else
    bad "锚点 $a 超出文件长度" "$p 只有 $total 行"
  fi
done
# 行号对不对不能只看范围：这几条是文档里真正拿来当证据的
cite() {
  local a="$1" needle="$2"
  local f="${a%%:*}" n="${a##*:}"
  local p
  p=$(find ui-core/src ui-cli/src -name "$f" -print -quit 2>/dev/null)
  if [ -n "$p" ] && sed -n "${n}p" "$p" | grep -qF -- "$needle"; then
    ok "$a 那一行确实写着「$needle」"
  else
    bad "$a 不再指向「$needle」" "代码动了，文档里引用它的那句话就成了假话"
  fi
}
cite "agent.rs:838" "append_raw"
cite "agent.rs:491" "append_raw"
cite "memory.rs:325" "append_raw"
cite "tools.rs:942" "pub fn registry"

# 行号锚点的脆弱性：往文件里插 14 行，三个"证据"全红——而代码其实没错。
# 所以对**动得频繁**的函数/路由改用符号存在性：行号会漂，符号不会。
symbol() {
  local file="$1" needle="$2"
  local p
  p=$(find ui-core/src ui-cli/src -name "$file" -print -quit 2>/dev/null)
  if [ -n "$p" ] && grep -qF -- "$needle" "$p"; then
    ok "$file 里仍有「$needle」"
  else
    bad "$file 里找不到「$needle」" "代码里被改名/删掉了"
  fi
}
symbol "server.rs" "fn on_tool"
symbol "server.rs" "/api/tool/"
# 网页端脱敏的两个出口（2026-11-20 修）：少了任何一个，/snapshot 就会重新带明文
symbol "server.rs" "ui_core::secrets::redact_json"

echo "=== 4. 已被证伪的说法 ==="
for phrase in "没有 OS 级沙箱" "挡在门口" "日志里没有密钥明文" "写了文件无记录" "日志里也没有对应记录" "记忆未做" "解法分别是" ; do
  hit=$(grep -rlF -- "$phrase" "${DOC_ALL[@]}" 2>/dev/null | tr '\n' ' ')
  if [ -z "$hit" ]; then
    ok "没人再说「$phrase」"
  else
    bad "文档里还有「$phrase」" "$hit"
  fi
done

echo "=== 5. 文档写的 ui 子命令存在 ==="
cmds=$(grep -hoP 'Some\("\K[a-z][a-z0-9_-]*(?="\))' ui-cli/src/main.rs | sort -u)
# 这些不是子命令，是 crate / 项目名，别把它们算成命令
noncmds="core cli harness"
# 只数"承诺句"里的命令：规划表里写着「未做 / ❌」的行、以及"这是未知命令"这类
# 叙述行，说的都是**不存在**，把它们当承诺就成了假阳性（第一版就误报了两次）。
used=$(grep -rnoP '(?<![\w-])ui \K[a-z][a-z0-9_-]*' "${DOC_ALL[@]}" 2>/dev/null |
  while IFS=: read -r f l c; do
    line=$(sed -n "${l}p" "$f" 2>/dev/null)
    case "$line" in
      *"未做"* | *"❌"* | *"未知"* | *"待办"* | *"计划"* | *"P3"*) continue ;;
    esac
    echo "$c"
  done |
  sort -u)
for c in $used; do
  case " $noncmds " in *" $c "*) continue ;; esac
  if echo "$cmds" | grep -qxF -- "$c"; then
    ok "ui $c 是真命令"
  else
    bad "文档写了「ui $c」，help 里没有这个子命令" "要么改名没同步，要么根本不存在"
  fi
done

# ---------- 6. 门禁脚本自己不能依赖被 gitignore 的目录 ----------
# 这条是被 CI 红了两轮换回来的：verify_privacy.sh 从 out/ 搬进 tools/ 时，
# 脚本里 9 处 `>out/xxx.log` 没跟着搬。开发树里 out/ 永远在，89/89 全绿；
# 干净检出里没有这个目录，重定向直接失败 -> 第一轮 run 没跑 -> 51 条连着红。
# 「本地全绿」不等于「别人 clone 下来全绿」，差的就是这一类引用。
# 只看代码行：注释里举例说明"不要这么写"是合理的，不能算违规
# （这条检查第一版就栽在自己的注释上）。
viol=$(git ls-files 'tools/*.sh' | while IFS= read -r f; do
  hit=$(grep -nE '2?>[[:space:]]*"?out/' "$f" 2>/dev/null \
        | grep -vE '^[0-9]+:[[:space:]]*#' || true)
  [ -n "$hit" ] && printf '%s\n' "$f: $hit"
done)
if [ -z "$viol" ]; then
  ok "tools/ 里没有脚本往 gitignored 的 out/ 写东西"
else
  bad "门禁脚本往 out/ 写日志（干净检出里没有这个目录）" "$viol"
fi

# 顺带钉住反向依赖：docs 里引用的 tools/ 脚本必须真在版本库里
refs=$(grep -rhoE 'tools/[a-zA-Z0-9_.-]+\.sh' "${DOC_ALL[@]}" 2>/dev/null | sort -u)
missing=""
for r in $refs; do
  git ls-files --error-unmatch "$r" >/dev/null 2>&1 && continue
  # 设计文档里"计划中"的脚本本来就还没入库；要求引用行显式写明，
  # 否则等于允许文档永远领先于代码（那是另一种失步）。
  if grep -qE "${r}[^A-Za-z0-9_]*.{0,12}(计划中|未实现|not implemented)" "${DOC_ALL[@]}" 2>/dev/null; then
    continue
  fi
  missing="$missing $r"
done
if [ -z "$missing" ]; then
  ok "文档引用的 tools 脚本都在库里"
else
  bad "文档引用了不入库的脚本" "$missing"
fi

echo
echo "通过 $PASS / 失败 $FAIL"
[ "$FAIL" -eq 0 ]
