//! Contract 5: one side effect, one durable log entry.
//!
//! Append-only JSONL, one fsync per event, and a hash chain over the whole file.
//! Events are written *before* the action they describe, so a crash leaves a
//! readable prefix and a specific question ("this call was started, never
//! finished — re-run it or mark it unknown?"), not an apology.

use crate::json::{self, Json};
use crate::sha256;
use crate::time;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
    pub events: Vec<Json>,
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


impl Journal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // 原子创建：绝不能写成 `if !path.exists() { fs::write(path, b"") }`——
        // 两个进程同时打开时，「判断」和「写空串」之间会被另一个进程插进来，
        // 后者会把前者刚写好的日志截断（真实踩过：三进程并发丢了两条记录）。
        OpenOptions::new().create(true).append(true).open(&path)?;

        // 读和修复都必须在锁里：别的 writer 正在写时，读到的是半条记录，
        // 会被当成「尾部撕裂」截掉——那等于把别人刚写的记录毁了。
        //
        // 长度也必须在**同一把锁**里取：如果出了锁再 metadata().len()，
        // 中间可能有别的写者追加，offset 就会跳过一条没解析过的记录，
        // 下一次 catch_up 看到 seq 对不上，直接报「日志链断了」。
        // 这不是理论问题——CI 上四线程并发时真的炸过。
        let (events, corrupt, offset) = {
            let lock_path = path.with_extension("lock");
            let _lock = FileLock::acquire(&lock_path)?;
            let (events, corrupt) = Journal::read(&path)?;
            if let Some(c) = &corrupt {
                if c.torn {
                    // A crash can leave one half-written record. Keep the verified
                    // prefix, drop the tear, and say so in the log itself.
                    let file = fs::OpenOptions::new().write(true).open(&path)?;
                    file.set_len(c.good_bytes as u64)?;
                    file.sync_all()?;
                }
            }
            // 崩溃可能落在 write_all(line) 与 write_all("\n") 之间：记录本身完整、
            // 可校验，但文件没有结尾换行。此时直接 append 会把两个 JSON 对象粘成
            // 一行，而**下一次** open 会把它们一起当成尾部撕裂截掉。先补上换行。
            ensure_trailing_newline(&path)?;
            let offset = fs::metadata(&path)?.len();
            (events, corrupt, offset)
        };
        let mut torn_note = None;
        if let Some(c) = corrupt {
            if c.torn {
                torn_note = Some(c.reason);
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
        let (seq, prev) = match events.last() {
            Some(last) => (
                last.u64_at("seq").unwrap_or(0),
                last.str_at("hash").unwrap_or(GENESIS).to_string(),
            ),
            None => (0, GENESIS.to_string()),
        };
        let mut journal = Journal {
            path,
            seq,
            prev,
            offset,
            writer: writer_id(),
            events,
        };
        if let Some(reason) = torn_note {
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

    /// 把别的 writer 在我们上次读取之后追加的内容补进来，并接上 seq/prev。
    ///
    /// 调用者必须已经持有写锁：这里假定「我看完之后到写之前」没有人再写。
    fn catch_up(&mut self) -> io::Result<()> {
        let data = fs::read(&self.path)?;
        if data.len() as u64 <= self.offset {
            return Ok(());
        }
        let tail = &data[self.offset as usize..];
        let mut consumed = 0usize;
        for line in tail.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                break; // 半条记录：不动，等它写完
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

    /// Append one event, durably, before the caller acts on it.
    ///
    /// 多个进程可以共享同一份日志：写入前先拿独占锁，再把「上次读到的位置之后」
    /// 的内容补读进来、对齐 seq/prev，然后才写自己这条。否则两个 writer 各自
    /// 拿着旧 seq，链会断。
    pub fn append(&mut self, kind: &str, payload: Json) -> io::Result<Json> {
        let lock_path = self.path.with_extension("lock");
        let _lock = FileLock::acquire(&lock_path)?;
        self.catch_up()?;

        let mut event = Json::obj(vec![
            ("seq", Json::from(self.seq + 1)),
            ("ts", Json::from(time::now_ms())),
            ("kind", Json::from(kind)),
        ]);
        if let (Json::Obj(extra), Json::Obj(items)) = (&payload, &mut event) {
            for (k, v) in extra {
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
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(&line)?;
        file.sync_all()?;

        self.seq += 1;
        self.prev = hash;
        self.offset += line.len() as u64;
        self.events.push(event.clone());
        Ok(event)
    }

    /// Read-only view of a journal, verifying every link in the chain.
    pub fn read(path: impl AsRef<Path>) -> io::Result<(Vec<Json>, Option<Corrupt>)> {
        // 按字节读：崩溃可能把一个多字节 UTF-8 字符撕成两半，`read_to_string`
        // 会让整份日志打不开——尾部撕裂的修复路径直接失效。
        let data = match fs::read(path.as_ref()) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
            Err(e) => return Err(e),
        };
        let mut events: Vec<Json> = Vec::new();
        let mut prev = GENESIS.to_string();
        // split 保留最后一段：文件以 `\n` 结尾时它是空段。
        let raw: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
        let mut offset = 0usize;
        for (idx, line) in raw.iter().enumerate() {
            let line_start = offset;
            offset += line.len();
            let terminated = idx + 1 < raw.len();
            if terminated {
                offset += 1;
            }
            if line_is_blank(line) {
                // 空行不算记录，但属于已验证前缀，截断时要保留。
                continue;
            }
            let is_last = raw[idx + 1..].iter().all(|l| line_is_blank(l));
            let fail = |seq: Option<u64>, reason: String| Corrupt {
                seq,
                reason,
                torn: is_last,
                good_bytes: line_start,
            };

            let text = match std::str::from_utf8(line) {
                Ok(t) => t,
                Err(e) => {
                    return Ok((
                        events,
                        Some(fail(None, format!("invalid utf-8 in line: {}", e))),
                    ))
                }
            };
            let record = match json::parse(text) {
                Ok(v) => v,
                Err(e) => {
                    return Ok((events, Some(fail(None, format!("unparsable line: {}", e)))))
                }
            };
            let recorded_prev = record.str_at("prev").unwrap_or("").to_string();
            if recorded_prev != prev {
                return Ok((
                    events,
                    Some(fail(record.u64_at("seq"), "broken hash chain".to_string())),
                ));
            }
            let declared = record.str_at("hash").unwrap_or("").to_string();
            let mut without_hash = record.clone();
            if let Json::Obj(items) = &mut without_hash {
                items.retain(|(k, _)| k != "hash");
            }
            if sha256::hex(without_hash.to_compact().as_bytes()) != declared {
                return Ok((
                    events,
                    Some(fail(record.u64_at("seq"), "hash mismatch".to_string())),
                ));
            }
            prev = declared;
            events.push(record);
        }
        Ok((events, None))
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
                open.remove(&(writer, e.u64_at("turn").unwrap_or(0)));
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
        assert_eq!(repaired.events.len(), 3);
        assert_eq!(repaired.events[2].str_at("kind"), Some("journal/torn"));
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
    // 直接断言不变量：**offset 必须正好等于它解析出的那些记录的字节数**。
    // 这样不用去赌「seq 是否刚好断」，只要时序撞上就会红。
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

    for round in 0..400 {
        let j = Journal::open(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        // events 覆盖的前缀字节数（按行累加）
        let covered: u64 = data
            .split_inclusive(|b| *b == b'\n')
            .take(j.events.len())
            .map(|l| l.len() as u64)
            .sum();
        assert_eq!(
            j.offset, covered,
            "第 {} 轮：offset {} 与解析出的 {} 条记录（{} 字节）不一致——open 跳过了记录",
            round,
            j.offset,
            j.events.len(),
            covered
        );
        drop(j);
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
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
