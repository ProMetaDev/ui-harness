//! OS 级沙箱（Linux Landlock）：把 shell 子进程的写入限制在工作区内。
//!
//! 为什么需要：效果策略只决定「这条命令要不要执行」，命令内部做什么管不了。
//! `effect/observed` 是**事后记账**；Landlock 是**事前阻止**——两者互补。
//!
//! 零依赖实现：直接调 syscall，不用 libc crate，也不用 bwrap。
//! 要求：Linux 5.13+ 且内核启用 Landlock（unprivileged，不需要 root）。
//!
//! 规则（保守但可用）：
//!   - 整个文件系统：**读 + 执行**（否则跑不了任何程序）
//!   - 工作区：读 + 写 + 执行（全部权限）
//!   - /tmp：读 + 写（编译产物、临时文件）
//!   - /dev/null：写（重定向别报错）
//!   - 网络：默认允许；开了 `net` 就禁止 TCP bind/connect（ABI>=4）

use std::ffi::CString;
use std::io;
use std::os::raw::{c_char, c_int, c_long, c_ulong};

// ── syscall 号（x86_64 与 aarch64 相同）──
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod nr {
    pub const CREATE_RULESET: isize = 444;
    pub const ADD_RULE: isize = 445;
    pub const RESTRICT_SELF: isize = 446;
}

// ── 访问权限位 ──
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13; // ABI >= 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI >= 3
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI >= 5

const NET_BIND_TCP: u64 = 1 << 0; // ABI >= 4
const NET_CONNECT_TCP: u64 = 1 << 1; // ABI >= 4

const RULE_PATH_BENEATH: c_int = 1;
const CREATE_RULESET_VERSION: u32 = 1;
const PR_SET_NO_NEW_PRIVS: c_int = 38;

const O_PATH: c_int = 0o10000000;

extern "C" {
    fn syscall(num: c_long, ...) -> c_long;
    fn prctl(option: c_int, arg2: c_ulong, arg3: c_ulong, arg4: c_ulong, arg5: c_ulong)
        -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// 这个内核支持到哪一版 Landlock。`None` = 不支持（旧内核或未启用）。
pub fn abi_version() -> Option<u32> {
    let rc = unsafe {
        syscall(
            nr::CREATE_RULESET as c_long,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION as c_long,
        )
    };
    if rc < 0 {
        None
    } else {
        Some(rc as u32)
    }
}

/// 按 ABI 版本算出这个内核算得出的权限位（不支持的位传下去会 EINVAL）。
fn fs_mask(abi: u32) -> u64 {
    let mut m = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        m |= FS_REFER;
    }
    if abi >= 3 {
        m |= FS_TRUNCATE;
    }
    if abi >= 5 {
        m |= FS_IOCTL_DEV;
    }
    m
}

#[derive(Clone, Debug)]
pub struct Config {
    /// 禁止写工作区之外
    pub fs: bool,
    /// 禁止 TCP 连接/监听（ABI >= 4）
    pub net: bool,
    /// 额外允许写的目录（例如构建缓存）
    pub extra_writable: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            fs: true,
            net: false,
            extra_writable: Vec::new(),
        }
    }
}

/// 一条规则：路径 + 允许的权限。
struct Rule {
    path: CString,
    access: u64,
}

fn build_rules(workspace: &str, cfg: &Config) -> Vec<Rule> {
    let mut rules = vec![
        // 整个文件系统只读 + 可执行：不然 exec 不了 /bin/sh
        Rule {
            path: CString::new("/").unwrap(),
            access: FS_READ_FILE | FS_READ_DIR | FS_EXECUTE,
        },
        // 工作区：全权
        Rule {
            path: CString::new(workspace).unwrap_or_else(|_| CString::new("/").unwrap()),
            access: u64::MAX,
        },
        // 临时目录：编译和临时文件都要写
        Rule {
            path: CString::new("/tmp").unwrap(),
            access: u64::MAX,
        },
        // 重定向到 /dev/null 不该报错
        Rule {
            path: CString::new("/dev/null").unwrap(),
            access: FS_WRITE_FILE | FS_READ_FILE,
        },
    ];
    for extra in &cfg.extra_writable {
        if let Ok(p) = CString::new(extra.as_str()) {
            rules.push(Rule {
                path: p,
                access: u64::MAX,
            });
        }
    }
    rules
}

/// 返回一个可以在 `Command::pre_exec` 里跑的闭包：在子进程 exec 之前上锁。
///
/// 失败会让子进程 exec 返回错误，从而整条命令失败——这是有意的：
/// **宁可命令跑不起来，也不要它跑起来却没有沙箱。**
pub fn pre_exec(
    workspace: &str,
    cfg: &Config,
) -> io::Result<impl FnMut() -> io::Result<()> + Send + Sync + 'static> {
    let abi = abi_version().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "内核不支持 Landlock（需要 Linux 5.13+ 且启用）",
        )
    })?;
    let mask = fs_mask(abi);
    let rules = build_rules(workspace, cfg);
    let net = if cfg.net && abi >= 4 {
        Some(NET_BIND_TCP | NET_CONNECT_TCP)
    } else {
        None
    };

    Ok(move || unsafe {
        // 1) 必须先声明「本次提权到此为止」，否则 restrict_self 会被拒
        if prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error().into_posix("prctl(NO_NEW_PRIVS)"));
        }

        // 2) 建规则集
        let attr = RulesetAttr {
            handled_access_fs: mask,
            handled_access_net: net.unwrap_or(0),
        };
        let rfd = syscall(
            nr::CREATE_RULESET as c_long,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0 as c_long,
        );
        if rfd < 0 {
            return Err(io::Error::last_os_error().into_posix("landlock_create_ruleset"));
        }
        let rfd = rfd as c_int;

        // 3) 逐条加规则。open(O_PATH) 拿到目录 fd，规则按 fd 生效。
        for rule in &rules {
            let pfd = open(rule.path.as_ptr(), O_PATH | 0o2000000 /* O_CLOEXEC */);
            if pfd < 0 {
                // 路径不存在（比如没有 /tmp 的极简容器）就跳过，不要因此失败
                continue;
            }
            let access = rule.access & mask;
            let pb = PathBeneathAttr {
                allowed_access: access,
                parent_fd: pfd,
            };
            let rc = syscall(
                nr::ADD_RULE as c_long,
                rfd as c_long,
                RULE_PATH_BENEATH as c_long,
                &pb as *const PathBeneathAttr,
                0 as c_long,
            );
            close(pfd);
            if rc < 0 {
                close(rfd);
                return Err(io::Error::last_os_error().into_posix("landlock_add_rule"));
            }
        }

        // 4) 上锁：从这一刻起，本进程及其所有子进程都受约束
        if syscall(nr::RESTRICT_SELF as c_long, rfd as c_long, 0 as c_long) < 0 {
            let e = io::Error::last_os_error();
            close(rfd);
            return Err(e.into_posix("landlock_restrict_self"));
        }
        close(rfd);
        Ok(())
    })
}

/// 给人看的描述，进 doctor 和工具 meta。
pub fn describe(cfg: &Config) -> String {
    match abi_version() {
        None => "不可用（内核不支持 Landlock）".to_string(),
        Some(abi) => {
            if !cfg.fs {
                return format!("关闭（内核支持 ABI {}，用 --sandbox off 关的）", abi);
            }
            let mut s = format!("Landlock ABI {} · 写入限制在工作区", abi);
            if cfg.net {
                if abi >= 4 {
                    s.push_str(" · TCP 禁止");
                } else {
                    s.push_str(" · 网络未限制（ABI < 4）");
                }
            }
            s
        }
    }
}

trait PosixMsg {
    fn into_posix(self, what: &str) -> io::Error;
}

impl PosixMsg for io::Error {
    fn into_posix(self, what: &str) -> io::Error {
        io::Error::new(self.kind(), format!("{}: {}", what, self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_some_abi_on_modern_linux() {
        // CI 容器里 Landlock 可能被 disabled，所以不断言 Some，只断言调用不 panic
        let abi = abi_version();
        println!("landlock abi = {:?}", abi);
        if let Some(v) = abi {
            assert!(v >= 1);
        }
    }

    #[test]
    fn describe_mentions_landlock_or_unavailable() {
        let d = describe(&Config::default());
        assert!(d.contains("Landlock") || d.contains("不可用"), "{}", d);
    }
}
