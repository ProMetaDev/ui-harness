//! 跨命令的不变量：日志路径可控、损坏可检出、JSON 输出可解析。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-cli-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario(dir: &std::path::Path) -> PathBuf {
    let f = dir.join("scenario.json");
    fs::write(&f, r#"{"model":"scripted","steps":[{"content":"好了"}]}"#).unwrap();
    f
}

#[test]
fn journal_flag_redirects_the_log() {
    let ws = temp_ws("journal");
    let sc = scenario(&ws);
    let target = ws.join("custom").join("mine.jsonl");

    let out = Command::new(bin())
        .args([
            "run",
            "--task",
            "x",
            "--mock",
            sc.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--journal",
            target.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert!(target.is_file(), "--journal 指定的文件必须被写到");
    assert!(
        !ws.join(".ui").join("journal.jsonl").exists(),
        "不该同时写默认位置"
    );
    let events = ui_core::journal::Journal::load(&target).unwrap();
    assert!(events.iter().any(|e| e.str_at("kind") == Some("turn/start")));
}

#[test]
fn verify_exits_2_on_a_tampered_journal() {
    use ui_core::json::Json;

    let ws = temp_ws("tamper");
    let path = ws.join(".ui").join("journal.jsonl");
    let mut j = ui_core::Journal::open(&path).unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("a"))]))
        .unwrap();
    j.append("note", Json::obj(vec![("text", Json::from("b"))]))
        .unwrap();

    // 改掉第一条：中间损坏（不是尾部撕裂），必须整体拒绝
    let text = fs::read_to_string(&path).unwrap();
    let tampered = text.replacen("\"a\"", "\"z\"", 1);
    fs::write(&path, tampered).unwrap();

    let out = Command::new(bin())
        .args(["verify", "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(out.status.code(), Some(2), "损坏必须返回 2");
    assert!(String::from_utf8_lossy(&out.stdout).contains("FAIL"));
}

#[test]
fn verify_is_quiet_on_a_clean_journal() {
    let ws = temp_ws("clean");
    let sc = scenario(&ws);
    Command::new(bin())
        .args([
            "run",
            "--task",
            "x",
            "--mock",
            sc.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");

    let out = Command::new(bin())
        .args(["verify", "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("哈希链完整"));
    assert!(stdout.contains("未完成 turn=false"));
}

#[test]
fn non_done_reasons_exit_non_zero() {
    let ws = temp_ws("excode");
    let empty = ws.join("empty.json");
    fs::write(&empty, r#"{"model":"scripted","steps":[{"content":""},{"content":""},{"content":""}]}"#).unwrap();

    let out = Command::new(bin())
        .args([
            "run",
            "--task",
            "说点什么",
            "--mock",
            empty.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(1),
        "empty_reply 这类非 done 的收尾必须返回非零，否则脚本会误判成功"
    );

    let ok = ws.join("ok.json");
    fs::write(&ok, r#"{"model":"scripted","steps":[{"content":"好了"}]}"#).unwrap();
    let out = Command::new(bin())
        .args([
            "run",
            "--task",
            "说点什么",
            "--mock",
            ok.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn audit_rejudges_a_quiet_run_under_strict_mode() {
    // 模式只改严格度、不改日志格式：所以 quiet 跑完还能用 strict 重判。
    let ws = temp_ws("modes");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"写个文件","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},
            {"content":"好了"}]}"#,
    )
    .unwrap();

    let run = Command::new(bin())
        .args([
            "run",
            "--task",
            "写个文件",
            "--mock",
            sc.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--mode",
            "quiet",
            "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));

    // 日志必须记下当时的模式
    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let turn = events
        .iter()
        .find(|e| e.str_at("kind") == Some("turn/start"))
        .expect("应有 turn/start");
    assert_eq!(turn.str_at("mode"), Some("quiet"));

    // 用 strict 重判：那次写入会被要求确认
    let audit = Command::new(bin())
        .args([
            "audit",
            "--workspace",
            ws.to_str().unwrap(),
            "--as",
            "strict",
            "--json",
        ])
        .output()
        .expect("audit");
    assert!(audit.status.success(), "{}", String::from_utf8_lossy(&audit.stderr));
    let parsed = ui_core::json::parse(String::from_utf8_lossy(&audit.stdout).trim()).unwrap();
    assert_eq!(parsed.str_at("as"), Some("strict"));
    assert!(
        parsed.u64_at("escalated").unwrap_or(0) >= 1,
        "严格模式下应至少有一个动作被收紧"
    );
    let rows = parsed.arr_at("rows").cloned().unwrap_or_default();
    assert_eq!(rows[0].str_at("tool"), Some("fs_write"));
    assert_eq!(rows[0].str_at("was"), Some("allow"));
    assert_eq!(rows[0].str_at("now"), Some("ask"));
}

#[test]
fn lab_bundle_records_lineage_and_verify_catches_tampering() {
    let ws = temp_ws("lab");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"写结论","toolCalls":[{"name":"fs_write","args":{"path":"metrics.json","content":"{\"acc\":0.8}"}}]},
            {"content":"好了"}]}"#,
    )
    .unwrap();
    let run = Command::new(bin())
        .args([
            "run", "--task", "写结论", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success());

    let bundle = ws.join("bundle");
    let pack = Command::new(bin())
        .args([
            "lab", "pack", "--workspace", ws.to_str().unwrap(),
            "--name", "t1", "--out", bundle.to_str().unwrap(),
        ])
        .output()
        .expect("pack");
    assert!(pack.status.success(), "{}", String::from_utf8_lossy(&pack.stderr));

    let manifest = fs::read_to_string(bundle.join("manifest.jsonl")).unwrap();
    assert!(manifest.contains("metrics.json"), "产物必须在清单里");
    let rec = ui_core::json::parse(manifest.lines().next().unwrap()).unwrap();
    assert_eq!(rec.str_at("path"), Some("metrics.json"));
    assert_eq!(rec.u64_at("bytes"), Some(11)); // {"acc":0.8} ? 11 ??
    // 血缘：哪次调用写的
    let origin = rec.get("producedBy").cloned().unwrap();
    assert_eq!(origin.str_at("tool"), Some("fs_write"));
    assert!(origin.u64_at("seq").unwrap_or(0) > 0);

    let ok = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(ok.status.code(), Some(0));

    // 篡改产物：必须被抓住
    fs::write(bundle.join("artifacts").join("metrics.json"), "{\"acc\":0.99}").unwrap();
    let bad = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(bad.status.code(), Some(3), "篡改后必须返回 3");
    assert!(String::from_utf8_lossy(&bad.stdout).contains("哈希不符"));
}

#[test]
fn lab_records_reads_and_verify_flags_data_drift() {
    let ws = temp_ws("lineage");
    fs::write(ws.join("data.csv"), "x,y\n1,0\n").unwrap();
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"读数据","toolCalls":[{"name":"fs_read","args":{"path":"data.csv"}}]},
            {"content":"好了"}]}"#,
    )
    .unwrap();
    let run = Command::new(bin())
        .args([
            "run", "--task", "读数据", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success());

    let bundle = ws.join("b");
    let pack = Command::new(bin())
        .args([
            "lab", "pack", "--workspace", ws.to_str().unwrap(),
            "--name", "l1", "--out", bundle.to_str().unwrap(),
        ])
        .output()
        .expect("pack");
    assert!(pack.status.success(), "{}", String::from_utf8_lossy(&pack.stderr));

    let inputs = fs::read_to_string(bundle.join("inputs.jsonl")).unwrap();
    let rec = ui_core::json::parse(inputs.lines().next().unwrap()).unwrap();
    assert_eq!(rec.str_at("path"), Some("data.csv"));
    assert!(rec.str_at("sha256").unwrap().len() == 64);

    let ok = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap(), "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(ok.status.code(), Some(0), "数据没变时应通过");

    // 数据换版本：结论就不成立了
    fs::write(ws.join("data.csv"), "x,y\n1,0\n9,9\n").unwrap();
    let drift = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap(), "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(drift.status.code(), Some(3), "数据漂移时必须返回 3");
    assert!(String::from_utf8_lossy(&drift.stdout).contains("输入"));
}

#[test]
fn lab_table_aggregates_seeds_and_refuses_mixed_data_versions() {
    let root = temp_ws("table");
    let mk = |name: &str, acc: f64, hash: &str| {
        let d = root.join(name);
        fs::create_dir_all(d.join("artifacts")).unwrap();
        fs::write(
            d.join("meta.json"),
            r#"{"name":"","createdMs":1,"workspace":"/tmp/x","journalEvents":1,
                "env":{"os":"Ubuntu","kernel":"k","cpus":8,"python":"3"}}"#
                .replace("\"name\":\"\"", &format!("\"name\":\"{}\"", name)),
        )
        .unwrap();
        fs::write(
            d.join("inputs.jsonl"),
            format!("{{\"path\":\"data.csv\",\"sha256\":\"{}\",\"bytes\":1}}\n", hash),
        )
        .unwrap();
        fs::write(
            d.join("artifacts").join("metrics.json"),
            format!("{{\"seed\":1,\"accuracy\":{}}}\n", acc),
        )
        .unwrap();
        d
    };
    let a = mk("a", 0.80, "hash-same");
    let b = mk("b", 0.82, "hash-same");

    let out = Command::new(bin())
        .args(["lab", "table", a.to_str().unwrap(), b.to_str().unwrap(), "--json"])
        .output()
        .expect("table");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = ui_core::json::parse(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v.u64_at("bundles"), Some(2));
    assert_eq!(v.u64_at("inputDrift"), Some(0));
    let metrics = v.arr_at("metrics").cloned().unwrap();
    assert_eq!(metrics.len(), 1, "seed 不该被当成指标");
    assert_eq!(metrics[0].str_at("metric"), Some("accuracy"));

    // 数据版本不一致：拒绝求均值
    let c = mk("c", 0.90, "hash-other");
    let mixed = Command::new(bin())
        .args(["lab", "table", a.to_str().unwrap(), c.to_str().unwrap()])
        .output()
        .expect("table");
    assert_eq!(mixed.status.code(), Some(3), "数据版本不同必须返回 3");
    assert!(String::from_utf8_lossy(&mixed.stdout).contains("不能求均值"));
}

#[test]
fn lab_claims_must_be_backed_by_artifacts() {
    let ws = temp_ws("claims");
    fs::write(ws.join("run_exp.sh"), "printf '{\"accuracy\":0.72}\\n' > metrics.json\n").unwrap();
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"跑实验","toolCalls":[{"name":"shell","args":{"command":"sh run_exp.sh"}}]},
            {"content":"准确率 0.72"}]}"#,
    )
    .unwrap();
    let run = Command::new(bin())
        .args([
            "run", "--task", "跑实验", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));

    let claim = Command::new(bin())
        .args([
            "lab", "claim", "--workspace", ws.to_str().unwrap(),
            "--name", "accuracy", "--value", "0.72",
        ])
        .output()
        .expect("claim");
    assert!(claim.status.success());

    let bundle = ws.join("b");
    let pack = Command::new(bin())
        .args([
            "lab", "pack", "--workspace", ws.to_str().unwrap(),
            "--name", "c1", "--out", bundle.to_str().unwrap(),
        ])
        .output()
        .expect("pack");
    assert!(pack.status.success(), "{}", String::from_utf8_lossy(&pack.stderr));

    let ok = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(ok.status.code(), Some(0), "{}", String::from_utf8_lossy(&ok.stdout));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("结论 1/1 对得上"));

    // 产物对不上结论：必须失败
    fs::write(bundle.join("artifacts").join("metrics.json"), "{\"accuracy\":0.99}\n").unwrap();
    let bad = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap()])
        .output()
        .expect("verify");
    assert_eq!(bad.status.code(), Some(3));
    let text = String::from_utf8_lossy(&bad.stdout);
    assert!(text.contains("哈希不符"));
    assert!(text.contains("找不到对应数值"));
}

#[test]
fn mcp_tools_are_callable_and_policy_visible() {
    let ws = temp_ws("mcp");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"调外部工具","toolCalls":[{"name":"mcp_call","args":{"server":"demo","tool":"echo","args":{"text":"hi"}}}]},
            {"content":"好了"}]}"#,
    )
    .unwrap();
    // 配置里必须用**绝对路径**：MCP server 是子进程，cwd 不保证在仓库根。
    // 之前用 fixtures/mcp/demo.json（里面写死了作者本机的 /mnt/x 路径），
    // 本地能过、CI 必挂——这是真实踩过的。
    let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("fixtures/mcp/server.py");
    let server = server.to_string_lossy().replace('\\', "/");
    let cfg = ws.join("mcp.json");
    fs::write(
        &cfg,
        format!(
            r#"{{"servers":[{{"name":"demo","cmd":["python3","{}"]}}]}}"#,
            server
        ),
    )
    .unwrap();
    let out = Command::new(bin())
        .args([
            "run", "--task", "用外部工具", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
            "--mcp", cfg.to_str().unwrap(),
        ])
        .output()
        .expect("run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{}", text);
    assert!(text.contains("echo: hi"), "工具结果没回来：{}", text);

    // 策略看得见：proc.exec 目标是 mcp:demo:echo，默认要审批
    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let req = events
        .iter()
        .find(|e| e.str_at("kind") == Some("tool/request"))
        .expect("应有 tool/request");
    assert_eq!(req.str_at("tool"), Some("mcp_call"));
    let effects = req.arr_at("effects").cloned().unwrap();
    assert_eq!(effects[0].str_at("kind"), Some("proc.exec"));
    assert_eq!(effects[0].str_at("target"), Some("mcp:demo:echo"));
    assert_eq!(req.str_at("decision"), Some("ask"));
}

#[test]
fn shell_writes_are_observed_and_fine_grained_tools_are_not() {
    // 观测式效果：shell 只声明 proc.exec，命令内部写了什么声明里没有，
    // 所以执行前后各扫一次工作区，把**实际改动**记进日志。
    // 反过来，fs_write 已经精确声明了路径，不该重复记一份 observed。
    let ws = temp_ws("observe");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"shell 写文件","toolCalls":[{"name":"shell","args":{"command":"echo hi > by-shell.txt"}}]},
            {"content":"精确写","toolCalls":[{"name":"fs_write","args":{"path":"by-tool.txt","content":"x"}}]},
            {"content":"完成"}]}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args([
            "run", "--task", "t", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let observed: Vec<_> = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("effect/observed"))
        .collect();
    assert_eq!(observed.len(), 1, "只该有一条观测记录（来自 shell）");
    let ev = observed[0];
    assert_eq!(ev.str_at("tool"), Some("shell"));
    let added = ev
        .get("changes")
        .and_then(|c| c.arr_at("added").cloned())
        .unwrap_or_default();
    let names: Vec<&str> = added.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"by-shell.txt"),
        "观测要记下 shell 实际写的文件，实际: {:?}",
        names
    );
    assert!(
        !names.contains(&"by-tool.txt"),
        "fs_write 的写入由它自己的效果覆盖，不该出现在观测里"
    );
}

#[test]
fn observation_can_be_switched_off() {
    let ws = temp_ws("observe-off");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"shell 写文件","toolCalls":[{"name":"shell","args":{"command":"echo hi > by-shell.txt"}}]},
            {"content":"完成"}]}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .env("UI_OBSERVE", "0")
        .args([
            "run", "--task", "t", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(out.status.success());

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    assert!(
        events
            .iter()
            .all(|e| e.str_at("kind") != Some("effect/observed")),
        "UI_OBSERVE=0 时不该有观测记录"
    );
}

#[test]
fn lab_pack_keeps_artifacts_flat_and_not_as_inputs() {
    // 回归（真模型跑出来的两个 bug）：
    //   1. 产物被复制成 bundle/artifacts/artifacts/metrics.json，对照表按目录扫数值扫不到；
    //   2. agent 读回自己刚写的产物，被记成「输入」，于是多 seed 表因为
    //      「同一路径哈希不同」拒绝求均值——而产物理所当然每个 seed 都不同。
    let ws = temp_ws("labpack");
    fs::create_dir_all(ws.join("artifacts")).unwrap();
    fs::write(ws.join("fixtures_input.txt"), "input-v1\n").unwrap();

    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"先读输入","toolCalls":[{"name":"fs_read","args":{"path":"fixtures_input.txt"}}]},
            {"content":"写产物","toolCalls":[{"name":"fs_write","args":{"path":"artifacts/metrics.json","content":"{\"accuracy\":0.9}"}}]},
            {"content":"读回产物确认","toolCalls":[{"name":"fs_read","args":{"path":"artifacts/metrics.json"}}]},
            {"content":"完成"}]}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args([
            "run", "--task", "t", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(bin())
        .args(["lab", "pack", "--name", "s1", "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("pack");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let bundle = ws.join("experiments").join("s1");
    assert!(
        bundle.join("artifacts").join("metrics.json").is_file(),
        "产物应当平铺在 bundle/artifacts/ 下，不能多套一层 artifacts/"
    );
    assert!(
        !bundle.join("artifacts").join("artifacts").exists(),
        "不该出现 artifacts/artifacts 这种双层目录"
    );

    let inputs = fs::read_to_string(bundle.join("inputs.jsonl")).unwrap();
    assert!(inputs.contains("fixtures_input.txt"), "真正的输入要记下来: {}", inputs);
    assert!(
        !inputs.contains("artifacts/metrics.json"),
        "产物不能被记成输入（否则多 seed 表拒绝求均值）: {}",
        inputs
    );

    // 校验必须真的数到 1 个产物
    let out = Command::new(bin())
        .args(["lab", "verify", bundle.to_str().unwrap()])
        .output()
        .expect("verify");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("产物 1 个校验通过"), "校验没数到产物: {}", text);
}

#[test]
fn export_streams_events_and_refuses_a_tampered_journal() {
    // 导出曾经把全部事件读进内存再拼成一个字符串（7 万条要 112 MB）。
    // 改成流式之后：事件数与日志一致，且损坏的日志必须拒绝导出。
    let ws = temp_ws("export");
    let sc = ws.join("sc.json");
    fs::write(
        &sc,
        r#"{"model":"scripted","steps":[
            {"content":"a","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"1"}}]},
            {"content":"b","toolCalls":[{"name":"fs_write","args":{"path":"b.txt","content":"2"}}]},
            {"content":"c","toolCalls":[{"name":"fs_write","args":{"path":"c.txt","content":"3"}}]},
            {"content":"done"}]}"#,
    )
    .unwrap();
    let out = Command::new(bin())
        .args([
            "run", "--task", "t", "--mock", sc.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--yes",
        ])
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let html_path = ws.join("trace.html");
    let out = Command::new(bin())
        .args([
            "export", "--workspace", ws.to_str().unwrap(), "--html", html_path.to_str().unwrap(),
        ])
        .output()
        .expect("export");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // 内嵌 JSON 必须完整可解析，且事件数与日志行数一致
    let html = fs::read_to_string(&html_path).unwrap();
    let marker = "<script id=\"ui-data\" type=\"application/json\">";
    let start = html.find(marker).expect("内嵌数据块") + marker.len();
    let end = html[start..].find("</script>").expect("数据块结束") + start;
    let data = ui_core::json::parse(&html[start..end]).expect("内嵌 JSON 必须可解析");
    let embedded = data.arr_at("events").map(|a| a.len()).unwrap_or(0);
    let lines = fs::read_to_string(ws.join(".ui").join("journal.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(embedded, lines, "导出的事件数必须与日志一致");

    // 篡改一个字节：必须拒绝，且不留下半成品文件
    let jp = ws.join(".ui").join("journal.jsonl");
    let text = fs::read_to_string(&jp).unwrap();
    fs::write(&jp, text.replacen("\"a.txt\"", "\"evil.txt\"", 1)).unwrap();
    let bad = ws.join("bad.html");
    let out = Command::new(bin())
        .args([
            "export", "--workspace", ws.to_str().unwrap(), "--html", bad.to_str().unwrap(),
        ])
        .output()
        .expect("export");
    assert_eq!(out.status.code(), Some(2), "损坏日志的导出必须退出 2");
    assert!(!bad.exists(), "损坏日志不该产出文件");
}
