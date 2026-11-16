#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
TASK=$(cat fixtures/acceptance-task2.txt)
node -e '
const fs=require("fs");
const task=fs.readFileSync("fixtures/acceptance-task2.txt","utf8");
fs.writeFileSync("/tmp/adapter-in2.json", JSON.stringify({prompt: task, repair: null}));
'
echo "--- 输入 $(( $(wc -c < /tmp/adapter-in2.json) )) 字节 ---"
bash tools/_node.sh tools/deepseek.mjs < /tmp/adapter-in2.json > /tmp/adapter-out2.json 2> /tmp/adapter-err2.txt
echo "exit=$?"
echo "--- stdout ---"
head -c 300 /tmp/adapter-out2.json; echo
echo "--- stderr ---"
head -c 600 /tmp/adapter-err2.txt
