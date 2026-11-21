//! Journal 的随机/边界截断：不能在任意半条记录上 panic，也不能把
//! 合法前缀吃掉。真正的 crash 修复逻辑在 `Journal::open`，这里守它的输入边界。

use std::fs;
use std::path::PathBuf;

use ui_core::json::Json;
use ui_core::journal::Journal;
use ui_core::stream_verify::verify_stream;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ui-journal-fuzz-{}-{}",
        std::process::id(),
        name
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build_journal(path: &std::path::Path, count: usize) {
    let mut journal = Journal::open(path).unwrap();
    for i in 0..count {
        journal
            .append(
                "note",
                Json::obj(vec![
                    ("text", Json::from(format!("记录 {} with unicode 你好", i))),
                    ("i", Json::from(i as u64)),
                ]),
            )
            .unwrap();
    }
}

#[test]
fn truncating_anywhere_is_fail_closed_or_repairable_but_never_panics() {
    let dir = scratch("truncate");
    let full = dir.join("full.jsonl");
    build_journal(&full, 24);
    let bytes = fs::read(&full).unwrap();
    assert!(bytes.len() > 100);

    // 每个字节都试太重；按 7 字节采样，覆盖换行前后、UTF-8 中间和 hash 中间。
    for cut in (0..bytes.len()).step_by(7) {
        let path = dir.join(format!("cut-{}.jsonl", cut));
        fs::write(&path, &bytes[..cut]).unwrap();

        let open = std::panic::catch_unwind(|| {
            let _ = Journal::open(&path);
        });
        assert!(open.is_ok(), "Journal::open panic at cut {}", cut);

        let verified = std::panic::catch_unwind(|| {
            let _ = verify_stream(&path);
        });
        assert!(verified.is_ok(), "verify_stream panic at cut {}", cut);

        // open 之后要么是可读链，要么明确报错；不能静默返回半个事件。
        if let Ok((events, _)) = Journal::read(&path) {
            for event in events {
                assert!(
                    event.u64_at("seq").is_some(),
                    "parsed event without seq at cut {}",
                    cut
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}
