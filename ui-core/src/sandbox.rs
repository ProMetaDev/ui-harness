//! OS 级沙箱：Linux 读侧最小根 + Landlock，Windows 用 Job Object（见 `winsandbox`）。
//!
//! Linux 的读侧视图在 `readjail.rs`：独立 user/mount namespace + chroot，只绑定
//! 系统运行时与工作区，并把隐私路径遮成不可读。Landlock 在这个视图里限制写入；
//! Windows 只有进程/资源约束，不限制文件系统。
//!
//! 零依赖：Linux 直接调 syscall，不引入 libc crate，也不用 bwrap。
//! 读侧要求 Linux 支持 user/mount namespace；Landlock 要求 5.13+。
//!
//! Landlock 规则（作用在读侧视图内）：
//!   - 系统运行时：读 + 执行
//!   - 工作区与 extra_writable：读 + 写 + 执行
//!   - /tmp：私有 tmpfs，读 + 写
//!   - 网络：默认允许；开了 `net` 就禁止 TCP bind/connect（ABI>=4）

use std::io;

/// 沙箱配置。跨平台共用，语义按平台解释。
#[derive(Clone, Debug)]
pub struct Config {
    /// 开启沙箱（Linux：写入限制；Windows：Job Object 约束）
    pub fs: bool,
    /// 禁用网络（仅 Linux Landlock ABI ≥ 4 的 TCP）
    pub net: bool,
    /// 额外允许写的目录（例如构建缓存）
    pub extra_writable: Vec<String>,
    /// Linux 读侧白名单视图（工作区隐私路径遮罩；Windows 暂不支持）
    pub read: bool,
    /// Linux 读侧白名单根。工作区与 extra_writable 会自动加入。
    pub read_roots: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            fs: true,
            net: false,
            extra_writable: Vec::new(),
            read: true,
            read_roots: default_read_roots(),
        }
    }
}

/// 默认允许子进程读取的系统运行时。PATH 里的其它工具目录也加入，避免
/// 用户装在 HOME 下的 node/cargo 在读侧视图里突然消失；这些目录仍是只读。
pub fn default_read_roots() -> Vec<String> {
    let mut roots: Vec<String> = ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.is_absolute() && dir.exists() {
                roots.push(dir.to_string_lossy().to_string());
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

// ─────────────────────────── Linux 实现 ───────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::Config;
    use std::ffi::CString;
    use std::io;
    use std::os::raw::{c_char, c_int, c_long, c_ulong};

    // syscall 号（x86_64 与 aarch64 相同）
    pub const NR_CREATE_RULESET: isize = 444;
    pub const NR_ADD_RULE: isize = 445;
    pub const NR_RESTRICT_SELF: isize = 446;

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
    const O_PATH: c_int = 0o10_000_000;
    const O_CLOEXEC: c_int = 0o2_000_000;

    extern "C" {
        fn syscall(num: c_long, ...) -> c_long;
        fn prctl(option: c_int, arg2: c_ulong, arg3: c_ulong, arg4: c_ulong, arg5: c_ulong) -> c_int;
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

    pub fn abi_version() -> Option<u32> {
        let rc = unsafe {
            syscall(
                NR_CREATE_RULESET as c_long,
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

    struct Rule {
        path: CString,
        access: u64,
    }

    fn build_rules(workspace: &str, cfg: &Config) -> Vec<Rule> {
        let mut rules = vec![
            // 整个文件系统：读 + 执行（否则 exec 不了 /bin/sh）
            Rule {
                path: CString::new("/").unwrap(),
                access: FS_READ_FILE | FS_READ_DIR | FS_EXECUTE,
            },
            Rule {
                path: CString::new(workspace).unwrap_or_else(|_| CString::new("/").unwrap()),
                access: u64::MAX,
            },
            Rule {
                path: CString::new("/tmp").unwrap(),
                access: u64::MAX,
            },
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

    /// 返回可在 `Command::pre_exec` 里执行的闭包：在 exec 之前上锁。
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
            if prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(wrap(io::Error::last_os_error(), "prctl(NO_NEW_PRIVS)"));
            }

            let attr = RulesetAttr {
                handled_access_fs: mask,
                handled_access_net: net.unwrap_or(0),
            };
            let rfd = syscall(
                NR_CREATE_RULESET as c_long,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0 as c_long,
            );
            if rfd < 0 {
                return Err(wrap(io::Error::last_os_error(), "landlock_create_ruleset"));
            }
            let rfd = rfd as c_int;

            for rule in &rules {
                let pfd = open(rule.path.as_ptr(), O_PATH | O_CLOEXEC);
                if pfd < 0 {
                    continue; // 路径不存在就跳过，不因此让命令失败
                }
                let pb = PathBeneathAttr {
                    allowed_access: rule.access & mask,
                    parent_fd: pfd,
                };
                let rc = syscall(
                    NR_ADD_RULE as c_long,
                    rfd as c_long,
                    RULE_PATH_BENEATH as c_long,
                    &pb as *const PathBeneathAttr,
                    0 as c_long,
                );
                close(pfd);
                if rc < 0 {
                    let e = io::Error::last_os_error();
                    close(rfd);
                    return Err(wrap(e, "landlock_add_rule"));
                }
            }

            if syscall(NR_RESTRICT_SELF as c_long, rfd as c_long, 0 as c_long) < 0 {
                let e = io::Error::last_os_error();
                close(rfd);
                return Err(wrap(e, "landlock_restrict_self"));
            }
            close(rfd);
            Ok(())
        })
    }

    fn wrap(e: io::Error, what: &str) -> io::Error {
        io::Error::new(e.kind(), format!("{}: {}", what, e))
    }
}

// ─────────────────────────── 跨平台门面 ───────────────────────────

/// 这个平台支持到哪一版 Landlock（非 Linux 恒为 None）。
pub fn abi_version() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        imp::abi_version()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 给人看的描述，进 doctor 与工具 meta。
pub fn describe(cfg: &Config) -> String {
    #[cfg(target_os = "linux")]
    {
        match imp::abi_version() {
            None => "不可用（内核不支持 Landlock）".to_string(),
            Some(abi) => {
                if !cfg.fs {
                    return format!("关闭（内核支持 ABI {}，用 UI_SANDBOX=0 关的）", abi);
                }
                let mut s = format!("Landlock ABI {} · 写入限制在工作区", abi);
                if cfg.read {
                    s.push_str(" · 读侧最小根（隐私路径遮罩）");
                } else {
                    s.push_str(" · 读侧未限制");
                }
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
    #[cfg(windows)]
    {
        if !cfg.fs {
            return "关闭（UI_SANDBOX=0）".to_string();
        }
        // Windows 的约束由 Job Object 提供，不含文件系统隔离
        crate::winsandbox::describe(&crate::winsandbox::Limits::from_env())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = cfg;
        "不支持（未知平台）".to_string()
    }
}

/// 准备一个在子进程 exec 前执行的钩子。非 Linux 平台返回 Unsupported。
pub fn pre_exec(
    workspace: &str,
    cfg: &Config,
) -> io::Result<Box<dyn FnMut() -> io::Result<()> + Send + Sync + 'static>> {
    #[cfg(target_os = "linux")]
    {
        let hook = imp::pre_exec(workspace, cfg)?;
        Ok(Box::new(hook))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (workspace, cfg);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "这个平台没有 Landlock；Windows 用 Job Object（见 winsandbox.rs）",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_is_never_empty() {
        let d = describe(&Config::default());
        assert!(!d.is_empty(), "describe 不该返回空串");
        println!("sandbox: {}", d);
    }

    #[test]
    fn abi_is_consistent_with_description() {
        let abi = abi_version();
        let d = describe(&Config::default());
        #[cfg(target_os = "linux")]
        {
            if abi.is_some() {
                assert!(d.contains("Landlock"), "{}", d);
            } else {
                assert!(d.contains("不可用"), "{}", d);
            }
        }
        #[cfg(not(target_os = "linux"))]
        assert!(abi.is_none());
    }
}
