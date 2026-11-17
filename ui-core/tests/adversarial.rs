//! 对抗性回归测试。
//!
//! 每个测试都对应一个具体攻击输入：在修复前它会失败（拒绝、崩溃或静默丢数据），
//! 修复后它必须稳定通过。测试只断言可观察行为，不依赖内部实现细节。

use std::fs;
use std::path::{Path, PathBuf};

use ui_core::budget::{Budget, Dim, Limits};
use ui_core::compact::{compact, CompactPolicy};
use ui_core::journal::{analyze_incomplete, Journal};
use ui_core::json::{self, Json};
use ui_core::model::{Model, ScriptedModel};
use ui_core::policy::{
    glob_match, Decision, Effect, Policy, Rule, FS_READ, FS_WRITE, PROC_EXEC,
};
use ui_core::tools::{find, registry, Ctx, Matcher};
use ui_core::{Agent, Message, NullHooks};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-adv-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn scripted(steps: Vec<Json>) -> Box<dyn Model + Send> {
    Box::new(
        ScriptedModel::from_json(&Json::obj(vec![
            ("model", Json::from("scripted")),
            ("steps", Json::Arr(steps)),
        ]))
        .unwrap(),
    )
}

fn call(name: &str, args: Vec<(&str, Json)>) -> Json {
    Json::obj(vec![
        ("name", Json::from(name)),
        ("args", Json::obj(args)),
    ])
}

// ---------------------------------------------------------------------------
// 1. 路径穿越：相对路径里的 `..` 不能离开 workspace
// ---------------------------------------------------------------------------

#[test]
fn dotdot_paths_do_not_escape_the_workspace() {
    let root = scratch("traversal");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(root.join("outside.txt"), "TOP-SECRET-CONTENT").unwrap();

    let ctx = Ctx {
        workspace: ws.clone(),
        ..Default::default()
    };
    let policy = Policy::new(Policy::defaults(&ws), &ws);

    // 攻击输入：相对 workspace 的 `..`。解析后的目标仍然是 `<ws>/../outside.txt`，
    // 而 `<ws>/**` 里的 `**` 跨目录，于是被当成"工作区内"放行。
    for path in [
        "../outside.txt",
        "sub/../../outside.txt",
        "./../outside.txt",
    ] {
        let args = Json::obj(vec![("path", Json::from(path))]);
        let effects = (find(&registry(), "fs_read").unwrap().effects)(&args, &ctx);
        assert_eq!(effects.len(), 1);
        assert_eq!(
            policy.decide(&effects).decision,
            Decision::Deny,
            "`{}` 解析成 {}，逃出了 {} 却被放行",
            path,
            effects[0].target,
            ws.display()
        );
    }

    // 工作区内部带 `..` 的路径仍然必须可用（规范化后仍在里面）。
    let inside_args = Json::obj(vec![("path", Json::from("sub/../inside.txt"))]);
    let inside_effects = (find(&registry(), "fs_read").unwrap().effects)(&inside_args, &ctx);
    assert_eq!(inside_effects[0].target, ws.join("inside.txt").to_string_lossy());
    assert_eq!(policy.decide(&inside_effects).decision, Decision::Allow);

    // 绝对路径在外面本来就被拒绝；这条不能因为修复而回退。
    let abs = Json::obj(vec![("path", Json::from("/etc/passwd"))]);
    let abs_effects = (find(&registry(), "fs_read").unwrap().effects)(&abs, &ctx);
    assert_eq!(policy.decide(&abs_effects).decision, Decision::Deny);
}

#[test]
fn agent_refuses_dotdot_read_and_write_end_to_end() {
    let root = scratch("traversal-agent");
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(root.join("outside.txt"), "TOP-SECRET-CONTENT").unwrap();

    let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
    // 先试读工作区外，再试写工作区外，然后结束。
    let mut agent = Agent::new(
        journal,
        scripted(vec![
            Json::obj(vec![
                ("content", Json::from("先读外面")),
                (
                    "toolCalls",
                    Json::Arr(vec![call(
                        "fs_read",
                        vec![("path", Json::from("../outside.txt"))],
                    )]),
                ),
            ]),
            Json::obj(vec![
                ("content", Json::from("再写外面")),
                (
                    "toolCalls",
                    Json::Arr(vec![call(
                        "fs_write",
                        vec![
                            ("path", Json::from("../pwned.txt")),
                            ("content", Json::from("pwned")),
                        ],
                    )]),
                ),
            ]),
            Json::obj(vec![("content", Json::from("结束"))]),
        ]),
        ws.clone(),
        Limits::default(),
    );

    let report = agent.run_turn("越界", &mut NullHooks).unwrap();
    assert_eq!(report.reason, "done");

    let tool_text = agent
        .history
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_text.contains("策略拒绝") || tool_text.contains("DENIED"),
        "越界调用必须被拒绝，实际工具结果：{}",
        tool_text
    );
    assert!(
        !tool_text.contains("TOP-SECRET-CONTENT"),
        "工作区外的内容泄漏进了模型上下文：{}",
        tool_text
    );
    assert!(
        !root.join("pwned.txt").exists(),
        "fs_write 的 `..` 在工作区外写入了文件"
    );
    assert!(
        !ws.join("pwned.txt").exists(),
        "`../pwned.txt` 不应落进 workspace"
    );

    // 两次越界都必须在日志里留下 deny 决策。
    let denies = agent
        .journal
        .events
        .iter()
        .filter(|e| e.str_at("kind") == Some("tool/request"))
        .filter(|e| e.str_at("decision") == Some("deny"))
        .count();
    assert_eq!(denies, 2, "每一次越界请求都要有一条 deny 决策");
}

// ---------------------------------------------------------------------------
// 2. journal：尾部撕裂 / 未终止记录 / 非 UTF-8
// ---------------------------------------------------------------------------

#[test]
fn an_unterminated_final_record_is_terminated_before_the_next_append() {
    let dir = scratch("unterminated-tail");
    let path = dir.join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("first"))]))
        .unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("second"))]))
        .unwrap();
    drop(j);

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    // 模拟崩溃发生在 write_all(line) 与 write_all("\n") 之间：
    // 最后一条记录本身完整、可校验，但文件没有结尾换行。
    fs::write(&path, text.trim_end_matches('\n')).unwrap();

    let mut reopened = Journal::open(&path).expect("未终止的最后一条记录不应让日志打不开");
    assert_eq!(
        reopened.events.len(),
        2,
        "已经校验通过的两条记录都必须保留"
    );
    reopened
        .append("note", Json::obj(vec![("text", Json::from("third"))]))
        .unwrap();

    let events = Journal::load(&path).expect("修好结尾后整条哈希链必须仍然可验证");
    assert_eq!(events.len(), 3, "第三条记录不能把第二条一起弄丢：{:?}", events);
    assert_eq!(events[2].str_at("text"), Some("third"));
}

#[test]
fn a_torn_tail_that_splits_a_utf8_character_is_repairable() {
    let dir = scratch("torn-utf8");
    let path = dir.join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("你好"))]))
        .unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("世界"))]))
        .unwrap();
    drop(j);

    // 半条记录，而且正好撕在「中」（E4 B8 AD）的第三个字节之前。
    let mut raw = fs::read(&path).unwrap();
    raw.extend_from_slice(b"{\"seq\":3,\"kind\":\"note\",\"text\":\"");
    raw.extend_from_slice(&[0xE4, 0xB8]);
    fs::write(&path, &raw).unwrap();

    let (events, corrupt) = Journal::read(&path)
        .expect("含半个 UTF-8 字符的撕裂尾部也必须能读出已验证前缀");
    assert_eq!(events.len(), 2);
    assert!(corrupt.expect("必须报告尾部撕裂").torn);

    let repaired = Journal::open(&path).unwrap();
    // open 现在只保留最后一条记录（内存与日志大小无关），修复说明由 append 追加
    assert_eq!(repaired.events.len(), 2, "最后一条记录 + 修复说明");
    assert_eq!(repaired.events[1].str_at("kind"), Some("journal/torn"));
    assert_eq!(repaired.seq(), 3);
    assert!(Journal::load(&path).is_ok(), "修复后哈希链必须重新成立");
}

#[test]
fn blank_lines_between_records_are_ignored_not_corruption() {
    let dir = scratch("blank-lines");
    let path = dir.join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("a"))]))
        .unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("b"))]))
        .unwrap();
    drop(j);

    let text = fs::read_to_string(&path).unwrap();
    // 外部工具在记录之间插入空行（非常常见的 diff/格式化后果）。
    let with_blanks = format!("\n{}\n\n\n{}\n", text.trim_end_matches('\n'), "");
    fs::write(&path, with_blanks).unwrap();

    let events = Journal::load(&path).expect("空行不是损坏");
    assert_eq!(events.len(), 2);
    assert_eq!(analyze_incomplete(&events).open_turn, None);

    let mut j = Journal::open(&path).unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("c"))]))
        .unwrap();
    assert_eq!(Journal::load(&path).unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// 3. json：深嵌套与超范围数字
// ---------------------------------------------------------------------------

#[test]
fn absurd_json_nesting_is_rejected_instead_of_blowing_the_stack() {
    // 200k 层递归在默认栈上会直接段错误；解析器必须有深度上限。
    let deep = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
    assert!(
        json::parse(&deep).is_err(),
        "20 万层嵌套必须被拒绝，而不是把进程栈打爆"
    );

    // 正常深度不能被误伤。
    let ok = format!("{}{}", "[".repeat(100), "]".repeat(100));
    assert!(json::parse(&ok).is_ok(), "100 层嵌套属于正常输入");
}

#[test]
fn out_of_range_json_numbers_are_rejected_not_silently_nulled() {
    // JSON 没有 Infinity；1e400 溢出 f64 后编码器会把它写成 null，破坏往返稳定性。
    assert!(json::parse("1e400").is_err(), "1e400 不能被解析成 inf");
    assert!(json::parse("-1e400").is_err(), "-1e400 不能被解析成 -inf");

    // 有限的大数仍要能往返。
    let v = json::parse("1e308").unwrap();
    assert_eq!(json::parse(&v.to_compact()).unwrap(), v);
}

// ---------------------------------------------------------------------------
// 4. budget：provider usage 溢出 / 溢出把账本清零
// ---------------------------------------------------------------------------

#[test]
fn absurd_provider_usage_cannot_reset_the_token_ledger() {
    let mut b = Budget::new(Limits {
        tokens: 1_000,
        ..Limits::default()
    });
    b.charge(Dim::Tokens, 100.0).unwrap();

    // 恶意/损坏的 provider 回报 u64::MAX：旧实现用 i64 运算，
    // 100 + (-1) - 100 -> 负数 -> 被 max(0) 清零，预算从此永远不再触发。
    b.reconcile_tokens(100, u64::MAX);
    assert!(
        b.used(Dim::Tokens) > 1e19,
        "离谱的 usage 只能把账本推高，不能清零，实际 used={}",
        b.used(Dim::Tokens)
    );
    assert!(
        b.charge(Dim::Tokens, 1.0).is_err(),
        "账本推高后必须仍然超预算"
    );
}

#[test]
fn provider_usage_near_i64_max_does_not_overflow() {
    let mut b = Budget::new(Limits {
        tokens: 1_000,
        ..Limits::default()
    });
    b.charge(Dim::Tokens, 100.0).unwrap();
    // debug 构建里 i64 加法会 panic（attempt to add with overflow）。
    b.reconcile_tokens(100, i64::MAX as u64);
    assert!(b.used(Dim::Tokens) > 1e18);
    assert!(b.charge(Dim::Tokens, 1.0).is_err());
}

#[test]
fn zero_limit_means_no_spending_not_unlimited() {
    // `--max-tokens 0` / `--max-usd -1` 不能被静默当成「无限预算」。
    let mut b = Budget::new(Limits {
        tokens: 0,
        tool_calls: 0,
        wall_ms: 0,
        usd: 0.0,
        max_steps: 24,
    });
    assert!(
        b.charge(Dim::Tokens, 1.0).is_err(),
        "0 token 限额必须是 0，不是无限"
    );
    assert!(
        b.charge(Dim::Usd, 0.01).is_err(),
        "0 美元限额必须是 0，不是无限"
    );

    let mut b = Budget::new(Limits {
        usd: -1.0,
        ..Limits::default()
    });
    assert!(
        b.charge(Dim::Usd, 0.5).is_err(),
        "负限额（可能来自参数解析）不能变成无限"
    );
}

// ---------------------------------------------------------------------------
// 5. compact：keep_recent 的溢出/下溢
// ---------------------------------------------------------------------------

#[test]
fn absurd_keep_recent_does_not_panic_or_mutate_history() {
    let mut history = vec![Message::user("任务锚点")];
    for i in 0..20 {
        history.push(Message::assistant(format!("第 {} 段 {}", i, "x".repeat(200))));
    }
    let before = history.len();

    // `keep_recent + 2` 在 debug 里溢出 panic；release 里 `len - keep_recent`
    // 下溢后切片越界，同样会崩。CLI 的 --keep-recent 是用户输入。
    let out = compact(
        &mut history,
        &CompactPolicy {
            max_history_tokens: 10,
            keep_recent: usize::MAX,
            keep_first_user: true,
        },
    );
    assert!(out.is_none(), "太大的 keep_recent 等价于不折叠");
    assert_eq!(history.len(), before, "拒绝折叠时历史不能被改动");
}

// ---------------------------------------------------------------------------
// 6. tools：fs_read 的窗口参数溢出
// ---------------------------------------------------------------------------

#[test]
fn fs_read_with_huge_limit_does_not_panic() {
    let dir = scratch("read-window");
    fs::write(dir.join("f.txt"), "a\nb\nc\nd\n").unwrap();
    let ctx = Ctx {
        workspace: dir,
        ..Default::default()
    };
    let tools = registry();
    let tool = find(&tools, "fs_read").unwrap();
    let result = (tool.run)(
        &Json::obj(vec![
            ("path", Json::from("f.txt")),
            ("offset", Json::from(2u64)),
            ("limit", Json::from(u64::MAX)),
        ]),
        &ctx,
    );
    assert!(result.ok, "大 limit 不是错误输入，应该正常返回窗口：{:?}", result.error);
    assert!(result.content.contains("b"));
    assert!(result.content.contains("d"));
}

// ---------------------------------------------------------------------------
// 7. tools：fs_search 的 matcher 不能指数回溯
// ---------------------------------------------------------------------------

#[test]
fn search_matcher_does_not_backtrack_exponentially() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // `*a` x24 + `*b` 对 120 个 a：回溯实现的组合数是 C(120,24)，永远算不完。
        let pattern = format!("{}*b", "*a".repeat(24));
        let line = "a".repeat(120);
        let _ = tx.send(Matcher::new(&pattern).is_match(&line));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(false) => {}
        Ok(true) => panic!("病态 pattern 不应匹配"),
        Err(_) => panic!("matcher 2 秒内没跑完：存在指数级回溯（ReDoS）"),
    }
}

#[test]
fn search_matcher_agrees_with_the_backtracking_reference() {
    // 旧实现（回溯）作为语义参照；新实现必须在所有小输入上完全一致。
    fn reference(pattern: &str, text: &str) -> bool {
        fn at(parts: &[char], text: &[char]) -> bool {
            if parts.is_empty() {
                return true;
            }
            match parts[0] {
                '.' => !text.is_empty() && at(&parts[1..], &text[1..]),
                '*' => (0..=text.len()).any(|i| at(&parts[1..], &text[i..])),
                c => !text.is_empty() && text[0] == c && at(&parts[1..], &text[1..]),
            }
        }
        let p: Vec<char> = pattern.chars().collect();
        let t: Vec<char> = text.chars().collect();
        (0..=t.len()).any(|s| at(&p, &t[s..]))
    }

    fn strings(alphabet: &[char], max: usize) -> Vec<String> {
        let mut all = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..max {
            let mut next = Vec::new();
            for s in &frontier {
                for c in alphabet {
                    let mut t = s.clone();
                    t.push(*c);
                    next.push(t);
                }
            }
            all.extend(next.iter().cloned());
            frontier = next;
        }
        all
    }

    for pattern in strings(&['a', 'b', '.', '*'], 4) {
        for text in strings(&['a', 'b'], 4) {
            assert_eq!(
                Matcher::new(&pattern).is_match(&text),
                reference(&pattern, &text),
                "pattern={:?} text={:?}",
                pattern,
                text
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 8. policy：glob 边界（验证已有语义，不是待修复项）
// ---------------------------------------------------------------------------

#[test]
fn glob_stays_inside_the_workspace_boundary() {
    let ws = Path::new("/ws");
    let policy = Policy::new(Policy::defaults(ws), ws);

    // 前缀冲突：`/ws/**` 不能把 /wsx、/ws-other 当工作区。
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/wsx/a.txt")).0,
        Decision::Deny
    );
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/ws-other/a.txt")).0,
        Decision::Deny
    );
    // workspace 根可读（fs_search 的目标），但根本身不可写。
    assert_eq!(policy.explain(&Effect::new(FS_READ, "/ws")).0, Decision::Allow);
    assert_eq!(
        policy.explain(&Effect::new(FS_WRITE, "/ws")).0,
        Decision::Deny
    );
    // 绝对路径与 secrets 目录。
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/etc/passwd")).0,
        Decision::Deny
    );
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/ws/secrets/deep/x")).0,
        Decision::Deny
    );
    assert_eq!(
        policy.explain(&Effect::new(FS_WRITE, "/ws/secrets/x")).0,
        Decision::Deny
    );
    // 普通工作区文件。
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/ws/src/main.rs")).0,
        Decision::Allow
    );
}

#[test]
fn glob_metacharacters_have_the_documented_meaning() {
    // `**` 跨目录，且 `**/` 可以匹配零层。
    assert!(glob_match("/ws/**", "/ws/a/b.txt"));
    assert!(glob_match("**/*.js", "a/b/c.js"));
    assert!(glob_match("**/*.js", "c.js"));
    // `*` 不跨目录。
    assert!(!glob_match("*.js", "a/c.js"));
    // `?` 正好一个字符。
    assert!(glob_match("src/?ain.rs", "SRC/main.rs"));
    assert!(!glob_match("src/?in.rs", "src/main.rs"));
    // 没有尾部 `/` 就不等于目录内部。
    assert!(!glob_match("/ws/**", "/ws"));
    // 空 pattern 只匹配空文本。
    assert!(glob_match("", ""));
    assert!(!glob_match("", "x"));
    // 反斜杠归一（Windows 路径也能被同一条规则覆盖）。
    assert!(glob_match("/ws/a\\b", "/ws/a/b"));
    // 大小写不敏感是文档化的语义。
    assert!(glob_match("/WS/**", "/ws/x"));
}

#[test]
fn relative_workspace_without_dotdot_still_works() {
    // 库调用方可以传相对 workspace（CLI 会 canonicalize）。
    // 规范化不能在普通路径上把 workspace 前缀吃掉，否则所有调用都会变成 deny。
    let ctx = Ctx {
        workspace: PathBuf::from("demo"),
        ..Default::default()
    };
    let policy = Policy::new(Policy::defaults(Path::new("demo")), Path::new("demo"));
    let inside = Json::obj(vec![("path", Json::from("a.txt"))]);
    let effects = (find(&registry(), "fs_read").unwrap().effects)(&inside, &ctx);
    assert_eq!(policy.decide(&effects).decision, Decision::Allow);

    // 而 `..` 仍然要在进入策略层之前折掉。
    let escape = Json::obj(vec![("path", Json::from("../outside.txt"))]);
    let effects = (find(&registry(), "fs_read").unwrap().effects)(&escape, &ctx);
    assert_eq!(policy.decide(&effects).decision, Decision::Deny);
}

#[test]
fn session_approval_is_exact_and_does_not_widen_glob_characters() {
    let ws = Path::new("/ws");
    let mut policy = Policy::new(Policy::defaults(ws), ws);

    // 用户批准的是「这一次」命令，不是「以 echo 开头且不含 / 的任何命令」。
    let approved = Effect::new(PROC_EXEC, "echo *");
    assert_eq!(policy.explain(&approved).0, Decision::Ask);
    policy.add_rule(
        Rule::new(PROC_EXEC, &approved.target, Decision::Allow).with_note("本次会话中批准"),
    );
    assert_eq!(
        policy.explain(&approved).0,
        Decision::Allow,
        "被批准的那一次效果必须免审"
    );
    assert_eq!(
        policy.explain(&Effect::new(PROC_EXEC, "echo pwned")).0,
        Decision::Ask,
        "批准 `echo *` 不能顺带批准 `echo pwned`"
    );

    // 路径里的 `?` 同理：批准 secrets 下某一个文件不能放开同族路径。
    let secret = Effect::new(FS_READ, "/ws/secrets/a?b");
    assert_eq!(policy.explain(&secret).0, Decision::Deny);
    policy.add_rule(Rule::new(FS_READ, &secret.target, Decision::Allow));
    assert_eq!(policy.explain(&secret).0, Decision::Allow);
    assert_eq!(
        policy.explain(&Effect::new(FS_READ, "/ws/secrets/axb")).0,
        Decision::Deny,
        "批准 `a?b` 不能顺带批准 `axb`"
    );
}

#[test]
fn longer_literal_prefix_wins_over_order() {
    let policy = Policy::new(
        vec![
            Rule::new(FS_WRITE, "**", Decision::Allow),
            Rule::new(FS_WRITE, "/ws/secrets/**", Decision::Deny),
            Rule::new(FS_WRITE, "/ws/**", Decision::Allow),
        ],
        Path::new("/ws"),
    );
    assert_eq!(
        policy.explain(&Effect::new(FS_WRITE, "/ws/secrets/x")).0,
        Decision::Deny,
        "更长字面前缀的 deny 必须赢过更宽的 allow"
    );
    assert_eq!(
        policy.explain(&Effect::new(FS_WRITE, "/ws/x")).0,
        Decision::Allow
    );
}

// 只在 unix 上跑：Windows 没有 std::os::unix，而且建符号链接要特权。
// 不加 cfg 的话，`cargo check --tests --target x86_64-pc-windows-gnu` 直接编不过。
#[cfg(unix)]
#[test]
fn a_symlink_cannot_smuggle_reads_out_of_the_workspace() {
    // 词法上 link.txt 在工作区里，真实位置在外面：策略只看字面路径，所以必须靠真实路径兜底。
    let tmp = std::env::temp_dir().join(format!("ui-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let ws = tmp.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(tmp.join("secret.txt"), "TOP-SECRET").unwrap();
    std::os::unix::fs::symlink("../secret.txt", ws.join("link.txt")).unwrap();

    let journal = ui_core::Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
    let script = ui_core::json::parse(
        r#"{"model":"scripted","steps":[
            {"content":"读链接","toolCalls":[{"name":"fs_read","args":{"path":"link.txt"}}]},
            {"content":"结束"}]}"#,
    )
    .unwrap();
    let model = Box::new(ui_core::ScriptedModel::from_json(&script).unwrap());
    let mut agent = ui_core::Agent::new(
        journal,
        model,
        ws.clone(),
        ui_core::Limits::default(),
    );
    agent.run_turn("读链接", &mut ui_core::NullHooks).unwrap();

    let events = ui_core::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let request = events
        .iter()
        .find(|e| e.str_at("kind") == Some("tool/request"))
        .expect("应有 tool/request");
    assert_eq!(
        request.str_at("decision"),
        Some("deny"),
        "符号链接指向工作区外时必须拒绝"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn shell_timeout_kills_the_whole_process_tree() {
    // 只 kill 直接子进程时，`sleep 3 & wait` 会活到自然结束，超时形同虚设。
    use std::time::Instant;
    let tmp = std::env::temp_dir().join(format!("ui-shelltimeout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let ctx = ui_core::Ctx {
        workspace: tmp.clone(),
        ..Default::default()
    };
    let tool = ui_core::registry()
        .into_iter()
        .find(|t| t.name == "shell")
        .expect("shell 工具存在");

    let started = Instant::now();
    let result = (tool.run)(
        &ui_core::Json::obj(vec![
            ("command", ui_core::Json::from("sleep 5 & wait")),
            ("timeoutMs", ui_core::Json::from(200u64)),
        ]),
        &ctx,
    );
    let elapsed = started.elapsed().as_millis();
    assert!(result.meta.bool_at("timedOut") == Some(true), "应标记超时");
    assert!(
        elapsed < 1500,
        "超时必须连后代一起杀掉，实际耗时 {}ms（后台进程还活着）",
        elapsed
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
