//! Contract 5: one side effect, one durable log entry.
//!
//! Append-only JSONL, one fsync per event, and a hash chain over the whole file.
//! Events are written *before* the action they describe, so a crash leaves a
//! readable prefix and a specific question ("this call was started, never
//! finished — re-run it or mark it unknown?"), not an apology.

use crate::json::{self, Json};
use crate::sha256;
use crate::time;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone)]
pub struct Corrupt {
    pub seq: Option<u64>,
    pub reason: String,
    /// True when only the final record is damaged (a torn write during a crash).
    /// A torn tail is repairable; damage in the middle is not.
    pub torn: bool,
    /// Byte length of the verified prefix, for truncation.
    pub good_bytes: usize,
}

pub struct Journal {
    path: PathBuf,
    seq: u64,
    prev: String,
    /// 已经读进 `events` 的字节数：append 前只补读这之后的新内容
    offset: u64,
    /// 本进程的 writer 标识：多进程共享日志时，靠它把交织的 turn 分开
    writer: String,
    /// 落账前的脱敏开关，**状态只存在这一份**（见 `append` / `append_raw`）
    redact: bool,
    pub events: Vec<Json>,
    /// 因为内存上限被挤出的事件数（日志文件里仍然完整）
    dropped: u64,
    /// turn/start 的总数。事件被挤出后不能再靠数 events 得到
    turns: u64,
}

/// 内存里最多保留多少条事件。
///
/// 为什么要上限：浸泡测试里 1200 步、10712 条事件把 RSS 从 6MB 顶到 32MB。
/// 那不是泄漏，是**无上限增长**——跑几小时的 agent 会把内存吃光。
/// 写日志只依赖 seq/prev（都是标量），历史事件只有 resume/回放需要，
/// 而它们走的是 `Journal::load`（从磁盘读全量），不依赖这个缓冲。
const MAX_KEPT_EVENTS: usize = 2000;

/// 日志写不进去时的错误要能直接看懂：说清哪个文件、哪一步、以及为什么停下。
///
/// 这条不是装饰——"日志写不进去还继续跑"等于没有审计，所以这里必须 fail closed，
/// 并且让人一眼知道是磁盘/权限问题而不是程序崩了。
fn journal_ctx(e: std::io::Error, path: &std::path::Path, what: &str) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        format!(
            "{}（{}）：{}。日志是执行记录，写不进去就不该继续执行；请检查磁盘空间与文件权限后重试。",
            what,
            path.display(),
            e
        ),
    )
}

fn writer_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    sha256::hex(format!("{}:{}", pid, nanos).as_bytes())[..8].to_string()
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// 独占锁：Unix 用 `flock`（进程崩溃时内核自动释放），其他平台用锁文件兜底。
struct FileLock {
    #[allow(dead_code)]
    file: fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

#[cfg(unix)]
impl FileLock {
    fn acquire(path: &Path) -> io::Result<FileLock> {
        use std::os::unix::io::AsRawFd;
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        // LOCK_EX = 2。阻塞等锁：日志写入是毫秒级的事，等一会儿比失败好。
        let rc = unsafe { flock(file.as_raw_fd(), 2) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileLock { file })
    }
}

#[cfg(not(unix))]
impl FileLock {
    fn acquire(path: &Path) -> io::Result<FileLock> {
        let lock = path.with_extension("lockfile");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(file) => return Ok(FileLock { file, path: lock }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // 超过 60 秒的锁文件视为遗留（进程崩了没删）
                    if let Ok(meta) = fs::metadata(&lock) {
                        if let Ok(age) = meta.modified().and_then(|m| {
                            m.elapsed().map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                        }) {
                            if age.as_secs() > 60 {
                                let _ = fs::remove_file(&lock);
                                continue;
                            }
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "等待日志锁超时（5s）",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 在同一个锁文件上执行一段「读全量 → 判断 → 追加」的临界区。
///
/// `Journal::append` 自己已经会拿 `<file>.lock`，但**冲突检查**需要把
/// 「重新加载 + 判断 + 写」放在同一个临界区里，否则两个 `ui remember`
/// 并发时会各自看到旧的空库、各写一条 live。锁序固定为
/// `<guard> → <journal>.lock`，`append` 内部只拿后者，不会反向死锁。
pub fn with_file_lock<T>(lock_path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = FileLock::acquire(lock_path)?;
    f()
}

/// 只扫一遍文件、只解析最后一条记录的结果。
struct TailScan {
    seq: u64,
    prev: String,
    /// 最后一条完整记录之后的字节位置（撕裂时就是可修复前缀长度）
    offset: u64,
    turns: u64,
    dropped: u64,
    last: Option<Json>,
    corrupt: Option<Corrupt>,
    torn_reason: Option<String>,
}

/// 扫尾部而不是全量：
///   - 逐行读，但**不解析**（只在字节里找 turn/start 标记），常数极小；
///   - 只保留最后一条完整记录并解析它，拿到 seq 与 hash；
///   - 尾部没换行 = 撕裂，交给调用方截断。
fn scan_tail(path: &Path) -> io::Result<TailScan> {
    use std::io::BufRead;
    let f = File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, f);

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut last: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    let mut good = 0u64;
    let mut turns = 0u64;
    let mut lines = 0u64;
    let mut corrupt: Option<Corrupt> = None;

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            // 崩溃窗口有两种：半条记录（坏）和“记录完整、只差换行”（好）。
            // 后者必须保留，否则它可能是已提交的 tool/start / turn/start，
            // 下一次 resume 就会把副作用重放一遍。
            let prev_hash = if last.is_empty() {
                GENESIS.to_string()
            } else {
                parse_line(&last)
                    .and_then(|e| e.str_at("hash").map(str::to_string))
                    .unwrap_or_else(|| GENESIS.to_string())
            };
            if verify_record(&buf, &prev_hash).is_some() {
                if contains(&buf, b"\"kind\":\"turn/start\"") {
                    turns += 1;
                }
                offset += n as u64;
                good = offset;
                std::mem::swap(&mut last, &mut buf);
            } else {
                corrupt = Some(Corrupt {
                    seq: None,
                    reason: format!("第 {} 行没有结尾换行且校验失败（崩溃残留）", lines + 1),
                    torn: true,
                    good_bytes: good as usize,
                });
            }
            break;
        }
        // 空行（外部工具插进来的）不是记录：不能让它成为"最后一条"，
        // 否则 seq/prev 会归零，下一次 append 就把链写断。
        let is_record = buf.iter().any(|b| !b.is_ascii_whitespace());
        if is_record {
            if contains(&buf, b"\"kind\":\"turn/start\"") {
                turns += 1;
            }
            std::mem::swap(&mut last, &mut buf);
        }
        offset += n as u64;
        good = offset;
        lines += 1;
    }

    let (seq, prev) = if last.is_empty() {
        (0, GENESIS.to_string())
    } else {
        match parse_line(&last) {
            Some(e) => (
                e.u64_at("seq").unwrap_or(0),
                e.str_at("hash").unwrap_or(GENESIS).to_string(),
            ),
            None => (0, GENESIS.to_string()),
        }
    };

    Ok(TailScan {
        seq,
        prev,
        offset: good,
        turns,
        dropped: 0,
        last: parse_line(&last),
        corrupt,
        torn_reason: None,
    })
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    // 两边都是 UTF-8 时交给 str::contains：标准库的实现够快，不必自己写
    // memchr，也不用 `windows(needle.len()).any()` 逐字节比。
    // 任一不是 UTF-8 就返回 false——日志是 JSONL，只按 UTF-8 找东西才有意义。
    match std::str::from_utf8(hay) {
        Ok(s) => match std::str::from_utf8(needle) {
            Ok(n) => s.contains(n),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

fn parse_line(line: &[u8]) -> Option<Json> {
    let text = std::str::from_utf8(line).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    json::parse(text).ok()
}

/// 校验一条记录：JSON 可解析、prev 接得上、hash 对得上。
fn verify_record(line: &[u8], expected_prev: &str) -> Option<Json> {
    let record = parse_line(line)?;
    if record.str_at("prev")? != expected_prev {
        return None;
    }
    let declared = record.str_at("hash")?.to_string();
    let mut without_hash = record.clone();
    if let Json::Obj(items) = &mut without_hash {
        items.retain(|(k, _)| k != "hash");
    }
    if sha256::hex(without_hash.to_compact().as_bytes()) != declared {
        return None;
    }
    Some(record)
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| journal_ctx(e, &path, "创建日志目录失败"))?;
        }
        // 原子创建：绝不能写成 `if !path.exists() { fs::write(path, b"") }`——
        // 两个进程同时打开时，「判断」和「写空串」之间会被另一个进程插进来，
        // 后者会把前者刚写好的日志截断（真实踩过：三进程并发丢了两条记录）。
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| journal_ctx(e, &path, "打不开日志"))?;

        // 读和修复都必须在锁里：别的 writer 正在写时，读到的是半条记录，
        // 会被当成「尾部撕裂」截掉——那等于把别人刚写的记录毁了。
        //
        // 长度也必须在**同一把锁**里取：如果出了锁再 metadata().len()，
        // 中间可能有别的写者追加，offset 就会跳过一条没解析过的记录，
        // 下一次 catch_up 看到 seq 对不上，直接报「日志链断了」。
        // 这不是理论问题——CI 上四线程并发时真的炸过。
        let (mut tail, offset) = {
            let lock_path = path.with_extension("lock");
            let _lock = FileLock::acquire(&lock_path)
                .map_err(|e| journal_ctx(e, &path, "拿不到日志锁"))?;
            // 只扫尾部：不解析历史全部记录，内存与日志大小无关。
            // 全链校验是 ui verify 的职责（流式，O(1) 内存）；
            // 这里只需要 seq/prev/offset、turn 计数、以及尾部是否撕裂。
            let mut tail = scan_tail(&path).map_err(|e| journal_ctx(e, &path, "扫描日志尾部失败"))?;
            if let Some(c) = &tail.corrupt {
                if c.torn {
                    // A crash can leave one half-written record. Keep the verified
                    // prefix, drop the tear, and say so in the log itself.
                    let file = fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(|e| journal_ctx(e, &path, "打开日志准备修复失败"))?;
                    file.set_len(c.good_bytes as u64)
                        .map_err(|e| journal_ctx(e, &path, "截断撕裂尾巴失败"))?;
                    file.sync_all()
                        .map_err(|e| journal_ctx(e, &path, "修复后落盘失败"))?;
                    tail.offset = c.good_bytes as u64;
                    tail.torn_reason = Some(c.reason.clone());
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "journal corrupt at seq {}: {} ({})",
                            c.seq.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
                            c.reason,
                            path.display()
                        ),
                    ));
                }
            }
            // 崩溃可能落在 write_all(line) 与 write_all("\n") 之间：记录本身完整、
            // 可校验，但文件没有结尾换行。此时直接 append 会把两个 JSON 对象粘成
            // 一行，而**下一次** open 会把它们一起当成尾部撕裂截掉。先补上换行。
            ensure_trailing_newline(&path)
                .map_err(|e| journal_ctx(e, &path, "补齐结尾换行失败"))?;
            let offset = fs::metadata(&path)
                .map_err(|e| journal_ctx(e, &path, "取日志大小失败"))?
                .len();
            (tail, offset)
        };

        let mut torn_note = tail.torn_reason.clone();
        let events: Vec<Json> = tail.last.take().into_iter().collect();
        let mut journal = Journal {
            path,
            seq: tail.seq,
            prev: tail.prev,
            offset,
            writer: writer_id(),
            redact: std::env::var("UI_REDACT").map(|v| v != "0").unwrap_or(true),
            events,
            dropped: tail.dropped,
            turns: tail.turns,
        };
        if let Some(reason) = torn_note.take() {
            journal.append(
                "journal/torn",
                Json::obj(vec![
                    ("reason", Json::from(reason.as_str())),
                    ("note", Json::from("崩溃时留下半条记录，已截断到最后一条完整记录")),
                ]),
            )?;
        }
        Ok(journal)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 这份日志落账前脱不脱敏。**状态只存在这里一份**。
    ///
    /// 之前它是 `Agent` 上的一个 bool 字段，于是出现了两种各自漂移的 bug：
    /// CLI 在 `begin_run()` 之后才改字段（记号漏写），以及 agent 外的写者
    /// （`turn/skipped`、`memory/write`）根本没读到它（明文进盘）。
    /// 开关跟门放在一起，调用方只能透过 `set_redact` 改，改不到第二份状态上。
    pub fn set_redact(&mut self, on: bool) {
        self.redact = on;
    }

    pub fn redacting(&self) -> bool {
        self.redact
    }

    /// 已经完成过多少轮（不受内存上限影响）。
    pub fn turn_count(&self) -> u64 {
        self.turns
    }

    /// 当前已写入的事件总数（= 最后一条的 seq）。
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// 有多少条历史事件为了省内存被挤出缓冲（日志文件里仍然完整）。
    pub fn dropped_events(&self) -> u64 {
        self.dropped
    }

    /// 当前进程的日志写者标识。
    ///
    /// 调用 ID 必须跨进程、跨轮次唯一；只拿 `seq` 不够，因为恢复或并发的
    /// 另一份日志也可能从同一起点编号。
    pub fn writer(&self) -> &str {
        &self.writer
    }

    /// 把别的 writer 在我们上次读取之后追加的内容补进来，并接上 seq/prev。
    ///
    /// 调用者必须已经持有写锁：这里假定「我看完之后到写之前」没有人再写。
    fn catch_up(&mut self) -> io::Result<()> {
        // 只读「上次读到的位置之后」的新内容。
        // 之前是 `fs::read(整个文件)`：在 15 MB 的日志上，每 append 一次就把
        // 15 MB 读进内存（实测开一轮 18 MB RSS / 159 ms）。日志越大越慢，
        // 而 append 是最热的路径。
        let len = fs::metadata(&self.path)?.len();
        if len <= self.offset {
            return Ok(());
        }
        let mut tail: Vec<u8> = Vec::with_capacity((len - self.offset) as usize);
        {
            let mut f = File::open(&self.path)?;
            f.seek(SeekFrom::Start(self.offset))?;
            f.read_to_end(&mut tail)?;
        }
        let tail = &tail[..];
        let mut consumed = 0usize;
        for line in tail.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                // 半条记录绝对不能再往后追加：那会把两个 JSON 粘成一行，
                // 下一次 open 只能把整段当尾部损坏截掉。让调用方重开日志，
                // 由 open 在锁内做“保留合法无换行记录 / 截断坏尾”的修复。
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "日志尾部有未完成记录；不要继续追加，先重新打开日志修复",
                ));
            }
            let text = match std::str::from_utf8(line) {
                Ok(t) => t,
                Err(_) => break,
            };
            let trimmed = text.trim();
            consumed += line.len();
            if trimmed.is_empty() {
                continue;
            }
            let event = match json::parse(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("并发写入的日志行不是 JSON: {}", e),
                    ))
                }
            };
            let seq = event.u64_at("seq").unwrap_or(0);
            let prev = event.str_at("prev").unwrap_or("");
            if seq != self.seq + 1 || prev != self.prev {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "日志链在另一个进程里断了：期望 seq {} prev {}，实际 seq {} prev {}",
                        self.seq + 1,
                        &self.prev[..8.min(self.prev.len())],
                        seq,
                        &prev[..8.min(prev.len())]
                    ),
                ));
            }
            self.seq = seq;
            self.prev = event.str_at("hash").unwrap_or(GENESIS).to_string();
            self.events.push(event);
        }
        self.offset += consumed as u64;
        Ok(())
    }

    /// 落账那道门：**默认**把 payload 里任何字符串的凭据形状换掉，再写盘。
    ///
    /// 为什么门在这里而不是在调用方：`Agent::log_event` 那道门第一版只覆盖
    /// agent 自己，结果 `ui-cli/src/server.rs` 的 `turn/skipped`（正文是**人自己
    /// 打的字**）和 `memory/write` 各走各的 `journal.append`，实测明文进盘。
    /// 靠"记得走门"已经错过两次，所以把它挪到唯一会写这个文件的地方：
    /// 新增落账点默认就带脱敏，绕过它必须写成 `append_raw`，一眼看得出是故意的。
    ///
    /// `redacted: N` 由这里盖上去：N>0 说明这一条被动过手，原文得回工作区找，
    /// 别在日志里找。
    pub fn append(&mut self, kind: &str, payload: Json) -> io::Result<Json> {
        let mut payload = payload;
        if self.redact {
            let hits = crate::secrets::redact_json(&mut payload);
            if hits > 0 {
                payload.set("redacted", Json::from(hits));
            }
        }
        self.append_raw(kind, payload)
    }

    /// 不脱敏的那一个：只给两处例外用，调用方要能说清为什么。
    ///
    /// 现在只有 `agent::journal_tool_output` 走这里——它自己按"模式脱敏 +
    /// 来源扣留"两档处理过正文，再过一遍会把 `redacted` 计数的含义洗掉。
    pub fn append_raw(&mut self, kind: &str, payload: Json) -> io::Result<Json> {
        let lock_path = self.path.with_extension("lock");
        let _lock = FileLock::acquire(&lock_path).map_err(|e| journal_ctx(e, &self.path, "拿不到日志锁"))?;
        self.catch_up().map_err(|e| journal_ctx(e, &self.path, "补读日志失败"))?;

        let mut event = Json::obj(vec![
            ("seq", Json::from(self.seq + 1)),
            ("ts", Json::from(time::now_ms())),
            ("kind", Json::from(kind)),
        ]);
        if let (Json::Obj(extra), Json::Obj(items)) = (&payload, &mut event) {
            for (k, v) in extra {
                // 这些字段属于 envelope；payload 里的同名键会把 seq/hash 挪到
                // 行中间，让按字节切 hash 的校验器误判整条日志损坏。
                if matches!(k.as_str(), "seq" | "ts" | "kind" | "w" | "prev" | "hash") {
                    continue;
                }
                items.push((k.clone(), v.clone()));
            }
        }
        // writer 标识：多个进程共享同一份日志时，光看 turn 序号分不清是谁的轮次
        event.set("w", Json::from(self.writer.clone()));
        event.set("prev", Json::from(self.prev.clone()));
        let hash = sha256::hex(event.to_compact().as_bytes());
        event.set("hash", Json::from(hash.clone()));

        // 记录和结尾换行一次写出：分两次写会给崩溃留出「整条记录但没换行」
        // 的窗口，下一次 append 就会把两条记录粘成一行。
        let mut line = event.to_compact().into_bytes();
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| journal_ctx(e, &self.path, "打不开日志"))?;
        file.write_all(&line)
            .map_err(|e| journal_ctx(e, &self.path, "写日志失败"))?;
        file.sync_all()
            .map_err(|e| journal_ctx(e, &self.path, "日志落盘失败"))?;

        self.seq += 1;
        self.prev = hash;
        self.offset += line.len() as u64;
        if kind == "turn/start" {
            self.turns += 1;
        }
        self.events.push(event.clone());
        if self.events.len() > MAX_KEPT_EVENTS {
            let excess = self.events.len() - MAX_KEPT_EVENTS;
            self.events.drain(0..excess);
            self.dropped += excess as u64;
        }
        Ok(event)
    }

    /// Read-only view of a journal, verifying every link in the chain.
    ///
    /// 判定必须与 `stream_verify` 完全一致：后者按原始字节校验 hash，
    /// 这里如果重新序列化再算 hash，就会出现“verify 说坏、read 说好”
    /// （或者反过来）的两套事实。这里先走同一个流式 verifier，再只解析
    /// 它确认过的那段字节。
    pub fn read(path: impl AsRef<Path>) -> io::Result<(Vec<Json>, Option<Corrupt>)> {
        let path = path.as_ref();
        let report = crate::stream_verify::verify_stream(path)?;
        let data = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
            Err(e) => return Err(e),
        };
        let end = report.good_bytes.min(data.len());
        let mut events: Vec<Json> = Vec::new();
        for line in data[..end].split(|b| *b == b'\n') {
            if line_is_blank(line) {
                continue;
            }
            if let Ok(text) = std::str::from_utf8(line) {
                if let Ok(event) = json::parse(text.trim()) {
                    events.push(event);
                }
            }
        }
        Ok((events, report.corrupt))
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Vec<Json>> {
        let (events, corrupt) = Journal::read(path)?;
        match corrupt {
            None => Ok(events),
            Some(c) if c.torn => Ok(events),
            Some(c) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("journal corrupt at seq {:?}: {}", c.seq, c.reason),
            )),
        }
    }
}

fn line_is_blank(line: &[u8]) -> bool {
    match std::str::from_utf8(line) {
        Ok(s) => s.trim().is_empty(),
        Err(_) => false,
    }
}

/// 崩溃可能落在 `write_all(line)` 与 `write_all(b"\n")` 之间：记录本身完整、
/// 可校验，但文件没有结尾换行。保留这条记录，但先把它封口；否则下一次 append
/// 会把两个 JSON 对象粘成一行，再下一次 open 会把它们一起当尾部撕裂截掉。
fn ensure_trailing_newline(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    drop(file);
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Work that was started but never finished: the resume surface.
#[derive(Debug, Clone, Default)]
pub struct Incomplete {
    pub open_turn: Option<Json>,
    pub open_tools: Vec<Json>,
    pub last_snapshot: Option<Json>,
}

pub fn analyze_incomplete(events: &[Json]) -> Incomplete {
    use std::collections::{HashMap, HashSet};

    let mut turns: Vec<Json> = Vec::new();
    // 按「谁的第几轮」记账：多进程共享日志时，turn 数量会交织，
    // 只按顺序配对会把 A 的结束算到 B 头上，然后永远报「有未完成 turn」。
    let mut open: HashMap<(String, u64), Json> = HashMap::new();
    let mut started: HashMap<String, Json> = HashMap::new();
    let mut finished: HashSet<String> = HashSet::new();
    let mut last_snapshot = None;

    for e in events {
        let writer = e.str_at("w").unwrap_or("").to_string();
        match e.str_at("kind").unwrap_or("") {
            "turn/start" => {
                turns.push(e.clone());
                open.insert((writer.clone(), e.u64_at("turn").unwrap_or(0)), e.clone());
            }
            "turn/end" => {
                let turn_no = e.u64_at("turn").unwrap_or(0);
                if open.remove(&(writer.clone(), turn_no)).is_none() {
                    // resume/旧版本可能用新的 writer id 写 turn/end；按 turn
                    // 编号兜底关闭，否则 doctor 会永远报“有未完成 turn”。
                    if let Some(key) = open
                        .keys()
                        .find(|(_, t)| *t == turn_no)
                        .cloned()
                    {
                        open.remove(&key);
                    }
                }
            }
            "prompt/snapshot" => last_snapshot = Some(e.clone()),
            "tool/start" => {
                if let Some(id) = e.str_at("callId") {
                    started.insert(id.to_string(), e.clone());
                }
            }
            "tool/end" => {
                if let Some(id) = e.str_at("callId") {
                    finished.insert(id.to_string());
                }
            }
            _ => {}
        }
    }

    let mut open_turn: Vec<Json> = open.into_values().collect();
    open_turn.sort_by_key(|e| e.u64_at("seq").unwrap_or(0));
    let open_turn = open_turn.pop();

    let mut open_tools: Vec<Json> = started
        .into_iter()
        .filter(|(id, _)| !finished.contains(id))
        .map(|(_, v)| v)
        .collect();
    open_tools.sort_by_key(|e| e.u64_at("seq").unwrap_or(0));

    Incomplete {
        open_turn,
        open_tools,
        last_snapshot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("journal.jsonl")
    }

    #[test]
    fn the_door_is_the_writer_itself_not_the_caller() {
        // 回归：这道门原来挂在 `Agent::log_event` 上，于是 agent 之外的落账点
        // ——`ui-cli/src/server.rs` 的 `turn/skipped`（正文是人自己打的那句话）、
        // `memory/write`、`apply/*`——各写各的 `journal.append`。
        // 实跑复现过一次：排队时撤掉的消息把 AKIA… 原样写进了 journal.jsonl。
        // 靠"调用方记得走门"已经错过两次，现在门就是写这个文件的那个函数。
        let path = tmp("door");
        let mut j = Journal::open(&path).unwrap();
        // 开关钉死，不让别人机器上的 UI_REDACT 改变这条测试的结论
        j.set_redact(true);
        j.append(
            "turn/skipped",
            Json::obj(vec![
                ("reason", Json::from("cancelled-while-queued")),
                ("text", Json::from("部署要用 AKIAIOSFODNN7EXAMPLE")), // 测试夹具：密钥形状，非真凭据（allowlist secret）
            ]),
        )
        .unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("AKIAIOSFODNN7EXAMPLE"), // 测试夹具：密钥形状，非真凭据（allowlist secret）
            "默认这一档：盘上不能出现原文\n{}",
            on_disk
        );
        assert!(on_disk.contains("[REDACTED:aws key]"), "要留下抹除的痕迹");
        assert!(on_disk.contains("\"redacted\":1"), "还要留下抹了几处");

        // 绕过门必须写成 `append_raw`：diff 里一眼就看得出这是故意的
        j.append_raw(
            "memory/write",
            Json::obj(vec![("text", Json::from("ghp_CCCCDEEEFFFGGGHHHIII"))]),
        )
        .unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("ghp_CCCCDEEEFFFGGGHHHIII"), "raw 就是 raw");

        // 关掉门（`--no-redact` / `UI_REDACT=0`）：走的是同一个 `append`，明文
        j.set_redact(false);
        j.append(
            "turn/skipped",
            Json::obj(vec![("text", Json::from("再来一次 AKIAIOSFODNN7EXAMPLE"))]), // 测试夹具：密钥形状，非真凭据（allowlist secret）
        )
        .unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("AKIAIOSFODNN7EXAMPLE"), "关掉了就该是明文"); // 测试夹具：密钥形状，非真凭据（allowlist secret）
        assert!(!on_disk.contains("\"redacted\":2"), "关掉时不该盖计数");

        // 门改的是写出去之前的那一份 payload：链必须仍然自洽
        let (events, corrupt) = Journal::read(&path).unwrap();
        assert!(corrupt.is_none(), "过门之后哈希链不能断: {:?}", corrupt);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn appends_and_verifies_chain() {
        let path = tmp("chain");
        let mut j = Journal::open(&path).unwrap();
        j.append("turn/start", Json::obj(vec![("turn", Json::from(1u64))]))
            .unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("你好"))]))
            .unwrap();
        let events = Journal::load(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].str_at("kind"), Some("note"));
    }

    #[test]
    fn detects_tampering_in_the_middle() {
        let path = tmp("tamper");
        let mut j = Journal::open(&path).unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("a"))]))
            .unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("b"))]))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"a\"", "\"c\"", 1);
        fs::write(&path, tampered).unwrap();
        let (events, corrupt) = Journal::read(&path).unwrap();
        assert_eq!(events.len(), 0);
        let c = corrupt.expect("tampered journal must not verify");
        assert!(!c.torn, "damage in the middle is not a repairable tear");
    }

    #[test]
    fn repairs_a_torn_tail_and_says_so() {
        use std::io::Write as _;
        let path = tmp("torn");
        let mut j = Journal::open(&path).unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("a"))]))
            .unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("b"))]))
            .unwrap();

        // A crash landing in the middle of a write: half a record, no newline.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"seq\":3,\"kind\":\"note\",\"prev\":\"").unwrap();
        file.sync_all().unwrap();

        let (events, corrupt) = Journal::read(&path).unwrap();
        assert_eq!(events.len(), 2, "the verified prefix survives");
        assert!(corrupt.expect("torn tail must be reported").torn);

        let repaired = Journal::open(&path).unwrap();
        // open 现在只保留最后一条记录（内存与日志大小无关），修复动作自己会追加一条
        assert_eq!(repaired.events.len(), 2, "最后一条记录 + 修复说明");
        assert_eq!(repaired.events[1].str_at("kind"), Some("journal/torn"));
        assert_eq!(repaired.seq(), 3, "seq 必须接上（2 条有效 + 1 条说明）");
        assert!(
            Journal::load(&path).is_ok(),
            "the chain verifies again after repair"
        );
    }

    #[test]
    fn finds_unfinished_tool_calls() {
        let path = tmp("incomplete");
        let mut j = Journal::open(&path).unwrap();
        j.append("turn/start", Json::obj(vec![("turn", Json::from(1u64))]))
            .unwrap();
        j.append(
            "tool/start",
            Json::obj(vec![
                ("callId", Json::from("c1")),
                ("tool", Json::from("fs_write")),
            ]),
        )
        .unwrap();
        let events = Journal::load(&path).unwrap();
        let inc = analyze_incomplete(&events);
        assert_eq!(inc.open_tools.len(), 1);
        assert_eq!(inc.open_tools[0].str_at("callId"), Some("c1"));
        assert!(inc.open_turn.is_some());
    }
}

#[test]
fn two_writers_share_one_journal_without_breaking_the_chain() {
    // 并行子代理共享同一 workspace 的前提：多个进程各持一个 Journal，追加时
    // 必须先补读别人的记录、接上 seq/prev，否则链会断、日志直接不可用。
    let dir = std::env::temp_dir().join(format!("ui-journal-mw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.jsonl");

    let writers = 4;
    let per_writer = 25;
    let mut handles = Vec::new();
    for w in 0..writers {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let mut j = Journal::open(&path).unwrap();
            for i in 0..per_writer {
                j.append(
                    "note",
                    Json::obj(vec![
                        ("writer", Json::from(w)),
                        ("i", Json::from(i)),
                    ]),
                )
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let (events, corrupt) = Journal::read(&path).unwrap();
    assert!(corrupt.is_none(), "并发追加后不该出现损坏: {:?}", corrupt);
    assert_eq!(events.len(), writers * per_writer, "一条都不能丢");
    for (i, ev) in events.iter().enumerate() {
        assert_eq!(
            ev.u64_at("seq"),
            Some(i as u64 + 1),
            "seq 必须连续，第 {} 条断了",
            i + 1
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_offset_always_matches_the_records_it_parsed() {
    // 回归：open() 曾经在**锁外**取 offset。如果读完到取长度之间别的写者追加了一条，
    // offset 就会跳过那条没解析过的记录，下一次 append 的 catch_up 看到 seq 对不上，
    // 报「日志链断了」。CI 四线程并发时复现过。
    //
    // 断言的不变量（open 改成只扫尾部之后）：
    //   1. offset 必须落在行边界上、且不超过文件长度；
    //   2. seq 与磁盘上的记录数一致（不能少算，否则 append 会写出重复 seq）；
    //   3. 最关键的一条：**open 之后立刻 append 必须成功**——旧 bug 的表现就是这里报「链断了」。
    let dir = std::env::temp_dir().join(format!("ui-journal-toctou-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.jsonl");
    Journal::open(&path).unwrap().append("seed", Json::obj(vec![])).unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let path = path.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let mut j = Journal::open(&path).unwrap();
                let _ = j.append("noise", Json::obj(vec![]));
                std::thread::sleep(std::time::Duration::from_micros(20));
            }
        })
    };

    for round in 0..60 {
        let mut j = Journal::open(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        let _complete: u64 = data.split_inclusive(|b| *b == b'\n')
            .filter(|l| l.ends_with(b"\n"))
            .map(|l| l.len() as u64)
            .sum();

        assert!(
            j.offset <= data.len() as u64,
            "第 {} 轮：offset {} 超过文件长度 {}",
            round, j.offset, data.len()
        );
        // offset 必须在行边界（要么 0，要么前一个字节是换行）
        assert!(
            j.offset == 0 || data[(j.offset - 1) as usize] == b'\n',
            "第 {} 轮：offset {} 不在行边界上",
            round, j.offset
        );
        assert!(
            j.seq <= complete_count(&data),
            "第 {} 轮：seq {} 超过了磁盘上的记录数",
            round, j.seq
        );

        // 真正的回归点：拿到的 seq/prev 必须能直接接上
        j.append("probe", Json::obj(vec![])).unwrap_or_else(|e| {
            panic!("第 {} 轮：open 之后 append 失败（open 跳过了记录？）：{}", round, e)
        });
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(test)]
fn complete_count(data: &[u8]) -> u64 {
    data.split_inclusive(|b| *b == b'\n')
        .filter(|l| l.ends_with(b"\n"))
        .count() as u64
}

#[test]
fn memory_buffer_is_capped_but_disk_keeps_everything() {
    // 浸泡测试发现的问题：Journal 把所有事件留在内存里，1200 步后 RSS 从 6MB 涨到 32MB。
    // 上限之后：内存有顶、轮次计数仍然准确、磁盘上的日志一条不少。
    let dir = std::env::temp_dir().join(format!("ui-journal-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.jsonl");

    let total = MAX_KEPT_EVENTS + 500;
    let turns: u64 = 7;
    let mut j = Journal::open(&path).unwrap();
    for i in 0..total {
        let kind = if (i as u64) < turns { "turn/start" } else { "note" };
        j.append(kind, Json::obj(vec![("i", Json::from(i as u64))])).unwrap();
    }

    assert!(
        j.events.len() <= MAX_KEPT_EVENTS,
        "内存里最多保留 {} 条，实际 {}",
        MAX_KEPT_EVENTS,
        j.events.len()
    );
    assert_eq!(j.dropped_events(), (total - MAX_KEPT_EVENTS) as u64);
    assert_eq!(j.turn_count(), turns, "轮次计数不能被挤出影响");

    // 磁盘完整性不受缓冲上限影响
    let (events, corrupt) = Journal::read(&path).unwrap();
    assert!(corrupt.is_none(), "链必须完整: {:?}", corrupt);
    assert_eq!(events.len(), total, "磁盘上一条都不能少");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_writer_sees_events_appended_by_the_first() {
    let dir = std::env::temp_dir().join(format!("ui-journal-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.jsonl");

    let mut a = Journal::open(&path).unwrap();
    let mut b = Journal::open(&path).unwrap();
    a.append("note", Json::obj(vec![("who", Json::from("a"))])).unwrap();
    // b 打开时只看到 0 条；append 时应当先补读 a 的那条
    assert_eq!(b.events.len(), 0);
    b.append("note", Json::obj(vec![("who", Json::from("b"))])).unwrap();
    assert_eq!(b.events.len(), 2, "b 必须先把 a 的记录补进来");
    assert_eq!(b.events[0].str_at("who"), Some("a"));

    let (events, corrupt) = Journal::read(&path).unwrap();
    assert!(corrupt.is_none());
    assert_eq!(events.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn opening_an_existing_journal_never_truncates_it() {
    // 老实现是 `if !exists { write("") }`：并发打开时后者会把前者刚写的记录清掉。
    let dir = std::env::temp_dir().join(format!("ui-journal-trunc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.jsonl");

    let mut a = Journal::open(&path).unwrap();
    a.append("note", Json::obj(vec![("who", Json::from("a"))])).unwrap();

    // 再打开一次：必须能看见已有的那条，且不能把文件清空
    let b = Journal::open(&path).unwrap();
    assert_eq!(b.events.len(), 1, "open 不能截断已有日志");
    let (events, corrupt) = Journal::read(&path).unwrap();
    assert!(corrupt.is_none());
    assert_eq!(events.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
