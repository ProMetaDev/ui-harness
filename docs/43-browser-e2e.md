# 真实浏览器 e2e（CDP，零 npm）

> 工具：[tools/browser_e2e.py](tools/browser_e2e.py)
>
> 不引入 Playwright/Puppeteer/npm；用 Chrome/Edge 的 DevTools Protocol + Python 标准库。

## 1. 运行

```powershell
# 先构建 Windows 二进制
bash tools/build_windows.sh
copy out\ui.exe ...
python tools/browser_e2e.py --ui out\ui.exe
```

Linux CI 没有 Chrome 时脚本明确 `SKIP`。

## 2. 检查内容

1. 启动 `ui serve --token off`；
2. 启动 headless Chrome/Edge；
3. 通过 CDP `Runtime.evaluate` 检查：
   - `.shell` 已挂载；
   - `#diffBtn` 存在；
   - 按钮文本包含「变更」；
4. 点击「变更」，确认 `#diffOverlay` 真的打开（无 checkpoint 时显示明确空态）；
5. `Page.navigate` 到 `/admin`，确认真实 DOM 有「管理面」、至少 4 个 `.card`、
   角色矩阵表与用户表单（`userForm`/`matrixBody`/`userBody`）；
6. `taskkill /T` 清理浏览器与 server 进程树。

## 3. 实测

```text
PASS browser e2e: title=... diff=ok overlay=ok admin=ok forms=ok
```

## 4. 边界

- 只验证首屏挂载 + 变更面板，不覆盖完整输入/SSE 流程；
- 没有 CI 步骤；本机 Windows 实测通过；
- 浏览器路径是 Windows 常见安装位置；找不到则 SKIP。
