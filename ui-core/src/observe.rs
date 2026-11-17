//! 观测式效果：shell / 插件 / MCP 这类"信任边界"通道，执行前后各扫一次工作区，
//! 把**实际变了什么**记进日志。
//!
//! 为什么需要它：效果集是"声明"的——shell 只声明 `proc.exec <命令>`，命令内部
//! 读了什么、写了什么，声明里没有。审计要回答"到底改了什么"，只能靠观测。
//!
//! 两条实现路径：
//!   - **inotify（Linux）**：目录事件驱动，每次调用只 drain 一下队列，不再扫描。
//!     这是默认路径；watch 数量超限或队列溢出时自动退回扫描。
//!   - **全量扫描**：读元数据（大小 + mtime），跳过重目录，文件数上限 2 万。
//!     其他平台或 watcher 不可用时用它。
//!
//! 代价控制：只读元数据、不读内容、不哈希；超限就标记 truncated，绝不拖死任务。

use crate::json::Json;
use std::collections::BTreeMap;
use std::path::Path;

/// 扫描时跳过的目录名（任何层级命中都跳过）。
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".ui",
    "experiments",
    "dist",
    "out",
    ".venv",
    "__pycache__",
];

/// 单次扫描最多看多少个文件。超过就截断并如实标记。
pub const MAX_FILES: usize = 20_000;

/// watcher 最多挂多少个目录。超过就退回全量扫描（不静默降级成"只监控一部分"）。
const MAX_WATCHES: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct FsState {
    /// 相对路径 → (字节数, 修改时间毫秒)
    pub entries: BTreeMap<String, (u64, u64)>,
    pub truncated: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub scanned: usize,
    pub truncated: bool,
}

impl Changes {
    pub fn total(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn to_json(&self) -> Json {
        let arr = |v: &Vec<String>| Json::Arr(v.iter().map(|s| Json::from(s.as_str())).collect());
        Json::obj(vec![
            ("added", arr(&self.added)),
            ("changed", arr(&self.changed)),
            ("removed", arr(&self.removed)),
            ("scanned", Json::from(self.scanned)),
            ("truncated", Json::Bool(self.truncated)),
        ])
    }

    /// 给人看的一行摘要，例如 `+2 ~1 -0`。
    pub fn summary(&self) -> String {
        format!(
            "+{} ~{} -{}",
            self.added.len(),
            self.changed.len(),
            self.removed.len()
        )
    }
}

pub fn scan(root: &Path) -> FsState {
    let mut st = FsState::default();
    let mut count = 0usize;
    walk(root, root, &mut st, &mut count);
    st
}

fn walk(root: &Path, dir: &Path, st: &mut FsState, count: &mut usize) {
    if st.truncated {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if st.truncated {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, st, count);
            continue;
        }
        if *count >= MAX_FILES {
            st.truncated = true;
            return;
        }
        *count += 1;
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        st.entries.insert(rel, (meta.len(), mtime));
    }
}

pub fn diff(before: &FsState, after: &FsState) -> Changes {
    let mut ch = Changes {
        scanned: after.entries.len(),
        truncated: before.truncated || after.truncated,
        ..Default::default()
    };
    for (path, (size, mtime)) in &after.entries {
        match before.entries.get(path) {
            None => ch.added.push(path.clone()),
            Some((bsize, bmtime)) => {
                if bsize != size || bmtime != mtime {
                    ch.changed.push(path.clone());
                }
            }
        }
    }
    for path in before.entries.keys() {
        if !after.entries.contains_key(path) {
            ch.removed.push(path.clone());
        }
    }
    ch.added.sort();
    ch.changed.sort();
    ch.removed.sort();
    // 截断时列表可能不完整，但"变了"这件事仍然成立——调用方会看到 truncated=true
    ch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ui-observe-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_add_change_remove_and_skips_heavy_dirs() {
        let d = tmp("basic");
        std::fs::write(d.join("a.txt"), "one").unwrap();
        std::fs::write(d.join("b.txt"), "two").unwrap();

        let before = scan(&d);
        assert_eq!(before.entries.len(), 2);

        std::fs::write(d.join("a.txt"), "one-longer").unwrap();
        std::fs::remove_file(d.join("b.txt")).unwrap();
        std::fs::write(d.join("c.txt"), "new").unwrap();
        // 这些目录必须被跳过，否则大仓库里扫一次要好几秒
        std::fs::create_dir_all(d.join("target").join("deep")).unwrap();
        std::fs::write(d.join("target").join("deep").join("x.o"), "junk").unwrap();

        let after = scan(&d);
        let ch = diff(&before, &after);
        assert_eq!(ch.added, vec!["c.txt".to_string()]);
        assert_eq!(ch.changed, vec!["a.txt".to_string()]);
        assert_eq!(ch.removed, vec!["b.txt".to_string()]);
        assert!(!ch.truncated);
        assert!(!after.entries.keys().any(|k| k.starts_with("target/")));
        let _ = std::fs::remove_dir_all(&d);
    }

#[test]
    #[cfg(target_os = "linux")]
    fn watcher_reports_created_changed_and_removed() {
        use std::io::Write;
        let d = tmp("watch");
        let mut w = match super::Watcher::new(&d) {
            Some(w) => w,
            None => return, // 某些内核/容器不支持 inotify，跳过而不是失败
        };
        // 清掉建立 watcher 过程中可能产生的事件
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = w.drain();

        std::fs::write(d.join("a.txt"), "one").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        let ch = w.drain().expect("不能溢出");
        assert!(ch.added.contains(&"a.txt".to_string()), "新增要记下: {:?}", ch.added);

        let mut f = std::fs::OpenOptions::new().append(true).open(d.join("a.txt")).unwrap();
        f.write_all(b"two").unwrap();
        drop(f);
        std::thread::sleep(std::time::Duration::from_millis(80));
        let ch = w.drain().expect("不能溢出");
        assert!(ch.changed.contains(&"a.txt".to_string()), "修改要记下: {:?}", ch.changed);

        std::fs::remove_file(d.join("a.txt")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        let ch = w.drain().expect("不能溢出");
        assert!(ch.removed.contains(&"a.txt".to_string()), "删除要记下: {:?}", ch.removed);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn watcher_follows_newly_created_directories() {
        let d = tmp("watch-dir");
        let mut w = match super::Watcher::new(&d) {
            Some(w) => w,
            None => return,
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = w.drain();

        // 建目录并立刻写文件：watch 挂上之前的事件可能丢，靠 collect_existing 补
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.txt"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let ch = w.drain().expect("不能溢出");
        assert!(
            ch.added.contains(&"sub/b.txt".to_string()),
            "新目录里的文件也要记下: {:?}",
            ch.added
        );

        // 之后这个子目录里的改动仍然能被跟踪
        std::fs::write(d.join("sub").join("c.txt"), "y").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let ch = w.drain().expect("不能溢出");
        assert!(
            ch.added.contains(&"sub/c.txt".to_string()),
            "新目录之后的写入也要记下: {:?}",
            ch.added
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_change_means_empty_diff() {
        let d = tmp("same");
        std::fs::write(d.join("a.txt"), "x").unwrap();
        let before = scan(&d);
        let after = scan(&d);
        assert!(diff(&before, &after).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ─────────────────────── inotify（Linux）───────────────────────


/// 非 Linux 平台的占位：没有 inotify，构造永远返回 None，
/// 调用方（agent）自然退回全量扫描。
///
/// 这个桩是必须的：`agent.rs` 无条件持有 `Option<Watcher>` 字段，
/// 少了它 Windows 目标直接编译失败（真实踩过——CI 的 Windows job 因此是假绿）。
#[cfg(not(target_os = "linux"))]
pub struct Watcher;

#[cfg(not(target_os = "linux"))]
impl Watcher {
    pub fn new(_root: &Path) -> Option<Watcher> {
        None
    }

    pub fn drain(&mut self) -> Option<Changes> {
        None
    }
}

#[cfg(target_os = "linux")]
pub use watcher::Watcher;

#[cfg(target_os = "linux")]
mod watcher {
    use super::{Changes, SKIP_DIRS};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::os::raw::{c_char, c_int, c_void};
    use std::path::{Path, PathBuf};

    const SYS_INOTIFY_INIT1: isize = 294;
    const SYS_INOTIFY_ADD_WATCH: isize = 254;

    const IN_NONBLOCK: c_int = 0o4000;
    const IN_CLOEXEC: c_int = 0o2_000_000;

    const IN_MODIFY: u32 = 0x0000_0002;
    const IN_ATTRIB: u32 = 0x0000_0004;
    const IN_CLOSE_WRITE: u32 = 0x0000_0008;
    const IN_MOVED_FROM: u32 = 0x0000_0040;
    const IN_MOVED_TO: u32 = 0x0000_0080;
    const IN_CREATE: u32 = 0x0000_0100;
    const IN_DELETE: u32 = 0x0000_0200;
    const IN_DELETE_SELF: u32 = 0x0000_0400;
    const IN_Q_OVERFLOW: u32 = 0x0000_4000;
    const IN_IGNORED: u32 = 0x0000_8000;
    const IN_ISDIR: u32 = 0x4000_0000;

    const WATCH_MASK: u32 = IN_MODIFY
        | IN_ATTRIB
        | IN_CLOSE_WRITE
        | IN_MOVED_FROM
        | IN_MOVED_TO
        | IN_CREATE
        | IN_DELETE
        | IN_DELETE_SELF;

    extern "C" {
        fn syscall(num: isize, ...) -> isize;
        fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn close(fd: c_int) -> c_int;
    }

    #[repr(C)]
    struct Event {
        wd: c_int,
        mask: u32,
        cookie: u32,
        len: u32,
    }

    pub struct Watcher {
        fd: c_int,
        root: PathBuf,
        /// wd → 目录路径
        dirs: BTreeMap<i32, PathBuf>,
        watches: usize,
    }

    impl Watcher {
        /// 在工作区上建立递归 watch。任何一个目录挂不上就整体放弃（返回 None），
        /// 让调用方退回全量扫描——不做"只监控了一部分"的假监控。
        pub fn new(root: &Path) -> Option<Watcher> {
            let fd = unsafe { syscall(SYS_INOTIFY_INIT1, IN_NONBLOCK | IN_CLOEXEC) } as c_int;
            if fd < 0 {
                return None;
            }
            let mut w = Watcher {
                fd,
                root: root.to_path_buf(),
                dirs: BTreeMap::new(),
                watches: 0,
            };
            if !w.add_tree(root) {
                return None; // Drop 会关 fd
            }
            Some(w)
        }

        fn add_dir(&mut self, path: &Path) -> bool {
            use std::os::unix::ffi::OsStrExt;
            let mut c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => return false,
            };
            let wd = unsafe {
                syscall(
                    SYS_INOTIFY_ADD_WATCH,
                    self.fd,
                    c.as_ptr() as *const c_char,
                    WATCH_MASK as c_int,
                )
            };
            let _ = &mut c;
            if wd < 0 {
                return false;
            }
            self.dirs.insert(wd as c_int, path.to_path_buf());
            self.watches += 1;
            true
        }

        /// 新目录刚挂上 watch 时，里面可能已经有文件了（建目录和写文件几乎同时）。
        /// 扫一遍补记，避免漏报。
        fn collect_existing(&self, dir: &Path, added: &mut BTreeSet<String>) {
            let rd = match std::fs::read_dir(dir) {
                Ok(r) => r,
                Err(_) => return,
            };
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    self.collect_existing(&path, added);
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(&self.root) {
                    added.insert(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }

        fn add_tree(&mut self, dir: &Path) -> bool {
            if self.watches >= super::MAX_WATCHES {
                return false;
            }
            if !self.add_dir(dir) {
                return false;
            }
            let rd = match std::fs::read_dir(dir) {
                Ok(r) => r,
                Err(_) => return true, // 读不了子目录不算失败（可能刚被删）
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) && !self.add_tree(&e.path()) {
                    return false;
                }
            }
            true
        }

        /// 取出自上次 drain 以来观测到的改动。
        /// 返回 None 表示"这次不可信"（队列溢出 / 读失败），调用方必须退回全量扫描。
        pub fn drain(&mut self) -> Option<Changes> {
            let mut ch = Changes::default();
            let mut added: BTreeSet<String> = BTreeSet::new();
            let mut changed: BTreeSet<String> = BTreeSet::new();
            let mut removed: BTreeSet<String> = BTreeSet::new();
            let mut buf = vec![0u8; 64 * 1024];

            loop {
                let n = unsafe { read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        break; // 读空了，走统一收尾（去重 + 赋值）
                    }
                    return None; // 真错误：不可信，调用方退回扫描
                }
                if n == 0 {
                    break;
                }
                let mut off = 0usize;
                let bytes = &buf[..n as usize];
                while off + std::mem::size_of::<Event>() <= bytes.len() {
                    let ev = unsafe { &*(bytes.as_ptr().add(off) as *const Event) };
                    let name_len = ev.len as usize;
                    let name_start = off + std::mem::size_of::<Event>();
                    let name_end = name_start + name_len;
                    if name_end > bytes.len() {
                        break;
                    }
                    off = name_end;

                    if ev.mask & IN_Q_OVERFLOW != 0 {
                        return None; // 事件丢了：不可信，退回扫描
                    }
                    if ev.mask & IN_IGNORED != 0 {
                        self.dirs.remove(&ev.wd);
                        continue;
                    }
                    let dir = match self.dirs.get(&ev.wd) {
                        Some(d) => d.clone(),
                        None => continue,
                    };
                    let name = String::from_utf8_lossy(&bytes[name_start..name_end])
                        .trim_end_matches('\0')
                        .to_string();
                    if name.is_empty() {
                        // 目录自身的事件（DELETE_SELF 等）
                        if ev.mask & IN_DELETE_SELF != 0 {
                            if let Ok(rel) = dir.strip_prefix(&self.root) {
                                let rel = rel.to_string_lossy().replace('\\', "/");
                                if !rel.is_empty() {
                                    removed.insert(rel);
                                }
                            }
                        }
                        continue;
                    }
                    let path = dir.join(&name);
                    let rel = match path.strip_prefix(&self.root) {
                        Ok(r) => r.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };
                    if rel.split('/').any(|c| SKIP_DIRS.contains(&c)) {
                        continue;
                    }

                    if ev.mask & (IN_CREATE | IN_MOVED_TO) != 0 {
                        if ev.mask & IN_ISDIR != 0 {
                            // 新目录：挂上子树，好继续观测里面的写入。
                            // 但「建目录」和「往里写文件」可能几乎同时发生，
                            // watch 挂上之前的事件会丢——所以再把这个子树扫一遍，
                            // 把已经存在的文件补记为 added。这是竞态，不是优化。
                            let _ = self.add_tree(&path);
                            self.collect_existing(&path, &mut added);
                        } else {
                            added.insert(rel.clone());
                        }
                    }
                    if ev.mask & (IN_MODIFY | IN_CLOSE_WRITE | IN_ATTRIB) != 0
                        && ev.mask & IN_ISDIR == 0
                    {
                        changed.insert(rel.clone());
                    }
                    if ev.mask & (IN_DELETE | IN_MOVED_FROM) != 0 && ev.mask & IN_ISDIR == 0 {
                        removed.insert(rel);
                    }
                }
            }

            // 去重：刚创建又写入的文件算"新增"，不再重复算"修改"
            let added_set: std::collections::BTreeSet<String> = added.iter().cloned().collect();
            changed.retain(|p| !added_set.contains(p));
            removed.retain(|p| !added_set.contains(p));
            ch.scanned = added.len() + changed.len() + removed.len();

            ch.added = added.into_iter().collect();
            ch.changed = changed.into_iter().collect();
            ch.removed = removed.into_iter().collect();
            Some(ch)
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            unsafe {
                close(self.fd);
            }
        }
    }
}

