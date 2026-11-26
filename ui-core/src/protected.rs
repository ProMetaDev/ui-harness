//! 工具永远不能读、写、搜索的目录。
//!
//! 这里刻意不复用 `observe::SKIP_DIRS`：那张表的语义是「扫描工作区做变更
//! 观测时不想看的大目录」（`node_modules`、`target`……），按噪音/性能调整；
//! 这张表是安全边界，按「能否交给模型」调整。两者变化的方向不同，合并会
//! 让其中一张表为了另一张表的目的被改宽。
//!
//! 单一来源的消费方：`fs_search` 的 walk、`grep`（系统 grep 的
//! `--exclude-dir` 与无 grep 时的 `literal_search`）、`tree`，
//! `Policy::defaults` 里针对这些目录的 Deny 规则，以及 `ui serve` 的
//! `/api/file` / `/api/tree`。**判据只有一份**：浏览器里能直达的目录，
//! 不该比模型能读到的更宽。
//!
//! 默认清单之外还能按项目追加（`--private` / `UI_PRIVATE`）：清单本身
//! 是进程级只读的，只能由人在命令行上给——模型的工具写不到这里，
//! 否则"把 secrets 从清单里删掉"就成了一次自主升级。

use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 受保护目录名。大小写不敏感，因为 Windows 文件系统不区分大小写：
/// `Secrets` 与 `secrets` 是同一个目录，按精确字节比较会漏。
///
/// `.ui` 是内核自己的状态目录（日志、会话、配置、被截断的工具输出）。
/// 它们不是用户源码，但里面装着完整的提示词和工具结果，让模型「读一眼
/// 日志」等于把控制面变成数据面。
/// `.git` 单独列出：仓库对象里可能有历史密钥、邮箱和钩子，而
/// `observe::SKIP_DIRS` 排除它只是性能考虑，不能当安全边界。
///
/// `target` / `node_modules` 仍归观测层管：它们是噪音，不是隐私。构建
/// 产物里确实可能嵌了密钥，但那种项目要按项目加策略规则，把整棵依赖树
/// 拉黑会让模型读不到构建配置，收益不抵代价。
pub const PROTECTED_DIRS: &[&str] = &[".ui", ".git", "secrets"];

/// 敏感名里的固定文件名（小写比较）。
const SENSITIVE_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// 敏感扩展名（小写比较）。
const SENSITIVE_EXTENSIONS: &[&str] = &["key", "pem", "p12", "pfx"];

/// 文件级敏感名：目录清单管不到 `.env` / 私钥这类单文件。
///
/// checkpoint 会把文件内容复制进 blob，index 会把路径/符号写进 manifest；
/// 两者都必须先过这一条。这里故意保守：宁可少索引一个配置文件，也不把
/// 凭据复制出第二份。
pub fn is_sensitive_file_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n == ".env" || n.starts_with(".env.") {
        return true;
    }
    if SENSITIVE_NAMES.contains(&n.as_str()) {
        return true;
    }
    match n.rsplit_once('.') {
        Some((_, ext)) => SENSITIVE_EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// 敏感名的 glob 形态，给**只能按名字过滤**的调用点用（系统 `grep --exclude`）。
///
/// 权威判据仍是 `is_sensitive_file_name` / `is_sensitive_read_path`：glob 只做
/// 读取前的粗筛，大小写变体与 `notes.txt -> .env` 这种软链别名它判不了，
/// 结果行还会按真实路径过滤一次。两边不一致时结果过滤是权威。
pub fn sensitive_file_globs() -> Vec<String> {
    let mut out = vec![".env".to_string(), ".env.*".to_string()];
    out.extend(SENSITIVE_NAMES.iter().map(|n| n.to_string()));
    out.extend(SENSITIVE_EXTENSIONS.iter().map(|e| format!("*.{}", e)));
    out
}

/// 受保护目录里唯一允许 `fs.read` 的路径前缀（相对 workspace，`/` 分隔）。
///
/// 长工具输出会落到 `.ui/tool-output/`，上下文里只留一小片，模型必须能把
/// 全文读回来，否则截断就等于丢数据。
///
/// **只此一个**。按隐私清单扣留的原文落 `.ui/tool-private/`，那里**故意不开
/// 读例外**：日志里那句"全文在 …"本身就是给模型看的提示词，例外一覆盖它，
/// 一次 `fs_read` 就能把正文完整捞回来，落账前的第二层隐私塌成一层。
/// 全文读回来（见 `agent::retain_tool_output`）。日志、会话、配置不在例外
/// 里，仍被 `PROTECTED_DIRS` 挡住。
///
/// 前缀只是**一半**判据：`is_allowed_read` 还要解析真实路径，要求它严格落在
/// `<ws>/.ui/tool-output/` 里、且是普通文件。所以：
///
/// - `.ui/tool-output/link -> secrets/astra.json` 借不到这条例外（真实路径
///   落在受保护目录里）；
/// - 指向工作区外、或者 target 不存在的符号链接也借不到（后者 fail closed）；
/// - `.env*`、私钥/证书这类敏感文件名不在例外里，名字命中就拒绝。
pub const PROTECTED_READ_EXCEPTIONS: &[&str] = &[".ui/tool-output/"];

// ───────────────────── 按项目追加的隐私路径 ─────────────────────

/// 一条追加的隐私声明。默认清单按**目录名**匹配（`.ui`/`secrets` 在
/// 任何层级都算），追加的可以是三种形态，各自有不同的匹配方式：
///
/// - 相对名/相对路径（`vendor/keys`、`id_rsa`）：在工作区内的任意深度
///   按**连续段**匹配，与默认清单同一条规则；
/// - 绝对路径（`/etc/foo`、`X:\bar`）：按真实前缀匹配，工作区外的也收——
///   工具本来就越不出工作区，但它给 `shell` 命令串的检测用；
/// - `~/…`：按 `HOME`（Windows 上 `USERPROFILE`）展开后当绝对路径处理。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    /// 人在命令行上写的那一份，用来打印和回显
    raw: String,
    /// 连续段匹配用（相对形态）。绝对形态为 `None`。
    segments: Option<Vec<String>>,
    /// 前缀匹配用（绝对形态）。相对形态为 `None`。
    absolute: Option<PathBuf>,
    /// `~/xxx` 声明里 `HOME` 之后那一段（`".aws/credentials"`）。
    /// 命令串里没人会写展开后的绝对路径，他们写的是 `~/.aws/…` 或
    /// `$HOME/.aws/…`——所以展开之外还要留住这个写法。
    tilde_tail: Option<String>,
}

fn table() -> &'static Mutex<Vec<Entry>> {
    static EXTRA: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    EXTRA.get_or_init(|| Mutex::new(Vec::new()))
}

fn expand_home(raw: &str) -> Option<(PathBuf, String)> {
    let rest = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"))?;
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())?;
    let mut p = PathBuf::from(home);
    let mut tail: Vec<String> = Vec::new();
    for seg in rest.split(|c| c == '/' || c == '\\') {
        if !seg.is_empty() && seg != "." {
            p.push(seg);
            tail.push(seg.to_lowercase());
        }
    }
    if tail.is_empty() {
        return None;
    }
    Some((p, tail.join("/")))
}

fn is_absolute(raw: &str) -> bool {
    raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.as_bytes().first().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
            && raw.as_bytes().get(1) == Some(&b':')
}

/// 登记一条隐私路径。空串与含 `..` 的声明直接拒绝：前者多半是引号写错，
/// 后者会把判定变成"往上跳"，边界不该由一个相对片段决定。
pub fn add_private(raw: &str) -> Result<(), String> {
    let entry = parse_private(raw)?;
    let mut guard = table().lock().unwrap();
    if !guard.iter().any(|e| e.raw == entry.raw) {
        guard.push(entry);
    }
    Ok(())
}

/// 用给定的一组声明**替换**追加清单。
///
/// 存在的理由有两个：嵌入内核的宿主想按自己的配置整体重设一次，以及跨
/// crate 的测试需要一个干净的起点。替换不等于放宽——这张表仍然只有进程外
/// 的调用方能写，模型的工具表里没有这条路，所以「把 secrets 从清单里删掉」
/// 依然不是一次它能自主发起的升级。
///
/// 先全部解析再整体换：中途有一条写错（空串、含 `..`）时清单保持原样，
/// 不会出现「前半段生效、后半段报错」这种比声明本身更糟的半成品边界。
pub fn set_privates(entries: &[&str]) -> Result<(), String> {
    let mut parsed = Vec::with_capacity(entries.len());
    for raw in entries {
        parsed.push(parse_private(raw)?);
    }
    let mut guard = table().lock().unwrap();
    guard.clear();
    for entry in parsed {
        if !guard.iter().any(|e| e.raw == entry.raw) {
            guard.push(entry);
        }
    }
    Ok(())
}

fn parse_private(raw: &str) -> Result<Entry, String> {
    let raw = raw.trim().trim_matches('"').trim();
    if raw.is_empty() {
        return Err("--private 的值是空的".to_string());
    }
    let lowered = raw.to_lowercase().replace('\\', "/");
    if lowered.split('/').any(|s| s == "..") {
        return Err(format!("--private 不接受 ..：{}", raw));
    }
    Ok(if let Some((abs, tail)) = expand_home(raw) {
        Entry {
            raw: raw.to_string(),
            segments: None,
            absolute: Some(abs),
            tilde_tail: Some(tail),
        }
    } else if is_absolute(raw) {
        Entry {
            raw: raw.to_string(),
            segments: None,
            absolute: Some(PathBuf::from(raw)),
            tilde_tail: None,
        }
    } else {
        let segments: Vec<String> = lowered
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| s.to_string())
            .collect();
        if segments.is_empty() {
            return Err(format!("--private 的值没有可用的段：{}", raw));
        }
        Entry {
            raw: raw.to_string(),
            segments: Some(segments),
            absolute: None,
            tilde_tail: None,
        }
    })
}

/// 清空追加清单。只给测试用：进程内的清单本应由命令行决定一次。
#[cfg(test)]
pub fn clear_private() {
    let _ = set_privates(&[]);
}

/// 测试用串行锁。
///
/// 追加清单是**进程级**共享可变状态，而 `cargo test` 默认多线程跑用例：
/// 没有这把锁，一个用例的 `clear_private()` 会把另一个用例刚登记的声明
/// 抽掉，断言于是变成掷硬币。每个碰清单的用例开头 `let _g = guard()`。
/// 取锁失败（前一个用例在里面 panic）时照旧接管：清单已经被
/// `clear_private()` 的下一个用例重置，没必要让一个失败拖成一片。
///
/// 给整个 crate 的测试用——工具层与策略层也要登记声明，它们碰的是同一张表。
#[cfg(test)]
pub fn private_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 有没有人追加过声明。
///
/// 扫描类入口（`walk`、`tree`、`literal_search`）先问这一句：默认清单按
/// **单个目录名**就能判掉，只有追加清单才需要拿完整路径再判一次。多段
/// 声明不常见，不该让每个目录项都为它付一次路径判定的钱。
pub fn has_declarations() -> bool {
    !table().lock().unwrap().is_empty()
}

/// 某条声明是不是人追加的（而不是默认清单里的）。
///
/// 只给展示用：`ui doctor` 要把「默认挡住」和「你让它挡住」分开印，
/// 否则加了 `--private` 却看不出它生效没有。判定仍只有一份。
pub fn is_declared(raw: &str) -> bool {
    table()
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.raw == raw)
}

/// 生效清单的人话形式（默认 + 追加），`ui doctor` 与错误信息都印它。
pub fn private_entries() -> Vec<String> {
    let mut out: Vec<String> = PROTECTED_DIRS.iter().map(|s| s.to_string()).collect();
    out.extend(
        table()
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.raw.clone())
            .collect::<Vec<_>>(),
    );
    out
}

/// 单段的名字型声明（`vendor`、`id_rsa`）：给"按段跳过"的调用点用。
fn private_segment_lists() -> Vec<Vec<String>> {
    table()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| e.segments.clone())
        .collect()
}

fn absolute_prefixes() -> Vec<PathBuf> {
    table()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| e.absolute.clone())
        .collect()
}

/// 追加清单里**位于工作区内**的路径，按相对 workspace、`/` 分隔给出。
///
/// 这是给策略层生成 Deny 规则用的：策略按绝对模式匹配，而"工作区外"的
/// 声明本来就落在 `<ws>/** → deny`（workspace 之外的读取）里，不需要重复
/// 写规则；只有工作区内的追加项需要和默认清单走同一套形状。
pub fn private_subpaths(workspace: &Path) -> Vec<String> {
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let guard = table().lock().unwrap();
    let mut out = Vec::new();
    for e in guard.iter() {
        if let Some(segs) = &e.segments {
            out.push(segs.join("/"));
            continue;
        }
        if let Some(abs) = &e.absolute {
            let real = abs.canonicalize().unwrap_or_else(|_| abs.clone());
            if let Ok(rel) = real.strip_prefix(&ws) {
                let s = rel.to_string_lossy().replace('\\', "/");
                if !s.is_empty() && s != "." {
                    out.push(s);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 某个路径段名字是否命中隐私清单（默认清单 + 单段声明）。
pub fn is_private_dir_name(name: &str) -> bool {
    if is_protected_dir(name) {
        return true;
    }
    let lower = name.to_lowercase();
    private_segment_lists()
        .iter()
        .any(|segs| segs.len() == 1 && segs[0] == lower)
}

/// 能按**单个目录名**排除的那一份清单：默认三项 + 单段声明。
///
/// 给 `grep --exclude-dir=` 这种"只能给名字"的调用点用。多段
/// （`vendor/keys`）与绝对声明要靠路径判定，不能塞进 `--exclude-dir`。
pub fn private_dir_names() -> Vec<String> {
    let mut out: Vec<String> = PROTECTED_DIRS.iter().map(|s| s.to_string()).collect();
    out.extend(
        private_segment_lists()
            .into_iter()
            .filter(|segs| segs.len() == 1)
            .map(|segs| segs[0].clone()),
    );
    out
}

/// `path`（相对 workspace 或直接给绝对路径）是否落在隐私清单里。
pub fn is_private_path(workspace: &Path, path: &Path) -> bool {
    if is_protected_path(workspace, path) {
        return true;
    }
    let rel = path.strip_prefix(workspace).unwrap_or(path);
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => name.to_str().map(|s| s.to_lowercase()),
            _ => None,
        })
        .collect();
    if private_segment_lists().iter().any(|segs| {
        !segs.is_empty()
            && segs.len() <= comps.len()
            && comps.windows(segs.len()).any(|w| w == segs.as_slice())
    }) {
        return true;
    }
    // 绝对声明按前缀判：`--private ~/.aws` 要能挡住 `/home/me/.aws/credentials`
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    absolute_prefixes()
        .into_iter()
        .any(|pre| abs.starts_with(pre))
}

/// 解析“最近的存在祖先”，再拼回不存在的尾部。
///
/// 写新文件时目标本身不存在，`canonicalize(path)` 必然失败；但父目录可能
/// 是一个指向 `.git`/`.ui`/`secrets` 的符号链接。只返回 `false` 会让
/// `blocked_write` 漏掉这类目标。
fn canonicalize_with_missing(path: &Path) -> Option<PathBuf> {
    if let Ok(real) = path.canonicalize() {
        return Some(real);
    }
    let mut ancestor = path.parent();
    while let Some(parent) = ancestor {
        if let Ok(real) = parent.canonicalize() {
            let mut out = real;
            if let Ok(rest) = path.strip_prefix(parent) {
                for comp in rest.components() {
                    out.push(comp.as_os_str());
                }
            }
            return Some(out);
        }
        ancestor = parent.parent();
    }
    None
}

/// 解析真实路径后再判一次，挡住符号链接别名。
pub fn is_private_real_path(workspace: &Path, path: &Path) -> bool {
    let real = match canonicalize_with_missing(path) {
        Some(p) => p,
        None => return false,
    };
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    is_private_path(&ws, &real)
}

/// 把任意文本切成「可能是路径的一段」。
///
/// 只按 `/` 切是不够的：命令串里 `secrets` 前面通常还挂着 `cat `、引号、
/// `$(`。宁可切得碎一点——漏判的代价是**当成不涉及隐私**，误判的代价只是
/// 多做一次脱敏。
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| {
            c == '/'
                || c == '\\'
                || c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '=' | ';' | ':' | '|' | '&' | '(' | ')' | '<' | '>' | ',' | '{'
                        | '}' | '[' | ']' | '$' | '*' | '?' | '#'
                )
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 文本里是否出现了隐私清单上的路径。
///
/// 这是给**观测不到的通道**用的：`shell` 的读取无法拦（Landlock 只限制写，
/// 见 `sandbox.rs`），但命令串里写了 `secrets/api.txt`、`~/.aws/credentials`，
/// 我们就知道"这一次可能读到了私密内容"——足够把它记成可审计的事实，
/// 并据此决定输出要不要出链（见 `agent::journal_tool_output`）。
///
/// 返回命中的那一条声明，供日志回显。
pub fn mentioned_in(text: &str) -> Option<String> {
    let hay: Vec<String> = tokens(text);
    for dir in PROTECTED_DIRS {
        if hay.iter().any(|seg| seg == dir) {
            return Some(dir.to_string());
        }
    }
    let guard = table().lock().unwrap();
    for e in guard.iter() {
        if let Some(segs) = &e.segments {
            if !segs.is_empty() && hay.windows(segs.len()).any(|w| w == segs.as_slice()) {
                return Some(e.raw.clone());
            }
            continue;
        }
        if let Some(abs) = &e.absolute {
            let flat = abs.to_string_lossy().to_lowercase().replace('\\', "/");
            if flat.is_empty() {
                continue;
            }
            // 整条展开后的路径出现在命令里，算命中
            if text.to_lowercase().replace('\\', "/").contains(&flat) {
                return Some(e.raw.clone());
            }
        }
        // `~/.aws/credentials` 的写法：人不会在命令里写 /home/me/.aws
        if let Some(tail) = &e.tilde_tail {
            let segs: Vec<&str> = tail.split('/').collect();
            if !segs.is_empty() && hay.windows(segs.len()).any(|w| w.join("/") == *tail) {
                return Some(e.raw.clone());
            }
        }
    }
    None
}

/// 同 `mentioned_in`，但先把**工作区自身的路径**从文本里挖掉。
///
/// 绝对路径形态的效果目标带着工作区前缀。工作区恰好住在 `~/secrets/` 下面的
/// 时候，那个前缀会让每一次读写都命中清单——整份日志退化成指针，等于没有日志。
/// 清单管的是工作区里面的东西，不是这个目录碰叫什么名字（与
/// `is_protected_path` 剥前缀的理由是同一条）。
pub fn mentioned_in_under(workspace: &Path, text: &str) -> Option<String> {
    let mut probe = text.to_string();
    let raw = workspace.to_string_lossy().to_string();
    let unix = raw.replace('\\', "/");
    for form in [raw.as_str(), unix.as_str()] {
        // 长度 >1 才挖：工作区是 "/" 时挖空的是整条目标。
        if form.chars().count() > 1 {
            probe = probe.replace(form, "");
        }
    }
    mentioned_in(&probe)
}

/// 目录名是否受保护。
pub fn is_protected_dir(name: &str) -> bool {
    PROTECTED_DIRS
        .iter()
        .any(|dir| name.eq_ignore_ascii_case(dir))
}

/// `path` 中是否有任何一段（相对 workspace）是受保护目录。
///
/// 先按路径分量剥掉 workspace，避免 workspace 自己的路径里恰好含有
/// `secrets` 时把整个工作区误判成受保护。
pub fn is_protected_path(workspace: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(workspace).unwrap_or(path);
    rel.components().any(|c| match c {
        Component::Normal(name) => name.to_str().map(is_protected_dir).unwrap_or(false),
        _ => false,
    })
}

/// 解析真实路径后再判断一次，挡住「工作区里的符号链接别名指向 secrets」。
///
/// 目标不存在（例如写新文件）时返回 `false`；写目标由策略层和
/// `agent::escapes_workspace` 负责。
pub fn is_protected_real_path(workspace: &Path, path: &Path) -> bool {
    let real = match canonicalize_with_missing(path) {
        Some(p) => p,
        None => return false,
    };
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    is_protected_path(&ws, &real)
}

/// 解析一次**读**操作的真实目标；解析不出真实位置就返回 `None`（拒绝）。
///
/// 与 `canonicalize_with_missing` 的差别只有一条：**悬空符号链接**（链接在、
/// target 不在）返回 `None`，而不是按「最近存在的祖先」猜一个位置。猜出来的
/// 位置永远留在原地，等于把一个指向别处的链接当成普通文件放行——那正是评审
/// N-P0-1 指出的洞。写路径仍用后者：新文件本来就还不存在，不能因为"不存在"
/// 就拒绝建文件。
///
/// 目标本身不存在、但祖先都能解析（例如被保留策略清掉的截断缓存）时，返回
/// 「真实祖先 + 剩余字面段」：这种路径要能走到 `fs_read` 的"已按保留策略
/// 清理"提示，而不是被当成越界。
fn canonicalize_read_target(path: &Path) -> Option<PathBuf> {
    if let Ok(real) = path.canonicalize() {
        return Some(real);
    }
    // 目标自己存在（或本身就是一条链接）却解析不出来：真实位置未知，fail closed。
    if std::fs::symlink_metadata(path).is_ok() {
        return None;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        tail.push(cur.file_name()?.to_os_string());
        let parent = cur.parent()?;
        if let Ok(real) = parent.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        // 祖先存在却也解析不出来：中间有一段是悬空链接，同样不猜。
        if std::fs::symlink_metadata(parent).is_ok() {
            return None;
        }
        cur = parent;
    }
}

/// 受保护目录里唯一能读的东西：`<ws>/.ui/tool-output/` 里的普通文件。
///
/// 两道判据缺一不可：
///
/// 1. **字面**：调用方给的路径相对 workspace 必须写在 `.ui/tool-output/` 下；
/// 2. **真实**：解析之后的路径必须严格落在 `<ws_real>/.ui/tool-output/` 里，
///    且剩余段上没有 `secrets` / `.git` / 其它受保护段与声明，末段不是敏感
///    文件名。
///
/// 只判字面就是评审 N-P0-1 那条洞：`.ui/tool-output/link` 的**名字**在例外
/// 目录里，符号链接的 target 却可以是 `secrets/astra.json`，甚至工作区外。
/// 只判真实也不行：例外只该覆盖这一个目录，不能让"解析后落在某处"成为新的
/// 放行理由。
///
/// 只放宽读，不放宽写：模型往 `.ui/tool-output/` 写文件就是往内核的截断缓存
/// 里塞伪造内容，下一轮它自己会被骗。写一律按受保护处理（`blocked_write`）。
///
/// 目标不存在时仍要能回答：被保留策略清掉的截断缓存要给出"已清理"而不是
/// "不存在"的提示（`tools::missing_file_note`）。所以"不存在"本身不拒绝——
/// 只有**解析不出真实位置**（悬空链接、解析不了的祖先）才拒绝，fail closed。
pub fn is_allowed_read(workspace: &Path, path: &Path) -> bool {
    // ① 字面：相对 workspace 的文本必须就在例外前缀下面。
    let rel = path.strip_prefix(workspace).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    let text = text.trim_start_matches("./");
    if !PROTECTED_READ_EXCEPTIONS
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return false;
    }
    let ws_real = match workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    // 相对路径按 workspace 解释：字面判据已经把前者当成 workspace 内的位置。
    let target = if path.is_relative() {
        ws_real.join(path)
    } else {
        path.to_path_buf()
    };
    // ② 真实：解析不出来就不放行（悬空链接在这里被拒）。
    let real = match canonicalize_read_target(&target) {
        Some(p) => p,
        None => return false,
    };
    let base = ws_real.join(".ui").join("tool-output");
    let Ok(rest) = real.strip_prefix(&base) else {
        return false;
    };
    // 目录本身（剩余段为空）不算；剩余段必须都是普通名字（`..` 已被解析掉）。
    if rest.as_os_str().is_empty()
        || !rest
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return false;
    }
    // 默认清单、单段/多段声明、绝对声明：按剩余段再判一次。
    // （`secrets/x`、`--private vendor/keys`、`--private /etc/ssh` 都在这里。）
    if is_private_path(&base, &real) {
        return false;
    }
    if real
        .file_name()
        .and_then(|n| n.to_str())
        .map(is_sensitive_file_name)
        .unwrap_or(false)
    {
        return false;
    }
    // 例外只对**普通文件**生效：目录、设备、FIFO 都不是"截断缓存的全文"。
    !real.exists() || real.is_file()
}

/// 读入口的敏感文件名判定（`.env*`、私钥、证书……）。
///
/// 判两处：调用方给的名字，和解析后的真实名字。后者挡 `notes.txt -> .env`
/// 这种别名——只看字面名字，一次软链就绕过去了。清单与 checkpoint / index
/// 共用 `is_sensitive_file_name` 那一份，不会各拿一张表。
///
/// 解析不出真实路径时**不额外拒绝**：名字这一关已经过了，剩下的边界归
/// `is_allowed_read` 与隐私清单；而敏感名本身永远没有读取例外。
pub fn is_sensitive_read_path(path: &Path) -> bool {
    has_sensitive_name(path)
        || canonicalize_with_missing(path)
            .map(|real| has_sensitive_name(&real))
            .unwrap_or(false)
}

/// 只看**调用方给的名字**的那一半判定（软链别名要靠真实路径那一半）。
fn has_sensitive_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(is_sensitive_file_name)
        .unwrap_or(false)
}

/// 读入口与写入口共用的「这个条目不可见」判定：敏感单文件（含软链别名）
/// 或隐私清单（含真实路径）都算。
///
/// 写入口没有读例外，所以这里**不**引入 `is_allowed_read`：`.ui/tool-output/`
/// 能读回来，但不能借同一条路径被写。`grep`/`fs_search`/`tree` 的条目过滤
/// 与 `fs_write`/`fs_edit`/`patch`/`fs_mv` 的拒绝走的是这一条，敏感名不再只
/// 挂在 `fs_read` 上。
///
/// 名字/路径的字面判定先短路，普通条目只多付一次 `canonicalize`（软链别名
/// 必须解析真实路径才判得出来）；扫描大目录时不能每个条目解析两遍。
pub fn is_sensitive_or_private(workspace: &Path, path: &Path) -> bool {
    if has_sensitive_name(path) || is_private_path(workspace, path) {
        return true;
    }
    // 相对路径按 workspace 解释：`canonicalize` 自己会用进程 CWD，不能直接喂。
    let target = if path.is_relative() {
        workspace.join(path)
    } else {
        path.to_path_buf()
    };
    match canonicalize_with_missing(&target) {
        Some(real) => has_sensitive_name(&real) || is_private_path(workspace, &real),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_names_are_case_insensitive() {
        assert!(is_protected_dir("secrets"));
        assert!(is_protected_dir("Secrets"));
        assert!(is_protected_dir("SECRETS"));
        assert!(is_protected_dir(".ui"));
        assert!(is_protected_dir(".UI"));
        assert!(is_protected_dir(".git"));
        assert!(!is_protected_dir("secret"));
        assert!(!is_protected_dir("src"));
        // 构建产物是噪音不是隐私，归 observe::SKIP_DIRS 管
        assert!(!is_protected_dir("target"));
        assert!(!is_protected_dir("node_modules"));
    }

    #[test]
    fn tool_state_and_git_objects_are_protected() {
        let ws = Path::new("/ws");
        for path in [
            ".ui/journal.jsonl",
            ".ui/sessions/abc.jsonl",
            ".ui/config.toml",
            ".git/config",
            ".git/hooks/pre-commit",
        ] {
            assert!(
                is_protected_path(ws, &ws.join(path)),
                "{} 必须落在安全边界内",
                path
            );
        }
        assert!(!is_protected_path(ws, &ws.join("src/main.rs")));
    }

    /// 临时工作区。`is_allowed_read` 现在要解析真实路径，`/ws` 这种假路径在
    /// Windows 上连盘符都对不上，边界用例必须落在真目录上。
    fn temp_ws(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ui-prot-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn only_truncated_tool_output_may_be_read_back() {
        let ws = temp_ws("output");
        std::fs::create_dir_all(ws.join(".ui/tool-output")).unwrap();
        std::fs::write(ws.join(".ui/tool-output/0001-shell.txt"), "ok\n").unwrap();
        std::fs::write(ws.join(".ui/journal.jsonl"), "{}\n").unwrap();
        assert!(is_allowed_read(&ws, &ws.join(".ui/tool-output/0001-shell.txt")));
        assert!(is_allowed_read(
            &ws,
            Path::new("./.ui/tool-output/0001-shell.txt")
        ));
        // 例外只到这一个目录：兄弟目录、目录本身都不放行
        assert!(!is_allowed_read(&ws, &ws.join(".ui/tool-output")));
        assert!(!is_allowed_read(&ws, &ws.join(".ui/tool-output-x/a.txt")));
        assert!(!is_allowed_read(&ws, &ws.join(".ui/journal.jsonl")));
        assert!(!is_allowed_read(&ws, &ws.join(".git/config")));
        // 嵌套的 tool-output 不算：例外按 workspace 相对前缀写死一层
        assert!(!is_allowed_read(&ws, &ws.join("pkg/.ui/tool-output/a.txt")));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 评审 N-P0-1：`.ui/tool-output/` 下一个软链**名字**在例外目录里，
    /// target 却可以是 `secrets/…`——只判字面就是一条绕过真实路径保护的路。
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_tool_output_cannot_borrow_the_exception() {
        let ws = temp_ws("link-secret");
        std::fs::create_dir_all(ws.join(".ui/tool-output")).unwrap();
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets/astra.json"), "{\"api_key\":\"x\"}\n").unwrap();
        std::os::unix::fs::symlink(ws.join("secrets/astra.json"), ws.join(".ui/tool-output/link"))
            .unwrap();
        assert!(
            !is_allowed_read(&ws, &ws.join(".ui/tool-output/link")),
            "软链把 secrets 借成了截断缓存"
        );
        // 同一目录里的普通文件照旧能读：别为了堵洞把截断缓存废掉
        std::fs::write(ws.join(".ui/tool-output/ok.txt"), "ok\n").unwrap();
        assert!(is_allowed_read(&ws, &ws.join(".ui/tool-output/ok.txt")));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 指向工作区外的软链同样不能借：真实路径不在 `<ws>/.ui/tool-output/` 里。
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_tool_output_cannot_escape_the_workspace() {
        let ws = temp_ws("link-outside");
        let outside = std::env::temp_dir().join(format!("ui-prot-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("passwd-demo"), "root:x:0:0\n").unwrap();
        std::fs::create_dir_all(ws.join(".ui/tool-output")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("passwd-demo"),
            ws.join(".ui/tool-output/link"),
        )
        .unwrap();
        assert!(!is_allowed_read(&ws, &ws.join(".ui/tool-output/link")));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// target 不存在的链接按拒绝处理：猜位置等于把指向别处的链接当成普通文件。
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_refused_not_guessed() {
        let ws = temp_ws("link-dangling");
        std::fs::create_dir_all(ws.join(".ui/tool-output")).unwrap();
        std::os::unix::fs::symlink(
            ws.join(".ui/tool-output/gone.txt"),
            ws.join(".ui/tool-output/link"),
        )
        .unwrap();
        assert!(!is_allowed_read(&ws, &ws.join(".ui/tool-output/link")));
        // 悬空链接当目录用：中间那一段解析不出来，也不能放行
        std::os::unix::fs::symlink(
            ws.join(".ui/tool-output/nope-dir"),
            ws.join(".ui/tool-output/gone"),
        )
        .unwrap();
        assert!(!is_allowed_read(&ws, &ws.join(".ui/tool-output/gone/x.txt")));
        // 只是"还没写出来"的普通文件不算悬空链接：截断缓存的已清理提示要能走到
        assert!(is_allowed_read(&ws, &ws.join(".ui/tool-output/new-file.txt")));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 敏感文件名没有例外档：即使它躺在 tool-output 里也不读。
    #[test]
    fn sensitive_names_do_not_get_the_read_exception() {
        let ws = temp_ws("sensitive-exception");
        std::fs::create_dir_all(ws.join(".ui/tool-output")).unwrap();
        for name in [".env", ".env.local", "id_rsa", "server.key", "deploy.pem"] {
            std::fs::write(ws.join(".ui/tool-output").join(name), "SECRET\n").unwrap();
            assert!(
                !is_allowed_read(&ws, &ws.join(".ui/tool-output").join(name)),
                "{} 不该拿到读取例外",
                name
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 读入口的敏感名判定看**真实名字**：`notes.txt -> .env` 的名字看不出来。
    #[test]
    fn a_sensitive_name_is_seen_through_an_alias() {
        let ws = temp_ws("sensitive-alias");
        std::fs::write(ws.join(".env"), "TOKEN=secret\n").unwrap();
        assert!(is_sensitive_read_path(&ws.join(".env")));
        assert!(is_sensitive_read_path(&ws.join("id_rsa")));
        assert!(is_sensitive_read_path(&ws.join("server.key")));
        assert!(is_sensitive_read_path(&ws.join("certs/deploy.pem")));
        assert!(!is_sensitive_read_path(&ws.join("notes.txt")));
        assert!(!is_sensitive_read_path(&ws.join("src/main.rs")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(ws.join(".env"), ws.join("notes.txt")).unwrap();
            assert!(
                is_sensitive_read_path(&ws.join("notes.txt")),
                "别名要按真实名字判"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn protected_path_only_looks_below_the_workspace() {
        let ws = Path::new("/home/me/secrets-project");
        assert!(is_protected_path(ws, &ws.join("secrets")));
        assert!(is_protected_path(ws, &ws.join("secrets/astra.json")));
        assert!(is_protected_path(ws, &ws.join("src/secrets/x")));
        assert!(!is_protected_path(ws, &ws.join("src/main.rs")));
        assert!(!is_protected_path(ws, ws));
    }

    // ───────────── 可声明的追加清单（--private / UI_PRIVATE） ─────────────
    //
    // 清单是进程级可变的，测试之间会互相污染，所以每个用例自己清空、
    // 自己登记，并且**只断言自己加的那几条**——不去数总长度。

    #[test]
    fn declared_relative_paths_match_at_any_depth() {
        let _g = private_test_guard();
        clear_private();
        add_private("vendor/keys").unwrap();
        add_private("id_rsa").unwrap();
        let ws = Path::new("/ws");
        assert!(is_private_path(ws, &ws.join("vendor/keys/deploy.pem")));
        // 连续段：中间隔一层就不算同一个路径
        assert!(!is_private_path(ws, &ws.join("vendor/deploy/key.pem")));
        // 但嵌套位置要算：工作区里任何一处 `vendor/keys` 都是它
        assert!(is_private_path(ws, &ws.join("srv/vendor/keys/a.txt")));
        assert!(is_private_path(ws, &ws.join(".ssh/id_rsa")));
        // 名字型谓词只认单段声明：`vendor/keys` 排不进 --exclude-dir
        assert!(is_private_dir_name("id_rsa"));
        assert!(!is_private_dir_name("keys"));
        assert!(!is_private_dir_name("vendor"));
        // 默认清单一起生效，追加不会把它挤掉
        assert!(is_private_path(ws, &ws.join("secrets/api.txt")));
        assert!(is_private_dir_name(".git"));
        clear_private();
    }

    #[test]
    fn case_and_separators_do_not_slip_past_a_declaration() {
        let _g = private_test_guard();
        clear_private();
        // 声明侧写反斜杠、写大写都归一成同一份段表：Windows 上的
        // `--private Vendor\Keys` 与 Linux 上的 `--private vendor/keys` 是同一条
        add_private("Vendor\\Keys").unwrap();
        let ws = Path::new("/ws");
        assert!(is_private_path(ws, &ws.join("vendor/KEYS/deploy.pem")));
        assert!(is_private_path(ws, &ws.join("VENDOR/keys/deploy.pem")));
        assert!(!is_private_path(ws, &ws.join("vendor/keys2/a")));
        // 判定**输入**侧故意不归一：Linux 上 `vendor\keys\a` 是一个合法文件名，
        // 不是三层目录。替它拆分会误伤真实含反斜杠的文件，那是越界而不是收口。
        // （Windows 上分量本来就按 `\` 切，同一条断言会反过来，所以只在 unix 上钉。）
        #[cfg(unix)]
        assert!(!is_private_path(ws, &ws.join(r"vendor\keys\a")));
        clear_private();
    }

    #[test]
    fn absolute_and_tilde_declarations_cover_paths_outside_the_workspace() {
        let _g = private_test_guard();
        clear_private();
        add_private("/etc/ssh").unwrap();
        let ws = Path::new("/ws");
        assert!(is_private_path(ws, Path::new("/etc/ssh/sshd_config")));
        assert!(!is_private_path(ws, Path::new("/etc/hosts")));
        // 工作区外的绝对声明不进策略规则表：那条路本来就被
        // `<ws>/** → deny`（工作区之外不读）挡住了，重复写只会多两条看不出差别的规则
        let outside = private_subpaths(ws);
        assert!(!outside.contains(&"/etc/ssh".to_string()));
        clear_private();
    }

    #[test]
    fn home_expansion_is_registered_but_rejects_an_empty_tail() {
        let _g = private_test_guard();
        clear_private();
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok());
        let Some(home) = home else {
            // 没有 HOME 的机器（CI 里的裸 sh）不该把 `~/x` 静默当成相对路径
            assert!(add_private("~/.aws").is_ok());
            clear_private();
            return;
        };
        add_private("~/.aws/credentials").unwrap();
        let want = Path::new(&home).join(".aws").join("credentials");
        let ws = Path::new("/ws");
        assert!(
            is_private_path(ws, &want),
            "{} 应按展开后的绝对路径命中",
            want.display()
        );
        // `~` 单独一条没有可匹配的尾段：那是「把家目录整片挡掉」的误写
        assert!(expand_home("~").is_none());
        assert!(expand_home("~/").is_none());
        clear_private();
    }

    #[test]
    fn bad_declarations_are_refused_not_silently_dropped() {
        let _g = private_test_guard();
        clear_private();
        assert!(add_private("").is_err());
        assert!(add_private("   ").is_err());
        // 含 `..` 的声明会把判定变成「往上跳」，边界不该由一个相对片段决定
        assert!(add_private("secrets/../.ui").is_err());
        assert!(add_private("../outside").is_err());
        // 引号是 shell 层面漏进来的，不该变成路径的一部分
        add_private("\"vendor/keys\"").unwrap();
        assert!(private_entries().iter().any(|e| e == "vendor/keys"));
        // 重复登记不产生第二份判定（同一 raw 只留一条）
        assert!(add_private("vendor/keys").is_ok());
        assert_eq!(private_entries().iter().filter(|e| *e == "vendor/keys").count(), 1);
        clear_private();
    }

    #[test]
    fn in_workspace_declarations_become_policy_subpaths() {
        let _g = private_test_guard();
        clear_private();
        // `private_subpaths` 拿的是 canonicalize 之后的 workspace；
        // 用一个真实存在的临时目录，否则 strip_prefix 会因软链/大小写失配
        let dir = std::env::temp_dir().join(format!("ui-priv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.canonicalize().unwrap();
        add_private(ws.join("vendor/keys").to_str().unwrap()).unwrap();
        add_private("/definitely/not/in/workspace").unwrap();
        let subs = private_subpaths(&ws);
        assert!(
            subs.contains(&"vendor/keys".to_string()),
            "工作区内的绝对声明要能变成策略规则：{:?}",
            subs
        );
        assert_eq!(subs.iter().filter(|s| s.contains("not/in/workspace")).count(), 0);
        clear_private();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_lines_mention_declared_paths() {
        let _g = private_test_guard();
        clear_private();
        add_private("vendor/keys").unwrap();
        add_private("~/.aws/credentials").unwrap();
        // shell 的读拦不住（Landlock 只管写），命令串是唯一的观测面
        assert_eq!(mentioned_in("cat vendor/keys/deploy.pem").as_deref(), Some("vendor/keys"));
        assert_eq!(mentioned_in("cat VENDOR/KEYS/x").as_deref(), Some("vendor/keys"));
        assert_eq!(mentioned_in("ls ~/.aws/credentials").as_deref(), Some("~/.aws/credentials"));
        assert_eq!(mentioned_in("cat secrets/api.txt").as_deref(), Some("secrets"));
        // `.git` 在默认清单上，命令里出现它就是命中——「git status」这种
        // 不带路径的写法不会误伤（切出来是 "git status" 一整段）
        assert_eq!(mentioned_in(".git/config").as_deref(), Some(".git"));
        assert!(mentioned_in("git status").is_none());
        assert!(mentioned_in("echo hi").is_none());
        clear_private();
    }

    /// 效果目标是绝对路径时带着工作区前缀。工作区自己碰巧住在 `secrets/`
    /// 下面，不能因此让每一次读写都算命中清单。
    #[test]
    fn the_workspace_prefix_itself_is_not_a_declaration() {
        let ws = Path::new("/home/me/secrets/proj");
        assert!(
            mentioned_in_under(ws, "/home/me/secrets/proj/notes.txt").is_none(),
            "工作区名字里的 secrets 不算"
        );
        assert!(
            mentioned_in_under(ws, "cat /home/me/secrets/proj/notes.txt").is_none(),
            "命令里写出整个前缀也不算"
        );
        // 剥掉前缀之后，里面真正的隐私路径照旧命中
        assert_eq!(
            mentioned_in_under(ws, "/home/me/secrets/proj/secrets/api.txt").as_deref(),
            Some("secrets")
        );
        assert_eq!(
            mentioned_in_under(ws, "/home/me/secrets/proj/.ui/journal.jsonl").as_deref(),
            Some(".ui")
        );
        // 声明也按剥完前缀的形状匹配：工作区就叫 vendor/keys 时不该自咬
        let _g = private_test_guard();
        set_privates(&["vendor/keys"]).unwrap();
        assert!(mentioned_in_under(
            Path::new("/ws/vendor/keys"),
            "/ws/vendor/keys/deploy.pem"
        )
        .is_none());
        assert_eq!(
            mentioned_in_under(Path::new("/ws"), "/ws/vendor/keys/deploy.pem").as_deref(),
            Some("vendor/keys")
        );
        // 根目录当工作区时不能把整条目标挖空
        assert_eq!(
            mentioned_in_under(Path::new("/"), "secrets/api.txt").as_deref(),
            Some("secrets")
        );
        set_privates(&[]).unwrap();
    }

    #[test]
    fn setting_the_list_is_all_or_nothing() {
        let _g = private_test_guard();
        set_privates(&["vendor/keys"]).unwrap();
        assert!(is_private_path(Path::new("/ws"), Path::new("/ws/vendor/keys/a")));
        // 一条写错就整体不改：半张清单比没有清单更危险，人会以为已经挡住了
        let err = set_privates(&["notes", "bad/../.."]).unwrap_err();
        assert!(err.contains(".."), "报错要说清是哪一条：{}", err);
        assert!(
            is_private_path(Path::new("/ws"), Path::new("/ws/vendor/keys/a")),
            "失败的那次调用把旧清单写坏了"
        );
        assert!(!is_private_path(Path::new("/ws"), Path::new("/ws/notes/a")));
        // 成功的那次是**替换**而不是累加：重设成 [] 就回到只有默认清单
        set_privates(&[]).unwrap();
        assert!(!is_private_path(Path::new("/ws"), Path::new("/ws/vendor/keys/a")));
        assert!(is_private_path(Path::new("/ws"), Path::new("/ws/secrets/a")));
        assert_eq!(private_entries().len(), PROTECTED_DIRS.len());
    }

    #[test]
    fn declarations_are_marked_as_such_for_display() {
        let _g = private_test_guard();
        clear_private();
        // doctor 要把「默认挡住」和「你让它挡住」分开印
        assert!(!is_declared("secrets"));
        add_private("vendor/keys").unwrap();
        assert!(is_declared("vendor/keys"));
        assert!(!is_declared("src"));
        clear_private();
    }
}
