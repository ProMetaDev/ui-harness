#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
echo "== 位置 =="
echo "源码(X:)   $(pwd)  (WSL: /mnt/x/dsh_ws/harness-v2)"
echo "构建副本   $HOME/ui"
echo "构建产物   $HOME/ui-target"
echo
echo "== 体积 =="
du -sh . 2>/dev/null | awk '{print "仓库总计  " $1}'
du -sh ui-core ui-cli docs tools fixtures out 2>/dev/null
echo
echo "== 代码量 =="
printf "%-12s %s\n" "crate" "行数"
for d in ui-core ui-cli; do
  n=$(find $d/src -name '*.rs' | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
  printf "%-12s %s\n" "$d/src" "$n"
done
n=$(find ui-core/tests ui-cli/tests -name '*.rs' | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
printf "%-12s %s\n" "tests" "$n"
n=$(find ui-cli/assets -name '*.html' | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
printf "%-12s %s\n" "前端" "$n"
n=$(find tools -name '*.sh' -o -name '*.mjs' -o -name '*.py' | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
printf "%-12s %s\n" "tools" "$n"
n=$(find docs -name '*.md' | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
printf "%-12s %s\n" "docs" "$n"
echo
echo "== 模块 =="
ls ui-core/src/*.rs | xargs -n1 basename | tr '\n' ' '
echo
ls ui-cli/src/*.rs | xargs -n1 basename | tr '\n' ' '
echo
echo
echo "== 文档 =="
ls docs/
echo
echo "== 二进制 =="
ls -lh "$HOME/ui-target/release/ui" 2>/dev/null | awk '{print "ui  " $5}'
echo
echo "== 命令清单 =="
"$HOME/ui/target/release/ui" 2>&1 | grep -oE 'ui [a-z]+' | sort -u | tr '\n' ' '
echo
echo
echo "== 测试/门禁 =="
echo "测试: $(bash tools/count_tests.sh 2>/dev/null | grep -oE '[0-9]+')"
