# 本地 WORM 风格对象库（L2，可选）

> 不属于 `ui-core`；不改 server/UI；只用 Python 标准库（无第三方包）。
> 设计、威胁边界、保留期语义与 `/api/audit/export` 接入方式见
> [docs/52](../../docs/52-worm.md)。

## 文件

- `store.py`：内容寻址不可变对象库。
  - `put(bytes, retention_until_epoch) -> sha256 id`；
  - `get(id)` / `delete(id)` / `verify()` / `list_objects()`；
  - 对象 `objects/<sha256>` + sidecar `objects/<sha256>.meta.json`；
  - append-only `index.jsonl`（`put` / `delete` / `tombstone`，带 `prev` 哈希链）；
  - 墓碑 `tombstones/<sha256>.json`；保留期到期前 `delete` 拒绝；
  - 写入 `0444` + `fsync`，POSIX 上 `flock` 单写者；Windows 为单写者约定；
  - CLI：`put` / `get` / `delete` / `verify` / `list`。

## 用法

```bash
python3 adapters/worm/store.py put --store /srv/ui/worm \
  --file export.json --retention-days 365
python3 adapters/worm/store.py get --store /srv/ui/worm <id> --out export.json
python3 adapters/worm/store.py delete --store /srv/ui/worm <id>
python3 adapters/worm/store.py verify --store /srv/ui/worm
python3 adapters/worm/store.py list --store /srv/ui/worm --all --json
```

退出码：0 成功 / 1 输入或 I/O 错误 / 2 完整性失败或保留策略拒绝 / 64 用法错误。

**测试时钟**：`--now <epoch>` 只用于测试保留期，不出现在 `--help` 里；必须显式设置
`UI_WORM_TEST_CLOCK=1` 才被接受，否则退出 2（提示 test-only）。生产调用不要设置它。

```bash
# 只允许测试这样改时钟：
UI_WORM_TEST_CLOCK=1 python3 adapters/worm/store.py delete --store /srv/ui/worm <id> --now 1800000000
```

## 边界

- **这是本地 WORM 风格，不是 S3 Object Lock，也不是合规 WORM。** 没有服务端
  强制、没有 legal hold、没有可信时间戳；
- 保留期只是本工具在 `delete` 时检查本机时钟；owner/root/备份系统仍可修改文件；
- `0444` 是降低误改概率的约定，不是强制只读；
- Windows 没有 `fcntl`：同一时刻只允许一个写进程；
- 删除后的 id 不能重新 `put`（tombstone 拒绝“复活”）；
- `verify` 能发现单点篡改与大多数截断，但挡不住“整库重写并重算链”。

## 测试

```bash
bash tools/verify_worm.sh
```

覆盖 put/get 一致、重复 put 不覆盖、保留期内 delete 失败、到期删除 + tombstone、
篡改对象/sidecar/index 后 verify 失败、`--now` 未开测试时钟时被拒、无 `python3` 时 SKIP。
