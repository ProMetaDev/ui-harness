
# 仓库、备份与恢复

## 位置

| 内容 | 路径 |
|---|---|
| 源码（唯一开发副本） | `X:\dsh_ws\harness-v2` = `/mnt/x/dsh_ws/harness-v2` |
| 构建副本 | `~/ui`（`tools/dev.sh` 每次 rsync） |
| 编译产物 | `~/ui-target` |
| 备份（WSL ext4） | `~/backups/harness-v2-<时间>.tar.gz` |
| 备份镜像（Windows 系统盘） | `<用户目录>\backups\` |

三份在不同物理盘上：X:（源码盘）、WSL 的 `/dev/sdd`、C:（系统盘）。

## 备份

```bash
bash tools/backup.sh          # 打包 + 校验 + 镜像 + 保留最近 7 份
```

归档包含 `.git` 历史；排除 `out/`、`target/`、`.ui/`。

## 从备份恢复

```bash
cd ~/backups
tar xzf harness-v2-20261116-173753.tar.gz -C /tmp/restore
cd /tmp/restore/harness-v2
git log --oneline | head -3      # 历史也在
bash tools/test.sh               # 先验证再覆盖原目录
```

## 提交

```bash
git add -A && git commit -m "..."
```

`.gitignore` 已排除 `secrets/`、`out/`、`target/`、`.ui/`、`.env*`、`*.key`。
提交前 `tools/git_init.sh` 会检查暂存区里有没有密钥路径，命中就中止。
