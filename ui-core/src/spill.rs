//! 落盘原文的生命周期：`.ui/tool-output/`。
//!
//! 这个目录里放的是**内核替模型留的原文**，两种来源：
//!   - `tool/output` 命中隐私清单时，正文不进日志，只留字数、指纹和一个指针；
//!   - `tool/trim`：超长输出留在盘上，上下文里只留首尾 + 指针（按需展开）。
//!
//! 于是有了两个必须一起成立的要求：
//!   1. **不能无限攒**。日志干净不等于这台机器干净——凭据原文会以 0600 的
//!      形式一直躺在这儿，跑几小时的 serve 还能把盘写满。
//!   2. **不能假装它们还在**。日志里的指针、历史里的"全文在 …"是既成事实，
//!      清理之后必须说"已清理"，而不是留下一个点开就 404 的引用。
//!
//! 所以这里给：文件名与归属判定（`is_kernel_spill`：只清内核自己写的东西）、
//! 保留策略（`Policy`：先到先清 + 天数上限）、清理（`prune`）、脚印统计
//! （`footprint`）、悬空指针（`dangling`），以及 0600 写法（`write_private`）。

use crate::json::Json;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 截断缓存：模型**可以**按需读回的那一份原文。
///
/// 它是隐私清单唯一开了读例外的地方（`protected::PROTECTED_READ_EXCEPTIONS`），
/// 例外存在的理由只在 `tool/trim`：长输出留在盘上，上下文里只留一小片，
/// 模型要能把全文读回来。
pub const REL_DIR: &str = ".ui/tool-output";

/// 按来源扣留的原文：只给人看，模型读不到。
///
/// 为什么不和截断缓存放一起（这是本模块唯一一次目录分裂）：那两个字的
/// 隐私语义相反。`tool/output` 命中隐私清单时，内核判定的是"这段正文不该
/// 出现在日志、导出、回放里"；而日志里留下的那句"全文在 …"本身就是给模型
/// 看的提示词——它一次 `fs_read` 就把正文完整捞回来了，第二层隐私塌成一层。
/// 放在读例外**之外**，模型知道文件在哪、但拿不到内容，人才是那个能决定
/// 要不要 `cat` 一下的人。
pub const PRIVATE_DIR: &str = ".ui/tool-private";

/// 内核会写、也会管的两个落盘目录。`withheld` 决定放哪一个。
pub fn dir_with(withheld: bool) -> &'static str {
    if withheld {
        PRIVATE_DIR
    } else {
        REL_DIR
    }
}

pub fn dir(workspace: &Path) -> PathBuf {
    workspace.join(REL_DIR)
}

/// 只给人看的那一个（扣留原文）。
pub fn private_dir(workspace: &Path) -> PathBuf {
    workspace.join(PRIVATE_DIR)
}

/// 两个都要扫一遍的地方：保留策略、脚印统计、`ui secrets` 共用这一份，
/// 少一个目录就等于"扫过了"这句话是假的。
pub fn dirs(workspace: &Path) -> Vec<PathBuf> {
    vec![dir(workspace), private_dir(workspace)]
}

/// 这个指针字符串指的地方，是不是内核落盘的原文（任一目录）。
pub fn is_spill_pointer(rel: &str) -> bool {
    let text = rel.replace('\\', "/");
    let base = match text.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => return false,
    };
    let base = base.trim_end_matches('/');
    base.ends_with(REL_DIR) || base.ends_with(PRIVATE_DIR)
}

/// 默认保留策略：先到先清——7 天、200 个、256 MiB，谁先到听谁的。
///
/// 天数为什么敢给 7：这个目录里是**已经抹出日志的原文**，攒得越久，
/// "日志里没有"这句承诺在这台机器上就越站不住。留 7 天是给了排查窗口，
/// 不是给了永久留底；要永久留底请显式 `UI_SPILL_KEEP=all`（`ui spill` 会一直
/// 提示现在挂的是哪一档，不会让人以为已经在自动清）。
pub const DEFAULT_MAX_AGE_DAYS: u64 = 7;
pub const DEFAULT_MAX_FILES: usize = 200;
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 内核自己的命名：`{tool_seq:04}-{writer}-{journal_seq}-{tool}.txt`。
///
/// 带 writer 与日志序号是因为 `tool_seq` 每个 run 归零，不带就会让后一个 run
/// 覆盖前一个 run 的原文——上一条会话历史里那句"全文在 …"会指到别人的输出上。
pub fn file_name(tool_seq: u64, writer: &str, journal_seq: u64, tool: &str) -> String {
    format!("{:04}-{}-{}-{}.txt", tool_seq, writer, journal_seq, tool)
}

/// 这个名字是不是内核写下的原文文件。
///
/// 清理必须只动自己的东西：`ui spill clean` 把用户放进这个目录的文件当
/// "非我莫属的缓存"删掉，那就是数据损毁，不是隐私管理。
pub fn is_kernel_spill(name: &str) -> bool {
    let stem = match name.strip_suffix(".txt") {
        Some(s) => s,
        None => return false,
    };
    let parts: Vec<&str> = stem.split('-').collect();
    // 工具名里可能还有 '-'（`fs.read`、`mcp:file-list:tool`），所以只看前四段
    if parts.len() < 4 {
        return false;
    }
    let four = parts[0].len() == 4 && parts[0].bytes().all(|b| b.is_ascii_digit());
    let writer = parts[1].len() == 8 && parts[1].bytes().all(|b| b.is_ascii_hexdigit());
    let seq = !parts[2].is_empty() && parts[2].bytes().all(|b| b.is_ascii_digit());
    four && writer && seq
}

/// 建落盘目录，并把权限收到 0700（Windows 上没有这个概念，原样建好）。
///
/// 文件写 0600 只是故事的一半：目录 0755 时同机任何用户还能**列出文件名**，
/// 而名字里带着工具名与调用序号，够拼出"这个会话在什么时候跑了什么命令"。
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 0600 写入：只给本机用户看。
///
/// 这些文件可能是密钥原文，而这个目录又是隐私清单上唯一能读的地方。
/// 默认的 0644（共享组可读）等于把两件事合起来变成"同机任何用户都能读到
/// 这个会话的凭据"。Windows 上 `PermissionsExt` 不存在，ACL 要走 WinAPI：
/// 这里不改权限，真正的边界在清单与脱敏那两层。
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(content.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub max_age_days: u64,
    pub max_files: usize,
    pub max_bytes: u64,
    /// 一个都不动：给"我就是要把原文留在盘上排查"的人留的显式档位
    pub keep_all: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_age_days: DEFAULT_MAX_AGE_DAYS,
            max_files: DEFAULT_MAX_FILES,
            max_bytes: DEFAULT_MAX_BYTES,
            keep_all: false,
        }
    }
}

/// 天数、个数这类上限：只认非负整数，没有后缀这一说。
fn lookup_num(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<u64> {
    get(name)?.trim().parse::<u64>().ok()
}

/// 设了、trim 之后非空的那些环境变量的原值。
fn raw(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// 字节上限，接受 `512b / 1k / 256m / 2g`（大小写不限），纯数字按字节。
///
/// 这一档要人手写进 shell 配置里。逼人算 `268435456` 只会换来写错，而写错
/// 不会崩——它只会让"限了容量"这句话变成假的。
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1024u64),
        Some('m') => (&t[..t.len() - 1], 1024 * 1024),
        Some('g') => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        Some('b') => (&t[..t.len() - 1], 1),
        _ => (t.as_str(), 1),
    };
    let base: u64 = num.trim().parse().ok()?;
    base.checked_mul(mult)
}

impl Policy {
    /// `UI_SPILL_KEEP=all` / `UI_SPILL_MAX_DAYS` / `UI_SPILL_MAX_FILES` /
    /// `UI_SPILL_MAX_BYTES`。和 `UI_REDACT`、`UI_PRIVATE` 同一套路：容器里改不动
    /// 命令行时，环境变量是同一个入口。
    ///
    /// 「全留」只认 `all`/`never`/`off` 这三个词，**不认 `0`**：在年龄这个语境里
    /// `0` 通长是"立刻过期"（`UI_SPILL_MAX_DAYS=0` 就是一个不留），同一个 0 在
    /// 另一处变成"永远留"，写错的人会在三个月后才发现自己留了什么在盘上。
    pub fn from_env() -> Policy {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }

    /// 把"读环境"抽成参数：单测要能构造 `UI_SPILL_MAX_BYTES=1k` 这种输入，
    /// 又不能真去改进程环境——同一个测试二进制里跑着的 agent 测试会读到它。
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Policy {
        let base = Policy::default();
        let keep = raw(get, "UI_SPILL_KEEP")
            .unwrap_or_default()
            .to_ascii_lowercase();
        Policy {
            keep_all: keep == "all" || keep == "never" || keep == "off",
            max_age_days: lookup_num(get, "UI_SPILL_MAX_DAYS").unwrap_or(base.max_age_days),
            max_files: lookup_num(get, "UI_SPILL_MAX_FILES")
                .map(|v| v as usize)
                .unwrap_or(base.max_files),
            max_bytes: raw(get, "UI_SPILL_MAX_BYTES")
                .and_then(|v| parse_size(&v))
                .unwrap_or(base.max_bytes),
        }
    }

    /// `UI_SPILL_*` 里"设了却没被采纳"的那些。
    ///
    /// 退回默认值本身是对的（一个写坏的旋钮不该让内核罢工），但不能默默退：
    /// 隐私上限写错的人，最晚会在某个不该有的场合发现盘上攒着什么。
    pub fn warnings() -> Vec<String> {
        Self::warnings_from(&|k| std::env::var(k).ok())
    }

    pub fn warnings_from(get: &impl Fn(&str) -> Option<String>) -> Vec<String> {
        let mut out = Vec::new();
        for (name, kind) in [("UI_SPILL_MAX_DAYS", "天数"), ("UI_SPILL_MAX_FILES", "个数")] {
            if let Some(v) = raw(get, name) {
                if lookup_num(get, name).is_none() {
                    out.push(format!("{name}=\"{v}\" 不是一个{kind}，这一档没生效（沿用默认）"));
                }
            }
        }
        if let Some(v) = raw(get, "UI_SPILL_MAX_BYTES") {
            if parse_size(&v).is_none() {
                out.push(format!(
                    "UI_SPILL_MAX_BYTES=\"{v}\" 看不懂，这一档没生效（认 512b/1k/256m/2g，或纯字节数）"
                ));
            }
        }
        if let Some(v) = raw(get, "UI_SPILL_KEEP") {
            let k = v.to_ascii_lowercase();
            if !matches!(k.as_str(), "all" | "never" | "off") {
                out.push(format!(
                    "UI_SPILL_KEEP=\"{v}\" 不认识，这一档没生效（只认 all/never/off；                     想要\"一个不留\"那是 UI_SPILL_MAX_DAYS=0——0 在年龄里是立刻过期，不是永远保留）"
                ));
            }
        }
        out
    }

    /// 给人看的一行：现在到底是哪一档、以什么为准。
    pub fn describe(&self) -> String {
        if self.keep_all {
            return "保留全部原文（UI_SPILL_KEEP=all：不清，代价由你承担）".to_string();
        }
        format!(
            "{} 天 / {} 个 / {} MiB，先到先清",
            self.max_age_days,
            self.max_files,
            (self.max_bytes + 1024 * 1024 - 1) / (1024 * 1024)
        )
    }
}

/// 这个文件"最近一次被人碰到"是什么时候；两个时间都读不到时返回 `None`。
///
/// 为什么不是只看 mtime：截断缓存是要被反复 `fs_read` 回去的，读只动 atime
/// 不动 mtime——只看 mtime 会把一份还在用的活跃缓存当旧的清掉。
/// 为什么不算 ctime / birth time：`chmod`、`cp -a`、备份同步都会刷新它们，
/// 一次无关的元数据改动不该给一份隐私原文续命。
///
/// `None` 由调用方换算成"当新的处理"（见 `Entry::touched_ms`）：读不出时间
/// 不是删除理由，把它当成纪元 0 反而会排进"最旧的先删"第一个。
fn touched(path: &Path) -> Option<u64> {
    let meta = path.metadata().ok()?;
    let mut best: Option<u64> = None;
    for t in [
        meta.modified().ok(),
        meta.accessed().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(d) = t.duration_since(UNIX_EPOCH) {
            let ms = d.as_millis() as u64;
            if best.map(|b| ms > b).unwrap_or(true) {
                best = Some(ms);
            }
        }
    }
    best
}

/// 看不见时间戳时用的哨兵值：比任何"现在"都新，所以永远不会被年龄那一刀砍到。
const TIME_UNKNOWN: u64 = u64::MAX;

/// 目录里的一个文件。`mine` = 名字是内核的命名格式，因此**清理可以动它**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// mtime 与 atime 里较新的那个；读不到就是 `TIME_UNKNOWN`（当新的处理）
    pub touched_ms: u64,
    pub mine: bool,
}

/// 目录清单，最旧的在前。目录不存在 = 空清单（这个工作区还没落过原文）。
///
/// 顺序依据"最近被碰到"而不是文件名：`tool_seq` 每个 run 归零，按名字排会把
/// 上一个 run 的东西排到前面。统计、决策、`ui spill` 的列表都读这一份，
/// 三处不会再对"这里有什么"有不同看法。
pub fn listing(path: &Path) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let entries = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for e in entries.flatten() {
        let p = e.path();
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        out.push(Entry {
            mine: is_kernel_spill(&name),
            bytes: meta.len(),
            touched_ms: touched(&p).unwrap_or(TIME_UNKNOWN),
            name,
            path: p,
        });
    }
    out.sort_by_key(|e| e.touched_ms);
    out
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Footprint {
    pub files: usize,
    pub bytes: u64,
    /// 不是内核命名的文件数：清理不会碰它们，但要如实报出来
    pub foreign: usize,
}

pub fn footprint(path: &Path) -> Footprint {
    let mut f = Footprint::default();
    for e in listing(path) {
        f.files += 1;
        f.bytes += e.bytes;
        if !e.mine {
            f.foreign += 1;
        }
    }
    f
}

/// 一份"该走哪些"的清单，**只看不动**。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// 最旧的在前
    pub remove: Vec<Entry>,
    pub bytes: u64,
    /// 不是内核命名，策略再狠也不碰
    pub skipped_foreign: usize,
    pub kept: Footprint,
    /// 策略是全留：`remove` 为空的意思是"我没清"，不是"这里没东西"
    pub held: bool,
}

/// 按策略算一遍：先按天数过期，再从最旧的开始退到"个数/字节"限内。
///
/// 决策为什么单独是一个函数：`ui spill clean --dry-run` 必须给出和真删**同
/// 一份**清单。两处各写一遍淘汰逻辑，早晚会有一处漏掉某个上限，而人是在
/// dry-run 的输出上按下"就这么删"的。
/// 单个目录的决策。落盘原文有**两个**目录（模型可读的截断缓存 ＋ 只给人看的
/// 扣留原文），预算要在合起来的清单上算，所以这层只是 `plan_all` 的壳。
/// 同时要管两个目录的地方请直接用 `plan_all`。
pub fn plan(path: &Path, policy: &Policy, now_ms: u64) -> Plan {
    decide(listing(path), policy, now_ms)
}

/// 多个目录**一起**算：保留策略管的是"这台机器上还留着多少份原文"。按目录分别
/// 算等于每个目录都领一份完整预算——200 个变 400 个，而且没人报错。
pub fn plan_all(paths: &[PathBuf], policy: &Policy, now_ms: u64) -> Plan {
    decide(listings(paths), policy, now_ms)
}

/// 合并清单，最旧的在前（"谁最旧"只能跨目录问）。
pub fn listings(paths: &[PathBuf]) -> Vec<Entry> {
    let mut all: Vec<Entry> = paths.iter().flat_map(|p| listing(p)).collect();
    all.sort_by_key(|e| e.touched_ms);
    all
}

/// 合并脚印：`ui spill` 与 `ui doctor` 报的是"盘上总共留了多少原文"。
pub fn footprints(paths: &[PathBuf]) -> Footprint {
    let mut acc = Footprint::default();
    for e in listings(paths) {
        acc.files += 1;
        acc.bytes += e.bytes;
        if !e.mine {
            acc.foreign += 1;
        }
    }
    acc
}

/// 决策本体：先过年龄这道隐私闸，再从最旧的开始退到个数/字节上限之内。
fn decide(all: Vec<Entry>, policy: &Policy, now_ms: u64) -> Plan {
    let skipped_foreign = all.iter().filter(|e| !e.mine).count();
    let mut out = Plan {
        skipped_foreign,
        ..Plan::default()
    };
    if policy.keep_all {
        out.held = true;
        out.kept = Footprint {
            files: all.len(),
            bytes: all.iter().map(|e| e.bytes).sum(),
            foreign: skipped_foreign,
        };
        return out;
    }
    let mine: Vec<Entry> = all.into_iter().filter(|e| e.mine).collect();

    let age_limit = now_ms.saturating_sub(policy.max_age_days.saturating_mul(24 * 3600 * 1000));
    // 第一刀：过期的，不论容量够不够——天数是隐私闸，不是容量闸。
    let mut rest: Vec<Entry> = Vec::new();
    for e in mine {
        if e.touched_ms < age_limit {
            out.bytes += e.bytes;
            out.remove.push(e);
        } else {
            rest.push(e);
        }
    }
    // 第二刀：还在窗口里，但个数/字节超了顶，同样从最旧的开始退。
    let mut count = rest.len();
    let mut bytes: u64 = rest.iter().map(|e| e.bytes).sum();
    for e in rest {
        if count <= policy.max_files && bytes <= policy.max_bytes {
            break;
        }
        out.bytes += e.bytes;
        count -= 1;
        bytes -= e.bytes;
        out.remove.push(e);
    }
    out.kept = Footprint {
        files: count,
        bytes,
        foreign: skipped_foreign,
    };
    out
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pruning {
    pub removed: usize,
    pub freed_bytes: u64,
    /// 在 `plan` 里、但真删的时候没删掉（只读挂载、被占用）。
    /// 报 0 和报"我以为删了"是两回事：前者是状态，后者是谎。
    pub failed: usize,
    /// 因为不是内核命名而留下来的
    pub skipped_foreign: usize,
    pub kept: Footprint,
    /// 策略是"全留"时为 true：removed=0 但有话要说，别让人以为清失败
    pub held: bool,
}

/// 目录存在却列不出来（权限、坏挂载）。这**不等于**"里面是空的"。
///
/// `plan`/`footprint` 都把 `read_dir` 失败当成空目录处理——没有东西可删确实是
/// 对的，但"这里没原文"和"我看不见这里"是两句话，调用方要用这个先问一次，
/// 否则一个读不了的目录会被报成最理想的状态。
pub fn unreadable(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound && path.exists(),
    }
}

/// 执行 `plan`：逐个删，删不掉的如实计数，然后把留下的那份统计出来。
///
/// 它不返回 `Result`：唯一会失败的两件事（列不出目录、删不掉文件）都已经
/// 变成结构里的字段（`unreadable` 与 `failed`），调用方不需要再处理一次错误。
/// 单目录版：见 `plan` 的说明，两个目录要一起管用 `prune_all`。
pub fn prune(path: &Path, policy: &Policy, now_ms: u64) -> Pruning {
    prune_all(&[path.to_path_buf()], policy, now_ms)
}

/// 按策略真删，**多个目录共用同一份预算**。
pub fn prune_all(paths: &[PathBuf], policy: &Policy, now_ms: u64) -> Pruning {
    let p = plan_all(paths, policy, now_ms);
    let mut out = Pruning {
        skipped_foreign: p.skipped_foreign,
        held: p.held,
        ..Pruning::default()
    };
    for e in &p.remove {
        if std::fs::remove_file(&e.path).is_ok() {
            out.removed += 1;
            out.freed_bytes += e.bytes;
        } else {
            out.failed += 1;
        }
    }
    // 以盘上的实际情况收尾：失败的那几个、外来的那些都会体现在这里
    out.kept = footprints(paths);
    out
}

/// 日志里指向落盘原文、但文件已经不在了的那些指针。
///
/// 为什么要单独报：清理是对的，但"账上写着全文在 X，X 没了"必须能被主动
/// 发现，否则人只能在模型下一次 `fs_read` 失败时才撞上它。
pub fn dangling(events: &[Json], workspace: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in events {
        let ptr = match e.str_at("file") {
            Some(p) if !p.is_empty() => p,
            _ => continue,
        };
        if !is_spill_pointer(ptr) {
            continue;
        }
        let cand = if Path::new(ptr).is_absolute() {
            PathBuf::from(ptr)
        } else {
            workspace.join(ptr.replace('\\', "/"))
        };
        if !cand.exists() {
            out.push(ptr.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_for(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ui-spill-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个"内核写的"文件，并把它的 mtime **和 atime** 一起挪到 days_ago 天前。
    ///
    /// 两个都要挪：年龄判据是"最近被碰到"= max(mtime, atime)（见 `touched`），
    /// 只改 mtime 的文件在判据眼里还是新的。这也正是 smoke 里用
    /// `touch -d '30 days ago'` 的原因——那个命令同样两个都设。
    fn seeded(d: &Path, seq: u64, tool: &str, days_ago: u64, bytes: usize) -> PathBuf {
        let p = d.join(file_name(seq, "deadbeef", seq * 10, tool));
        std::fs::write(&p, "x".repeat(bytes)).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days_ago * 24 * 3600);
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when).set_accessed(when))
            .unwrap();
        p
    }

    #[test]
    fn only_kernel_named_files_are_mine_to_delete() {
        // 清理误删用户放进这个目录的东西，是数据损毁，不是隐私管理。
        // 用 max_files=0 逼它真的动手：默认档下什么都不会删，那条断言就只是装饰。
        let d = dir_for("ownership");
        let mine = seeded(&d, 1, "shell", 0, 10);
        std::fs::write(d.join("my-notes.txt"), "别删我").unwrap();
        std::fs::write(d.join("0001-shell.txt"), "格式不对，别删我").unwrap();
        std::fs::create_dir_all(d.join("subdir.txt")).unwrap();

        let p = prune(
            &d,
            &Policy {
                max_files: 0,
                ..Policy::default()
            },
            crate::time::now_ms(),
        );
        assert_eq!(p.removed, 1, "只该动内核那一个: {:?}", p);
        assert!(!mine.exists());
        assert!(d.join("my-notes.txt").exists(), "人的文件必须在");
        assert!(d.join("0001-shell.txt").exists(), "少两段的一律不是我的");
        assert!(d.join("subdir.txt").is_dir(), "目录不删");
        assert_eq!(p.skipped_foreign, 2, "非我命名要如实报数");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_oldest_originals_go_first_under_both_caps() {
        let d = dir_for("caps");
        // days_ago 越大越旧：i=0 是 6 天前（最旧），i=5 是 1 天前。
        // 全都落在默认 7 天窗口里，所以这里动的只可能是容量那一刀。
        for i in 0..6u64 {
            seeded(&d, i, "shell", 6 - i, 100);
        }
        // 个数封顶 3：留最新的 3 个
        let policy = Policy {
            max_files: 3,
            ..Policy::default()
        };
        let p = prune(&d, &policy, crate::time::now_ms());
        assert_eq!(p.removed, 3);
        assert_eq!(p.kept.files, 3);
        assert!(p.kept.bytes <= 300);
        // 0..2 是旧的，必须已经没了；3..5 还在
        assert!(!d.join(file_name(0, "deadbeef", 0, "shell")).exists());
        assert!(!d.join(file_name(2, "deadbeef", 20, "shell")).exists());
        assert!(d.join(file_name(5, "deadbeef", 50, "shell")).exists());

        // 字节封顶 250：300 的存量要退到 2 个（200 字节）
        let by_bytes = Policy {
            max_files: 99,
            max_bytes: 250,
            ..Policy::default()
        };
        let p2 = prune(&d, &by_bytes, crate::time::now_ms());
        assert_eq!(p2.removed, 1, "按字节也该挤掉最旧那个: {:?}", p2);
        assert_eq!(p2.kept.files, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn age_is_a_hard_limit_even_when_caps_are_wide() {
        // 天数是隐私闸，不是容量闸：留得下不等于该留。
        let d = dir_for("age");
        let stale = seeded(&d, 1, "shell", 30, 10);
        let fresh = seeded(&d, 2, "shell", 1, 10);
        let wide = Policy {
            max_files: 100,
            max_bytes: 1024 * 1024,
            max_age_days: 7,
            ..Policy::default()
        };
        let p = prune(&d, &wide, crate::time::now_ms());
        assert_eq!(p.removed, 1);
        assert!(!stale.exists(), "30 天前的原文不该还在");
        assert!(fresh.exists(), "1 天前的还在保留窗口里");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_dry_run_names_the_same_files_the_real_run_would() {
        // `ui spill clean --dry-run` 的全部价值在于：人看的是这份清单，
        // 按下的却是那份执行。两边必须是同一次计算，否则"预览"就是误导。
        let d = dir_for("plan");
        for i in 0..4u64 {
            seeded(&d, i, "shell", 3 - i, 50);
        }
        let policy = Policy {
            max_files: 2,
            ..Policy::default()
        };
        let now = crate::time::now_ms();
        let p = plan(&d, &policy, now);
        assert_eq!(p.remove.len(), 2, "4 个里该走 2 个: {:?}", p.remove);
        assert_eq!(p.bytes, 100);
        assert_eq!(footprint(&d).files, 4, "算一遍不许动盘");
        assert_eq!(p.kept.files, 2);
        assert_eq!(p.kept.bytes, 100);
        // 最旧的那个（3 天前的 i=0）排在清单第一个
        let named: Vec<String> = p.remove.iter().map(|e| e.name.clone()).collect();
        assert_eq!(named[0], file_name(0, "deadbeef", 0, "shell"));

        let r = prune(&d, &policy, now);
        assert_eq!(r.removed, 2);
        assert_eq!(r.failed, 0, "普通目录不该有删不掉的");
        for n in &named {
            assert!(!d.join(n).exists(), "预览里点名的必须真的没了: {}", n);
        }
        assert_eq!(r.kept, p.kept, "执行后的存量要和预览算出来的一致");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keep_all_says_it_held_instead_of_reporting_a_clean_sweep() {
        // UI_SPILL_KEEP=all 时 removed=0 的含义是"我没清"，不是"这里没东西"。
        let d = dir_for("keep-all");
        seeded(&d, 1, "shell", 300, 10);
        let p = prune(
            &d,
            &Policy {
                keep_all: true,
                ..Policy::default()
            },
            crate::time::now_ms(),
        );
        assert!(p.held, "要如实说明是策略留住的");
        assert_eq!(p.removed, 0);
        assert_eq!(p.kept.files, 1);
        assert!(d.join(file_name(1, "deadbeef", 10, "shell")).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dangling_pointers_are_found_and_named() {
        // `dangling` 拿的是**工作区**（和 `prune` 拿的落盘目录不是同一个参数），
        // 所以这里按真实的目录结构铺：ws/.ui/tool-output/…
        let ws = dir_for("dangling");
        let spill = dir(&ws);
        std::fs::create_dir_all(&spill).unwrap();
        let events = vec![
            Json::obj(vec![
                ("kind", Json::from("tool/output")),
                (
                    "file",
                    Json::from(format!("{}/0001-deadbeef-10-shell.txt", REL_DIR)),
                ),
            ]),
            Json::obj(vec![
                ("kind", Json::from("tool/trim")),
                (
                    "file",
                    Json::from(format!("{}/0002-deadbeef-20-fs_read.txt", REL_DIR)),
                ),
            ]),
            // 不是落盘原文的 file 字段不该被牵连
            Json::obj(vec![
                ("kind", Json::from("fs/read")),
                ("file", Json::from("notes/a.md")),
            ]),
        ];
        std::fs::write(
            spill.join(file_name(2, "deadbeef", 20, "fs_read")),
            "还在",
        )
        .unwrap();
        let gone = dangling(&events, &ws);
        assert_eq!(gone.len(), 1, "只报真的没了的那个: {:?}", gone);
        assert!(gone[0].contains("0001-deadbeef-10-shell.txt"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_missing_directory_is_not_an_error() {
        let p = prune(
            &Path::new("/nonexistent-ui-spill-dir"),
            &Policy::default(),
            crate::time::now_ms(),
        );
        assert_eq!(p, Pruning::default());
        assert_eq!(footprint(&Path::new("/nonexistent-ui-spill-dir")).files, 0);
    }

    #[test]
    fn a_still_read_original_does_not_age_out() {
        // 年龄看的是"最近被碰到"，不是"什么时候写的"：截断缓存的意义就在于
        // 模型隔几天还会把全文读回去一次，只看 mtime 会在它还活着的时候清掉它。
        let d = dir_for("atime");
        let p = seeded(&d, 1, "shell", 30, 20);
        let _ = std::fs::read_to_string(&p).unwrap();
        if touched(&p).map(|t| t < 1000) == Some(true) {
            // 少数挂载点挂了 noatime：读不刷新 atime，这条断言在这里没有意义
            eprintln!("跳过：这个文件系统不记录访问时间的更新");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let fresh = plan(&d, &Policy::default(), crate::time::now_ms());
        assert!(
            fresh.remove.is_empty(),
            "刚被读过的原文不该按年龄过期: {:?}",
            fresh.remove
        );
        // 反过来：两头都旧了才叫过期
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old).set_accessed(old))
            .unwrap();
        let plan2 = plan(&d, &Policy::default(), crate::time::now_ms());
        assert_eq!(plan2.remove.len(), 1, "两头都旧了才叫过期");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_budget_is_shared_across_both_directories() {
        // 拆目录时真实的错法：对每个目录各调一次 plan。上限 200 个于是变成 400 个，
        // 而两个目录各自"没超顶"看起来完全正常，没有任何一处会说这条隐私闸漏了。
        let root = dir_for("shared");
        let a = root.join("tool-output");
        let b = root.join("tool-private");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // a 里一份很旧、一份最新；b 里两份都在中间
        let oldest = seeded(&a, 1, "shell", 6, 10);
        let _ = seeded(&a, 2, "shell", 0, 10);
        let _ = seeded(&b, 3, "shell", 5, 10);
        let _ = seeded(&b, 4, "shell", 4, 10);
        let dirs = vec![a.clone(), b.clone()];
        let now = crate::time::now_ms();
        let cap = Policy {
            max_files: 3,
            ..Policy::default()
        };
        let merged = plan_all(&dirs, &cap, now);
        assert_eq!(merged.remove.len(), 1, "合起来才看得见超了顶");
        assert_eq!(
            merged.remove[0].path, oldest,
            "先清的必须是跨目录最旧的那一个"
        );
        // 分开算的版本：两边各 2 个、都没超 3，于是一个都不清——这正是当初的错法
        let split: usize = dirs
            .iter()
            .map(|d| plan(d, &cap, now).remove.len())
            .sum();
        assert_eq!(split, 0, "分开算等于每个目录各领一份完整预算");
        assert_eq!(footprints(&dirs).files, 4);
        let p = prune_all(&dirs, &cap, now);
        assert_eq!(p.removed, 1);
        assert_eq!(p.kept.files, 3, "收尾要以盘上的实际情况为准");
        assert!(!oldest.exists(), "最旧的那个该没了");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_all_counts_both_directories_too() {
        // 全留档也要给出合并后的脚印：按单目录算的话，"我没清"这句话带的是错数字
        let root = dir_for("shared-keep");
        let a = root.join("tool-output");
        let b = root.join("tool-private");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        seeded(&a, 1, "shell", 30, 10);
        seeded(&b, 2, "shell", 30, 20);
        let p = plan_all(
            &vec![a.clone(), b.clone()],
            &Policy {
                keep_all: true,
                ..Policy::default()
            },
            crate::time::now_ms(),
        );
        assert!(p.held);
        assert!(p.remove.is_empty());
        assert_eq!((p.kept.files, p.kept.bytes), (2, 30), "{:?}", p.kept);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn both_spill_dirs_answer_to_one_policy() {
        // 拆成两个目录之后最容易犯的错：`ui spill` 与 run 边界的自动清只扫第一个。
        // 那里放的是模型可读的截断缓存，第二个放的才是按清单扣留的原文——漏扫的
        // 恰好是隐私等级更高的那一份。上限按合计算，所以两个目录都要动手。
        let root = dir_for("both");
        let trim = root.join(crate::spill::REL_DIR.rsplit_once('/').unwrap().1);
        let held = root.join(crate::spill::PRIVATE_DIR.rsplit_once('/').unwrap().1);
        for d in [&trim, &held] {
            std::fs::create_dir_all(d).unwrap();
            for i in 1..=3 {
                seeded(d, i, "shell", 0, 40);
            }
        }
        assert!(dir_with(true) != crate::spill::REL_DIR, "扣留原文必须在另一个目录");
        assert_eq!(listing(&trim).len(), 3);
        assert_eq!(listing(&held).len(), 3);
        // 一个都不该动：默认策略很宽
        let p = prune(&trim, &Policy::default(), crate::time::now_ms());
        assert_eq!((p.removed, p.failed), (0, 0));
        // 上限归零 = 内核命名的全清，两个目录各自清干净，别人的文件照旧不碰
        let foreign = held.join("keepme.txt");
        std::fs::write(&foreign, "人放的").unwrap();
        let now = crate::time::now_ms();
        for d in [&trim, &held] {
            let gone = Policy {
                max_files: 0,
                ..Policy::default()
            };
            let p = prune(d, &gone, now);
            assert_eq!(p.removed, 3, "清的是内核那三个");
            assert_eq!(p.skipped_foreign, if d == &held { 1 } else { 0 });
            assert_eq!(listing(d).len(), if d == &held { 1 } else { 0 });
        }
        assert!(foreign.is_file(), "不是内核命名的，策略再狠也不该动");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pointers_into_either_spill_dir_are_recognised() {
        // `.ui/tool-private/` 是后加的第二个目录。悬空指针检测如果只认第一个，
        // 那么隐私等级更高的那一份恰恰是没人报断的那一个。
        assert!(is_spill_pointer(".ui/tool-output/0001-deadbeef-10-shell.txt"));
        assert!(is_spill_pointer(".ui/tool-private/0001-deadbeef-10-shell.txt"));
        assert!(is_spill_pointer(".ui\\tool-private\\0001-deadbeef-10-shell.txt"));
        assert!(is_spill_pointer("/home/u/ws/.ui/tool-private/0001-deadbeef-10-shell.txt"));
        assert!(!is_spill_pointer("notes/a.md"), "普通文件的 file 字段不该被牵连");
        assert!(!is_spill_pointer(".ui/journal.jsonl"), "日志本身不是落盘原文");
        assert!(
            !is_spill_pointer("tool-output-hack/x.txt"),
            "目录名要落在尾巴上，不是包含就行"
        );
        assert!(!is_spill_pointer("0001-deadbeef-10-shell.txt"), "没有目录就没有归属");
    }

    #[test]
    fn a_wrongly_written_knob_says_so_instead_of_silently_keeping_everything() {
        // 这一档的真实缺陷：`UI_SPILL_MAX_BYTES=1k` 被当成没设，退回 256 MiB。
        // 写错的人看到的是"策略已生效"，盘上攒的却是全部原文。
        let get = |k: &str| match k {
            "UI_SPILL_MAX_BYTES" => Some("1k".to_string()),
            _ => None,
        };
        let p = Policy::from_lookup(&get);
        assert_eq!(p.max_bytes, 1024, "1k 就该是 1024 个字节");
        assert!(Policy::warnings_from(&get).is_empty(), "认得的写法不该唠叨");
        assert!(p.describe().contains("1 MiB"), "给人看的档位要跟着变：{}", p.describe());
        for (bad, why) in [
            ("1 kib", "后缀里夹空格"),
            ("many", "纯错"),
            ("1kb", "多写一个 b"),
        ] {
            let get = |k: &str| {
                if k == "UI_SPILL_MAX_BYTES" {
                    Some(bad.to_string())
                } else {
                    None
                }
            };
            let w = Policy::warnings_from(&get);
            assert_eq!(w.len(), 1, "{why}：要正好说一句，不能没有也不能重复：{:?}", w);
            assert!(w[0].contains("UI_SPILL_MAX_BYTES"), "{why}：要点名是哪个变量");
            assert_eq!(Policy::from_lookup(&get).max_bytes, DEFAULT_MAX_BYTES);
        }
        // 天数与个数没有后缀这一说，写错了同样要说
        let get = |k: &str| {
            if k == "UI_SPILL_MAX_DAYS" {
                Some("7d".to_string())
            } else {
                None
            }
        };
        let w = Policy::warnings_from(&get);
        assert_eq!(w.len(), 1, "写错的天数不能默默退回默认：{:?}", w);
        assert_eq!(Policy::from_lookup(&get).max_age_days, DEFAULT_MAX_AGE_DAYS);
        // 「全留」这一档最容易被写成 0，而 0 的意思恰恰是反面
        let get = |k: &str| {
            if k == "UI_SPILL_KEEP" {
                Some("0".to_string())
            } else {
                None
            }
        };
        assert!(!Policy::from_lookup(&get).keep_all, "0 不该被当成全留");
        let w = Policy::warnings_from(&get);
        assert!(
            w.iter().any(|s| s.contains("UI_SPILL_KEEP") && s.contains("MAX_DAYS")),
            "要指到那条正确的路：{:?}",
            w
        );
        // 空值 = 没设：不该为"我 export 了个空字符串"唠叨人
        let get = |k: &str| {
            if k == "UI_SPILL_KEEP" {
                Some("   ".to_string())
            } else {
                None
            }
        };
        assert!(Policy::warnings_from(&get).is_empty(), "空值不算设过：{:?}", Policy::warnings_from(&get));
    }

    #[test]
    fn the_policy_line_tells_you_which_tier_you_are_in() {
        assert!(Policy::default().describe().contains("7 天"));
        assert!(Policy {
            keep_all: true,
            ..Policy::default()
        }
        .describe()
        .contains("UI_SPILL_KEEP=all"));
    }
}
