//! Linux 读侧文件视图：把子进程放进独立挂载命名空间与 chroot。
//!
//! Landlock 的路径规则只能叠加允许，不能在工作区的大规则里挖掉 `secrets/`。
//! 因此读侧先在子进程里搭一个最小根：只绑定系统运行时、工作区和显式追加的
//! 可写目录；工作区内的隐私/敏感路径用不可读空目录或空文件遮住。随后丢弃
//! capability 并用 seccomp 禁止重新挂载/换命名空间，避免从 bind mount 绕回宿主。
//! 不挂载 procfs，杜绝 `/proc/self/root` 等 magic link。Landlock 仍负责这个视图里
//! 的写限制与网络限制。
//!
//! 这一层只支持 Linux x86_64/aarch64。拿不到 user/mount namespace 时失败关闭，
//! 不会静默退回“整盘可读”。

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
const MAX_VIEW_ENTRIES: usize = 200_000;

#[cfg(target_os = "linux")]
struct BindRoot {
    source: PathBuf,
    target: PathBuf,
    writable: bool,
}

#[cfg(target_os = "linux")]
struct Mask {
    /// 相对于 chroot 根的挂载点。
    target: PathBuf,
    directory: bool,
}

#[cfg(target_os = "linux")]
fn root_dir() -> io::Result<PathBuf> {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    if let Some(root) = ROOT.get() {
        return Ok(root.clone());
    }
    let root = std::env::temp_dir().join(format!("ui-harness-read-view-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty)?;
    std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o000))?;
    let empty_file = root.join("empty-file");
    if !empty_file.exists() {
        std::fs::File::create(&empty_file)?;
    }
    std::fs::set_permissions(&empty_file, std::fs::Permissions::from_mode(0o000))?;
    std::fs::create_dir_all(root.join("proc"))?;
    let _ = ROOT.set(root.clone());
    Ok(root)
}

#[cfg(target_os = "linux")]
fn jail_path(root: &Path, absolute: &Path) -> io::Result<PathBuf> {
    let rel = absolute
        .strip_prefix("/")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "读侧白名单必须使用绝对路径"))?;
    Ok(root.join(rel))
}

#[cfg(target_os = "linux")]
fn collect_masks(workspace: &Path, root: &Path, out: &mut Vec<Mask>) -> io::Result<()> {
    let mut stack = vec![workspace.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        seen += 1;
        if seen > MAX_VIEW_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "读侧视图超过 {} 个目录项，拒绝在不完整隐私遮罩下执行",
                    MAX_VIEW_ENTRIES
                ),
            ));
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                // 无法检查的目录无法证明里面没有隐私项，整棵遮住。
                let target = jail_path(root, &dir)?;
                out.push(Mask {
                    target,
                    directory: true,
                });
                continue;
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => {
                    // symlink_metadata 失败时无法分类；存在性本身就该 fail closed。
                    if path.symlink_metadata().is_ok() {
                        out.push(Mask {
                            target: jail_path(root, &path)?,
                            directory: false,
                        });
                    }
                    continue;
                }
            };
            // entry.metadata 不跟随符号链接。别名指向的隐私目标会在真实目标
            // 处被遮住；这里不再跟随，避免环与越界挂载。
            if crate::protected::is_sensitive_or_private(workspace, &path) {
                if !meta.file_type().is_symlink() {
                    out.push(Mask {
                        target: jail_path(root, &path)?,
                        directory: meta.is_dir(),
                    });
                }
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }

    // 工作区外的绝对声明：只有落在本次绑定的白名单根里才需要遮；未绑定的
    // 路径在 chroot 中本来就不存在。
    for declared in crate::protected::declared_absolute_paths() {
        // 声明的符号链接也必须遮住真实目标，否则 `/etc/private-link` 仍是
        // 同一份凭据的可读别名。未绑定的目标在 chroot 中本来不可达。
        let Ok(resolved) = declared.canonicalize() else {
            continue;
        };
        let Ok(meta) = resolved.symlink_metadata() else {
            continue;
        };
        out.push(Mask {
            target: jail_path(root, &resolved)?,
            directory: meta.is_dir(),
        });
    }
    Ok(())
}

/// 在父进程准备挂载描述；真正的 namespace/chroot 动作在 `pre_exec` 中完成。
#[cfg(target_os = "linux")]
pub fn prepare(
    workspace: &Path,
    cfg: &crate::sandbox::Config,
) -> io::Result<Box<dyn FnMut() -> io::Result<()> + Send + Sync + 'static>> {
    if !cfg.read {
        return Ok(Box::new(|| Ok(())));
    }
    if !cfg!(target_arch = "x86_64") && !cfg!(target_arch = "aarch64") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Linux 读侧沙箱目前只支持 x86_64/aarch64",
        ));
    }
    let workspace = workspace.canonicalize()?;
    if workspace == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "拒绝把整个文件系统根作为工作区读侧视图",
        ));
    }
    let root = root_dir()?;
    let empty = root.join("empty");
    std::fs::create_dir_all(root.join("proc"))?;
    std::fs::create_dir_all(root.join("tmp"))?;
    std::fs::create_dir_all(root.join("dev"))?;
    // merged-/usr 发行版只绑定 /usr 一次；保留 /bin、/lib*、/sbin 的兼容
    // 软链，否则动态加载器与 execvp 找不到 `/bin/sh`、`/lib64/ld-*`。
    for (name, dest) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        let link = root.join(name);
        if !link.exists() && !link.symlink_metadata().is_ok() {
            let _ = std::os::unix::fs::symlink(dest, link);
        }
    }
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let target = root.join("dev").join(name);
        if !target.exists() {
            let _ = std::fs::File::create(&target);
        }
    }

    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    for source in &cfg.read_roots {
        match Path::new(source).canonicalize() {
            Ok(path) => paths.push((path, false)),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("读侧白名单不存在：{}", source),
                ))
            }
        }
    }
    paths.push((workspace.clone(), true));
    for extra in &cfg.extra_writable {
        let path = PathBuf::from(extra)
            .canonicalize()
            .map_err(|e| io::Error::new(e.kind(), format!("额外可写目录不可用：{}", extra)))?;
        paths.push((path, true));
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    paths.dedup_by(|a, b| a.0 == b.0);
    // 父目录已经覆盖子目录；保留一个就够，避免在只读 bind 上重复挂载。
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    for (path, writable) in paths {
        if path == Path::new("/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "读侧白名单不能包含文件系统根 /",
            ));
        }
        if path == Path::new("/tmp") {
            // /tmp 在子进程里始终是独立空 tmpfs，不把宿主 /tmp（连同视图
            // 根目录自身）递归绑回来。
            continue;
        }
        if let Some((_, parent_writable)) =
            roots.iter().find(|(parent, _)| path.starts_with(parent))
        {
            if !writable || *parent_writable {
                continue;
            }
        }
        roots.push((path, writable));
    }
    let mut binds = Vec::new();
    for (source, writable) in roots {
        let target = jail_path(&root, &source)?;
        std::fs::create_dir_all(&target)?;
        binds.push(BindRoot {
            source,
            target,
            writable,
        });
    }

    let mut masks = Vec::new();
    collect_masks(&workspace, &root, &mut masks)?;
    // 绝对隐私声明只处理真的进入 chroot 白名单的那些；未绑定的路径本来就
    // 不可达，不必为它准备一个不存在的挂载点。
    let bound = |p: &Path| binds.iter().any(|b| p.starts_with(&b.source));
    masks.retain(|mask| {
        let Ok(rel) = mask.target.strip_prefix(&root) else {
            return false;
        };
        let abs = Path::new("/").join(rel);
        abs.starts_with(&workspace) || bound(&abs)
    });

    let root_c = CString::new(root.as_os_str().as_bytes()).unwrap();
    let workspace_c = CString::new(workspace.as_os_str().as_bytes()).unwrap();
    Ok(Box::new(move || unsafe {
        enter(&root_c, &workspace_c, &binds, &masks, &empty)
    }))
}

#[cfg(target_os = "linux")]
unsafe fn enter(
    root: &CString,
    workspace: &CString,
    binds: &[BindRoot],
    masks: &[Mask],
    empty: &Path,
) -> io::Result<()> {
    const CLONE_NEWUSER: c_int = 0x1000_0000u32 as i32;
    const CLONE_NEWNS: c_int = 0x0002_0000;
    const MS_NOSUID: c_ulong = 2;
    const MS_NODEV: c_ulong = 4;
    const MS_NOEXEC: c_ulong = 8;
    const MS_BIND: c_ulong = 4096;
    const MS_PRIVATE: c_ulong = 1 << 18;
    const MS_REC: c_ulong = 16384;

    extern "C" {
        fn unshare(flags: c_int) -> c_int;
        fn mount(
            source: *const c_char,
            target: *const c_char,
            fstype: *const c_char,
            flags: c_ulong,
            data: *const c_void,
        ) -> c_int;
        fn chroot(path: *const c_char) -> c_int;
        fn chdir(path: *const c_char) -> c_int;
        fn getuid() -> u32;
        fn getgid() -> u32;
    }
    use std::os::raw::{c_char, c_int, c_ulong, c_void};

    let uid = getuid();
    let gid = getgid();
    if unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0 {
        return Err(wrap(io::Error::last_os_error(), "unshare(user,mount)"));
    }
    // 新 user namespace 里把当前用户映射成 root；只需要一个 id，避免依赖
    // /etc/subuid 或 newuidmap 这类外部工具。
    std::fs::write("/proc/self/setgroups", "deny").map_err(|e| wrap(e, "setgroups"))?;
    std::fs::write("/proc/self/uid_map", format!("0 {} 1", uid)).map_err(|e| wrap(e, "uid_map"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {} 1", gid)).map_err(|e| wrap(e, "gid_map"))?;
    if mount(
        std::ptr::null(),
        b"/\0".as_ptr().cast(),
        std::ptr::null(),
        MS_REC | MS_PRIVATE,
        std::ptr::null(),
    ) != 0
    {
        return Err(wrap(io::Error::last_os_error(), "mount propagation"));
    }

    // 先建私有 /tmp 与 /dev，再绑定白名单。测试工作区和自己装在 /tmp 下的
    // 工具因此不会被后挂的 tmpfs 遮掉；/tmp 本身永远是新的空 tmpfs。
    let tmp_target = CString::new(format!("{}/tmp", root.to_string_lossy())).unwrap();
    if mount(
        b"tmpfs\0".as_ptr().cast(),
        tmp_target.as_ptr(),
        b"tmpfs\0".as_ptr().cast(),
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        b"mode=0700\0".as_ptr().cast(),
    ) != 0
    {
        return Err(wrap(io::Error::last_os_error(), "mount private tmp"));
    }
    let dev_target = CString::new(format!("{}/dev", root.to_string_lossy())).unwrap();
    if mount(
        b"tmpfs\0".as_ptr().cast(),
        dev_target.as_ptr(),
        b"tmpfs\0".as_ptr().cast(),
        MS_NOSUID | MS_NOEXEC,
        b"mode=0755\0".as_ptr().cast(),
    ) != 0
    {
        return Err(wrap(io::Error::last_os_error(), "mount private dev"));
    }
    for name in ["null", "zero", "random", "urandom", "tty"] {
        let source = CString::new(format!("/dev/{}", name)).unwrap();
        let target = CString::new(format!("{}/dev/{}", root.to_string_lossy(), name)).unwrap();
        let _ = std::fs::File::create(std::path::Path::new(&target.to_string_lossy().to_string()));
        if mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            MS_BIND,
            std::ptr::null(),
        ) != 0
        {
            if name != "tty" {
                return Err(wrap(io::Error::last_os_error(), "bind device"));
            }
        }
    }

    for bind in binds {
        let source = CString::new(bind.source.as_os_str().as_bytes()).unwrap();
        let target = CString::new(bind.target.as_os_str().as_bytes()).unwrap();
        std::fs::create_dir_all(&bind.target).map_err(|e| wrap(e, "create bind target"))?;
        if mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            MS_BIND | MS_REC,
            std::ptr::null(),
        ) != 0
        {
            return Err(wrap(io::Error::last_os_error(), "bind read root"));
        }
        if !bind.writable {
            remount_ro(&target)?;
        }
    }

    // 不挂载 procfs：`/proc/self/root` 等 magic link 能绕过 chroot。
    // 普通 shell/工具链不依赖 proc；需要它的程序应在明确配置后另行运行。
    let empty_c = CString::new(empty.as_os_str().as_bytes()).unwrap();
    let empty_file_c =
        CString::new(empty.with_file_name("empty-file").as_os_str().as_bytes()).unwrap();

    for mask in masks {
        let target = CString::new(mask.target.as_os_str().as_bytes()).unwrap();
        let source = if mask.directory {
            empty_c.as_ptr()
        } else {
            empty_file_c.as_ptr()
        };
        if mount(
            source,
            target.as_ptr(),
            std::ptr::null(),
            MS_BIND,
            std::ptr::null(),
        ) != 0
        {
            return Err(wrap(io::Error::last_os_error(), "mask private path"));
        }
        if mask.directory {
            remount_ro(&target)?;
        }
    }

    if chroot(root.as_ptr()) != 0 || chdir(workspace.as_ptr()) != 0 {
        return Err(wrap(io::Error::last_os_error(), "chroot/chdir"));
    }
    drop_capabilities()?;
    install_seccomp()?;
    Ok(())
}

#[cfg(target_os = "linux")]
unsafe fn remount_ro(target: &CString) -> io::Result<()> {
    extern "C" {
        fn mount(
            source: *const std::os::raw::c_char,
            target: *const std::os::raw::c_char,
            fstype: *const std::os::raw::c_char,
            flags: std::os::raw::c_ulong,
            data: *const std::os::raw::c_void,
        ) -> std::os::raw::c_int;
    }
    const MS_BIND: std::os::raw::c_ulong = 4096;
    const MS_REMOUNT: std::os::raw::c_ulong = 32;
    const MS_RDONLY: std::os::raw::c_ulong = 1;
    if mount(
        std::ptr::null(),
        target.as_ptr(),
        std::ptr::null(),
        MS_BIND | MS_REMOUNT | MS_RDONLY,
        std::ptr::null(),
    ) != 0
    {
        Err(wrap(io::Error::last_os_error(), "remount read-only"))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
unsafe fn drop_capabilities() -> io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let rc = {
        extern "C" {
            fn syscall(num: std::os::raw::c_long, ...) -> std::os::raw::c_long;
        }
        #[cfg(target_arch = "x86_64")]
        let nr = 126;
        #[cfg(target_arch = "aarch64")]
        let nr = 93;
        syscall(nr, &header as *const Header, data.as_ptr())
    };
    if rc != 0 {
        Err(wrap(io::Error::last_os_error(), "capset"))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
unsafe fn install_seccomp() -> io::Result<()> {
    use std::os::raw::c_ulong;
    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_JSET: u16 = 0x40;
    const BPF_RET: u16 = 0x06;
    const BPF_K: u16 = 0x00;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const EPERM: u32 = 1;
    const ENOSYS: u32 = 38;
    const CLONE_NEWUSER: u32 = 0x1000_0000;

    #[repr(C)]
    struct Filter {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }
    #[repr(C)]
    struct Program {
        len: u16,
        filter: *const Filter,
    }

    let mut f: Vec<Filter> = Vec::new();
    let ld = |off: u32| Filter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: off,
    };
    let jeq = |k: u32, jt: u8, jf: u8| Filter {
        code: BPF_JMP | BPF_JEQ | BPF_K,
        jt,
        jf,
        k,
    };
    let jset = |k: u32, jt: u8, jf: u8| Filter {
        code: BPF_JMP | BPF_JSET | BPF_K,
        jt,
        jf,
        k,
    };
    let ret = |k: u32| Filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k,
    };

    f.push(ld(4)); // seccomp_data.arch
    #[cfg(target_arch = "x86_64")]
    f.push(jeq(0xc000_003e, 1, 0));
    #[cfg(target_arch = "aarch64")]
    f.push(jeq(0xc000_00b7, 1, 0));
    f.push(ret(SECCOMP_RET_KILL_PROCESS));
    f.push(ld(0));

    #[cfg(target_arch = "x86_64")]
    let denied = [
        155u32, 161, 165, 166, 248, 272, 308, 428, 429, 430, 431, 432, 433,
    ];
    #[cfg(target_arch = "aarch64")]
    let denied = [39u32, 40, 41, 49, 97, 268, 428, 429, 430, 431, 432, 433];
    for nr in denied {
        f.push(jeq(nr, 0, 1));
        f.push(ret(SECCOMP_RET_ERRNO | EPERM));
    }
    // clone3 的 flags 在用户态结构体里，BPF 读不到；返回 ENOSYS 让 libc
    // 回退到 clone，再在下面拒绝其中的 CLONE_NEWUSER。
    f.push(jeq(435, 0, 1));
    f.push(ret(SECCOMP_RET_ERRNO | ENOSYS));
    #[cfg(target_arch = "x86_64")]
    f.push(jeq(56, 0, 4));
    #[cfg(target_arch = "aarch64")]
    f.push(jeq(220, 0, 4));
    f.push(ld(16)); // args[0]，低 32 位是 clone flags
    f.push(jset(CLONE_NEWUSER, 0, 1));
    f.push(ret(SECCOMP_RET_ERRNO | EPERM));
    f.push(ret(SECCOMP_RET_ALLOW));
    f.push(ret(SECCOMP_RET_ALLOW));

    let prog = Program {
        len: f.len() as u16,
        filter: f.as_ptr(),
    };
    extern "C" {
        fn prctl(
            option: std::os::raw::c_int,
            arg2: c_ulong,
            arg3: c_ulong,
            arg4: c_ulong,
            arg5: c_ulong,
        ) -> std::os::raw::c_int;
    }
    const PR_SET_SECCOMP: i32 = 22;
    const PR_SET_NO_NEW_PRIVS: i32 = 38;
    const SECCOMP_MODE_FILTER: c_ulong = 2;
    if prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(wrap(io::Error::last_os_error(), "no_new_privs"));
    }
    if prctl(
        PR_SET_SECCOMP,
        SECCOMP_MODE_FILTER,
        &prog as *const Program as c_ulong,
        0,
        0,
    ) != 0
    {
        Err(wrap(io::Error::last_os_error(), "seccomp"))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn wrap(e: io::Error, what: &str) -> io::Error {
    io::Error::new(e.kind(), format!("读侧沙箱 {}: {}", what, e))
}

#[cfg(not(target_os = "linux"))]
pub fn prepare(
    _workspace: &Path,
    _cfg: &crate::sandbox::Config,
) -> std::io::Result<Box<dyn FnMut() -> std::io::Result<()> + Send + Sync + 'static>> {
    Ok(Box::new(|| Ok(())))
}
