# Windows 沙箱方案（待实现）

Linux 用 Landlock 解决了写入限制；Windows 没有对应物，只能组合机制。
下面是把"能做"和"做不到"分清楚之后的方案，**按档实施，每档可独立验收**。

## 现状（v0.1.3）

| 平台 | 写入限制 | 进程树 | 资源上限 | 网络 |
|---|---|---|---|---|
| Linux | ✅ Landlock | ✅ 进程组 `kill(-pgid)` | ❌ | 可选禁 TCP（ABI≥4） |
| Windows | ❌ | ⚠️ `taskkill /T /F`（会被脱离/改名绕过） | ❌ | ❌ |

## A 档：Job Object（✅ 已完成，v0.1.4）

**目标**：进程树可靠回收 + 资源上限。不是文件沙箱。

实现要点（零依赖，直接声明 Win32）：

```rust
#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: u32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}
```

- `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`（class = 9）：
  - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`（0x2000）——关句柄即杀整棵树，**内核保证**
  - `JOB_OBJECT_LIMIT_PROCESS_MEMORY` / `JOB_OBJECT_LIMIT_JOB_MEMORY`——单进程/整组内存上限
  - `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`——进程数上限（防 fork 炸弹）
  - `JOB_OBJECT_LIMIT_JOB_TIME`——CPU 总时间上限
- `JOBOBJECT_BASIC_UI_RESTRICTIONS`（class = 4）：禁剪贴板、禁切换桌面、禁 handle 继承
- **注意**：`AssignProcessToJobObject` 要在子进程创建后、跑起来之前调用。我们目前用
  `Command::spawn`，拿到 pid 之后立刻 Assign——有极短窗口，但 Job 的 kill-on-close 仍然有效。
  更严格的做法是 `CREATE_SUSPENDED` + Assign + `ResumeThread`，需要改成手动 `CreateProcessW`。

**实测（2026-11-17，Windows 真机）**：

```
=== A: timeout kills the tree ===
ping during run=1 (before=0)
ping after exit=0
PASS child ran (1) and the whole tree was reclaimed

=== B: normal exit must not orphan (start /b) ===
ping before=0 after=0
PASS no orphan (job terminated on completion)
```

过程中发现一个真问题：**只靠 `KILL_ON_JOB_CLOSE` 不够**——命令正常结束（`start /b`
立刻返回）时，后台子进程会活下来。改成**正常结束时也显式 `TerminateJobObject`** 后，
B 才通过。回归脚本：`tools/verify_job_windows.ps1`。

**验收**：
- `shell` 跑 `cmd /C "start /b ping -n 60 127.0.0.1"` 后取消，**整棵树消失**（用 `tasklist` 验证）；
- 跑一个吃内存的循环，被 `PROCESS_MEMORY` 上限杀掉，门禁不挂机器；
- `ui doctor` 在 Windows 上打印实际生效的限制。

## B 档：低完整性令牌（+1~1.5 天）

**目标**：真正的写入限制。

- `SetTokenInformation(hToken, TokenIntegrityLevel(25), &TIL, size)` 把令牌降到
  `SECURITY_MANDATORY_LOW_RID (0x1000)`；
- 低完整性进程**写不了**中等完整性的文件（用户目录、工作区默认都是中等）；
- 要让工作区可写，二选一：
  1. 把**工作区目录**的完整性标签降到 Low（`icacls <ws> /setintegritylevel (OI)(CI)L`）；
  2. 或者给 Low 完整性 SID 加写 ACL（`icacls <ws> /grant *S-1-16-4096:(OI)(CI)M`）。
- 两者都会留下**副作用**：目录标签/ACL 被改过，需要在 `ui doctor` 里提示，且提供撤销命令。

**风险**（这是估计上浮的主因）：
- `cargo build` 之类的工具在被降完整性后可能因为临时目录（`%TEMP%` 是中等）而失败；
- NTFS 的继承规则在这里很绕，`(OI)(CI)` 写错会导致新建文件仍然中等完整性；
- Windows 的 `%TEMP%` 也要放行，否则我们踩过的"临时目录不可写"会复现。

**验收**：
- `shell` 写 `%USERPROFILE%\evil.txt` 被拒；
- `shell` 写工作区内正常；
- `cargo build` 在工作区内仍然能跑（这条最容易挂）；
- 撤销命令能把目录完整性/ACL 恢复原状（用 `icacls` 前后对比）。

## C 档：AppContainer（+2~3 天，暂不做）

- `CreateAppContainerProfile` 建容器 → `SECURITY_CAPABILITIES` → `STARTUPINFOEX` 里带
  `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` → `CreateProcessW`；
- 能力控制网络（`internetClient` / `privateNetworkClientServer`）；
- 收益是"进程级真沙箱 + 网络按能力"，但调试成本最高（只能靠 PowerShell 观察 `ui.exe`）。

**先不做**，理由是当前没有"必须隔离网络"的使用场景。

## 验证方式（Windows 侧只能这样验）

- 从 WSL 交叉编译 → `out/win/ui.exe` → 在 PowerShell 里跑脚本；
- 每条断言的产物：`tasklist` 输出、`icacls` 前后对比、`ui doctor` 的沙箱行；
- **不能**用 strace/Process Monitor 这类工具（没有），所以结论以"行为"为准，不以"调用"为准。

## 做完要同步的地方

1. `ui doctor`：Windows 那一段从"n/a"变成实际限制描述；
2. `README` 与 `SECURITY.md`：把"Windows 没有对等机制"改成实际做到的程度；
3. `docs/22`：验证包加第 9 场景（Windows 写区外被拒）——但要注意它只在 Windows 上有意义，
   Linux CI 上应该 SKIP；
4. `docs/23`：完成度表里 Windows 那格更新。
