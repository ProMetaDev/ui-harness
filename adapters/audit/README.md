# 审计导出签名与锚定（L2，可选）

> 不属于 `ui-core`；不引入 Rust crate / npm / Python 第三方包。
> 需要 `python3` 和支持 `-Y sign` / `-Y verify` 的 OpenSSH（8.1+）。
> 设计、威胁边界与 key 轮换见 [docs/49](../../docs/49-audit-sign.md)。

## 文件

- `sign_export.py`：用 `ssh-keygen -Y sign` 给 `/api/audit/export` 的 JSON 生成
  detached `.sig`，用 `allowed_signers` 验签；不修改导出文件，不打印私钥内容；
- `anchor.py`：append-only `anchors.jsonl`，每条记录
  `{"seq","sha256","prev","ts"}`，`sha256` 是导出文件哈希，`prev` 是上一条
  记录的哈希；提供 `append` / `verify` / `head`。

## 用法

```bash
# 1) 签名
python3 adapters/audit/sign_export.py sign \
  --export export.json --key ~/.ssh/audit_ed25519

# 2) 验签
python3 adapters/audit/sign_export.py verify \
  --export export.json --sig export.json.sig \
  --allowed-signers allowed_signers --identity auditor@example.com

# 3) 追加锚点并校验
python3 adapters/audit/anchor.py append --anchors anchors.jsonl --export export.json
python3 adapters/audit/anchor.py verify --anchors anchors.jsonl
python3 adapters/audit/anchor.py head --anchors anchors.jsonl --print head
```

## 边界

- 没有 `ssh-keygen` 时 `sign_export.py` 打印 `SKIP:` 并退出 2，不降级；
- `anchor.py` 的 0444 权限 + `O_APPEND` 只是降低误改概率，**不是 WORM**，
  也不是对象锁；owner/root 仍可修改。没有外部记录的 head 时，尾部截断不可知；
- 签名覆盖导出 JSON 的原始字节；重新格式化 JSON 会导致验签失败；
- `ts` 是本机时钟，不是可信时间戳。

## 测试

```bash
bash tools/verify_audit_sign.sh
```

覆盖签名通过、篡改失败、identity/namespace 绑定、锚定链通过、改中间条失败、
截断边界，以及无 `ssh-keygen` 时 SKIP。
