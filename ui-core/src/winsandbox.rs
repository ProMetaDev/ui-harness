//! Windows 进程约束（Job Object）。
//!
//! Windows 没有 Landlock 的等价物，但 Job Object 能保证三件长任务最怕的事：
//!   1. **整棵树必然回收**：`KILL_ON_JOB_CLOSE` 由内核保证，关掉句柄就全杀，
//!      子进程改名/脱离都躲不掉（`taskkill /T` 做不到这个保证）；
//!   2. **内存上限**：单个子进程和整个 job 的内存都有硬顶，防止把机器拖死；
//!   3. **进程数上限**：防止 fork 炸弹式的脚本。
//!
//! 它**不做**文件系统隔离——那需要低完整性令牌或 AppContainer，见 docs/25。
//!
//! 零依赖：直接声明 kernel32 的函数，不引入 windows-sys。

#![cfg(windows)]

use std::io;
use std::os::raw::{c_int, c_void};

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: c_int, info: *const c_void, len: u32) -> c_int;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> c_int;
    fn OpenProcess(access: u32, inherit: c_int, pid: u32) -> *mut c_void;
    fn TerminateJobObject(job: *mut c_void, code: u32) -> c_int;
    fn CloseHandle(h: *mut c_void) -> c_int;
}

// 用 NtResumeProcess 而不是 ResumeThread：std::process::Child 不暴露主线程
// 句柄，而 CREATE_SUSPENDED 挂起的正是主线程。NtResumeProcess 是 ntdll 的
// 稳定接口，按进程恢复所有线程，正好覆盖这个场景。
#[link(name = "ntdll")]
extern "system" {
    fn NtResumeProcess(process: *mut c_void) -> i32;
}

const PROCESS_SET_QUOTA: u32 = 0x0100;
const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_SUSPEND_RESUME: u32 = 0x0800;
pub const CREATE_SUSPENDED: u32 = 0x0000_0004;

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: c_int = 9;
const JOB_OBJECT_BASIC_UI_RESTRICTIONS_CLASS: c_int = 4;

const LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
const LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
const LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
const LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

const UI_LIMIT_DESKTOP: u32 = 0x0000_0040;
const UI_LIMIT_EXIT_WINDOWS: u32 = 0x0000_0080;

#[repr(C)]
#[derive(Default)]
struct BasicLimitInfo {
    per_process_user_time: i64,
    per_job_user_time: i64,
    limit_flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_ops: u64,
    write_ops: u64,
    other_ops: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInfo {
    basic: BasicLimitInfo,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[derive(Clone, Debug)]
pub struct Limits {
    /// 单进程内存上限（字节），0 = 不限
    pub process_memory: usize,
    /// 整个 job 内存上限（字节），0 = 不限
    pub job_memory: usize,
    /// 同时存在的进程数上限，0 = 不限
    pub active_processes: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        // 默认给一个"够用但不会拖死机器"的顶：单进程 4 GiB、整组 8 GiB、512 个进程
        Limits {
            process_memory: 4 * 1024 * 1024 * 1024,
            job_memory: 8 * 1024 * 1024 * 1024,
            active_processes: 512,
        }
    }
}

impl Limits {
    /// 从环境变量读，便于在不改代码的情况下调：
    ///   UI_JOB_PROCESS_MB / UI_JOB_TOTAL_MB / UI_JOB_PROCS（0 = 不限）
    pub fn from_env() -> Limits {
        let d = Limits::default();
        let num = |k: &str, fallback: usize| -> usize {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(fallback)
        };
        let process_mb = num("UI_JOB_PROCESS_MB", d.process_memory / (1024 * 1024));
        let total_mb = num("UI_JOB_TOTAL_MB", d.job_memory / (1024 * 1024));
        let procs = num("UI_JOB_PROCS", d.active_processes as usize) as u32;
        Limits {
            process_memory: process_mb * 1024 * 1024,
            job_memory: total_mb * 1024 * 1024,
            active_processes: procs,
        }
    }
}

/// 持有 job 句柄。Drop 时关闭句柄，`KILL_ON_JOB_CLOSE` 会顺手收掉残留进程。
pub struct Job {
    handle: *mut c_void,
}

// 句柄可以跨线程传递（我们会把它留在 shell 的等待循环里）
unsafe impl Send for Job {}

impl Job {
    /// 给已启动的子进程套上 job。失败返回 Err，调用方自行决定是否回退。
    pub fn assign(pid: u32, limits: &Limits) -> io::Result<Job> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            let mut info = ExtendedLimitInfo::default();
            info.basic.limit_flags = LIMIT_KILL_ON_JOB_CLOSE;
            if limits.process_memory > 0 {
                info.basic.limit_flags |= LIMIT_PROCESS_MEMORY;
                info.process_memory_limit = limits.process_memory;
            }
            if limits.job_memory > 0 {
                info.basic.limit_flags |= LIMIT_JOB_MEMORY;
                info.job_memory_limit = limits.job_memory;
            }
            if limits.active_processes > 0 {
                info.basic.limit_flags |= LIMIT_ACTIVE_PROCESS;
                info.basic.active_process_limit = limits.active_processes;
            }
            let rc = SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                &info as *const ExtendedLimitInfo as *const c_void,
                std::mem::size_of::<ExtendedLimitInfo>() as u32,
            );
            if rc == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }

            // UI 限制：不让子进程碰桌面、不让它关机
            let ui_flags: u32 = UI_LIMIT_DESKTOP | UI_LIMIT_EXIT_WINDOWS;
            let rc = SetInformationJobObject(
                handle,
                JOB_OBJECT_BASIC_UI_RESTRICTIONS_CLASS,
                &ui_flags as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );
            if rc == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }

            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 1, pid);
            if proc.is_null() {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }
            let rc = AssignProcessToJobObject(handle, proc);
            CloseHandle(proc);
            if rc == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }

            Ok(Job { handle })
        }
    }

    /// 恢复一个用 CREATE_SUSPENDED 启动的进程。必须在 AssignProcessToJobObject
    /// 成功之后调用；调用方负责在失败时 kill/terminate。
    pub fn resume(pid: u32) -> io::Result<()> {
        unsafe {
            let proc = OpenProcess(PROCESS_SUSPEND_RESUME, 1, pid);
            if proc.is_null() {
                return Err(io::Error::last_os_error());
            }
            let status = NtResumeProcess(proc);
            CloseHandle(proc);
            if status < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("NtResumeProcess failed: 0x{:08x}", status as u32),
                ));
            }
            Ok(())
        }
    }

    /// 立刻杀掉 job 里的所有进程。
    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// 这个句柄对应的 job 是否可用（doctor 用）。
pub fn available() -> bool {
    unsafe {
        let h = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

pub fn describe(limits: &Limits) -> String {
    if !available() {
        return "不可用（无法创建 Job Object）".into();
    }
    format!(
        "Job Object · 进程树强制回收 · 内存 单进程 {} MiB / 整组 {} MiB · 进程数 {}",
        limits.process_memory / (1024 * 1024),
        limits.job_memory / (1024 * 1024),
        if limits.active_processes == 0 {
            "不限".to_string()
        } else {
            limits.active_processes.to_string()
        }
    )
}
