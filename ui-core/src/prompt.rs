//! Contract 1: one step, one immutable prompt snapshot.
//!
//! The prompt is not a string a plugin mutates on the way out. It is a list of
//! attributed sections, each priced, hashed and journaled, which is what makes a
//! step diffable, replayable and billable.
//!
//! The `env` section is generated from the live process and then *verified*. If
//! the harness cannot prove a claim (workspace missing, temp unusable), it says
//! so instead of handing the model a fiction.

use crate::budget::estimate_tokens;
use crate::sha256;
use crate::tools::Tool;
use std::fs;
use std::path::{Path, PathBuf};

/// Distinguishes concurrent environment probes inside one process.
static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const CORE: &str = "你是 UI 里的执行代理。工作方式：

1. 先看清楚再做。改动前用 fs_read 读原文，别凭记忆改。
2. 一次只做一件可验证的事；做完用工具确认结果，不要仅凭推理宣布完成。
3. 工具调用是唯一的副作用来源；没有调用就等于没做。
4. 失败就报告失败，不要用「应该可以」替代验证。
5. 需要人做决定时停下来问，不要猜。";

#[derive(Debug, Clone)]
pub struct Env {
    pub os: String,
    pub runtime: String,
    pub cwd: PathBuf,
    pub workspace: PathBuf,
    pub workspace_ok: bool,
    pub shell: String,
    pub temp: PathBuf,
    pub temp_ok: bool,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

impl Env {
    pub fn render(&self) -> String {
        let mut lines = vec![
            format!("os: {}", self.os),
            format!("runtime: {}", self.runtime),
            format!(
                "workspace: {}{}",
                self.workspace.display(),
                if self.workspace_ok { "" } else { "  ⚠ 不存在" }
            ),
            format!("cwd: {}", self.cwd.display()),
            format!("shell: {}", self.shell),
            format!(
                "temp: {}{}",
                self.temp.display(),
                if self.temp_ok { "" } else { "  ⚠ 不可写" }
            ),
        ];
        lines.extend(self.notes.iter().cloned());
        lines.extend(self.warnings.iter().map(|w| format!("⚠ {}", w)));
        lines.join("\n")
    }
}

pub fn inspect_environment(workspace: &Path) -> Env {    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    let workspace_ok = fs::metadata(workspace).map(|m| m.is_dir()).unwrap_or(false);
    if !workspace_ok {
        warnings.push(format!("workspace 不存在或不是目录: {}", workspace.display()));
    }

    let temp = std::env::temp_dir();
    // A per-call unique probe: two threads inspecting the environment at once
    // must not fight over one path (that race made the snapshot non-deterministic).
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = temp.join(format!("ui-probe-{}-{}", std::process::id(), seq));
    let wrote = fs::write(&probe, b"").is_ok();
    let cleaned = match fs::remove_file(&probe) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    };
    let temp_ok = wrote && cleaned;
    if !temp_ok {
        warnings.push(format!(
            "临时目录不可写: {} —— 工具可能把临时文件写进工作区",
            temp.display()
        ));
    }

    let shell = if cfg!(windows) {
        "PowerShell / cmd (Windows)"
    } else {
        "sh -c (POSIX)"
    }
    .to_string();

    if cfg!(windows) {
        notes.push("本机是 Windows：命令用 PowerShell 方言，环境变量用 $env:NAME。".to_string());
    }
    notes.push(
        "工具里的相对路径一律相对 workspace 解析；不要把 workspace 目录名再拼进路径。"
            .to_string(),
    );

    Env {
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        runtime: format!("rust (ui {})", env!("CARGO_PKG_VERSION")),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        workspace: workspace.to_path_buf(),
        workspace_ok,
        shell,
        temp,
        temp_ok,
        notes,
        warnings,
    }
}

#[derive(Debug, Clone)]
pub struct Section {
    pub id: &'static str,
    pub owner: &'static str,
    pub text: String,
    pub tokens: u64,
    pub bytes: usize,
    pub volatility: &'static str,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub sections: Vec<Section>,
    pub text: String,
    pub tokens: u64,
    pub bytes: usize,
    pub env: Env,
}

impl Snapshot {
    /// Journal form: attribution and price, without the text (kept once, in the
    /// snapshot file, so the journal stays cheap to scan).
    pub fn to_json(&self) -> crate::json::Json {
        use crate::json::Json;
        Json::obj(vec![
            ("snapshotId", Json::from(self.id.as_str())),
            (
                "sections",
                Json::Arr(
                    self.sections
                        .iter()
                        .map(|s| {
                            Json::obj(vec![
                                ("id", Json::from(s.id)),
                                ("owner", Json::from(s.owner)),
                                ("tokens", Json::from(s.tokens)),
                                ("bytes", Json::from(s.bytes)),
                                ("volatility", Json::from(s.volatility)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("tokens", Json::from(self.tokens)),
            ("bytes", Json::from(self.bytes)),
        ])
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: "user",
            content: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Message {
        Message {
            role: "assistant",
            content: text.into(),
        }
    }

    pub fn tool(name: &str, ok: bool, content: &str) -> Message {
        Message {
            role: "tool",
            content: format!(
                "[{}] {}: {}",
                name,
                if ok { "ok" } else { "error" },
                truncate(content, 4000)
            ),
        }
    }

    pub fn note(text: impl Into<String>) -> Message {
        Message {
            role: "note",
            content: text.into(),
        }
    }

    fn render(&self) -> String {
        match self.role {
            "tool" => self.content.clone(),
            other => format!("{}: {}", other, self.content),
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &s[..end]
}

fn render_tools(tools: &[Tool]) -> String {
    let mut out = tools
        .iter()
        .map(|t| {
            let params = t
                .params
                .iter()
                .map(|(name, ty, required)| {
                    format!("{}: {}{}", name, ty, if *required { " (required)" } else { "" })
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "- {}({}) — {} [{}]",
                t.name,
                params,
                t.summary,
                if t.idempotent { "可重放" } else { "有副作用" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // 外部 MCP 工具：模型必须知道有哪些，否则 mcp_call 只能瞎猜
    if let Some(extra) = EXTRA_TOOLS.get() {
        if !extra.is_empty() {
            out.push_str("\n\n外部 MCP 工具（用 mcp_call 调，server/tool 照抄冒号前的部分）：");
            for (name, desc) in extra {
                out.push_str(&format!("\n- {}（{}）", name, desc));
            }
        }
    }
    out
}

/// 启动时注入一次：MCP server 的工具清单
static EXTRA_TOOLS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

pub fn set_extra_tools(list: Vec<(String, String)>) {
    let _ = EXTRA_TOOLS.set(list);
}

/// 启动时注入一次：调用方追加的系统指令（`--append-system-prompt`）
static EXTRA_SYSTEM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_extra_system(text: &str) {
    let _ = EXTRA_SYSTEM.set(text.to_string());
}

pub fn build_snapshot(
    workspace: &Path,
    tools: &[Tool],
    history: &[Message],
    goal: Option<&str>,
    env: Option<Env>,
    skills_catalog: &str,
    loaded_skills: &[(String, String)],
) -> Snapshot {
    let env = env.unwrap_or_else(|| inspect_environment(workspace));
    let mut sections = vec![
        Section {
            id: "core",
            owner: "kernel",
            text: {
                // `--append-system-prompt`：调用方追加的系统指令（A3）。
                // 放在 core 里，每轮都生效，不依赖调用方自己拼历史。
                let mut text = CORE.to_string();
                if let Some(extra) = EXTRA_SYSTEM.get() {
                    if !extra.trim().is_empty() {
                        text.push_str("\n\n## 调用方追加的指令\n\n");
                        text.push_str(extra.trim());
                    }
                }
                text
            },
            tokens: 0,
            bytes: 0,
            volatility: "stable",
        },
        Section {
            id: "env",
            owner: "kernel",
            text: env.render(),
            tokens: 0,
            bytes: 0,
            volatility: "turn",
        },
        Section {
            id: "tools",
            owner: "tools",
            text: render_tools(tools),
            tokens: 0,
            bytes: 0,
            volatility: "session",
        },
    ];

    if !skills_catalog.is_empty() {
        sections.push(Section {
            id: "skills",
            owner: "skill",
            text: format!(
                "可用 skill（需要时用 skill 工具按名字加载正文，不要凭名字猜内容）：\n{}",
                skills_catalog
            ),
            tokens: 0,
            bytes: 0,
            volatility: "session",
        });
    }

    if !loaded_skills.is_empty() {
        let text = loaded_skills
            .iter()
            .map(|(name, body)| format!("### {}\n{}", name, body))
            .collect::<Vec<_>>()
            .join("\n\n");
        sections.push(Section {
            id: "skill-body",
            owner: "skill",
            text,
            tokens: 0,
            bytes: 0,
            volatility: "session",
        });
    }

    if let Some(goal) = goal {
        sections.push(Section {
            id: "goal",
            owner: "goal",
            text: goal.to_string(),
            tokens: 0,
            bytes: 0,
            volatility: "turn",
        });
    }

    if !history.is_empty() {
        sections.push(Section {
            id: "history",
            owner: "session",
            text: history.iter().map(Message::render).collect::<Vec<_>>().join("\n\n"),
            tokens: 0,
            bytes: 0,
            volatility: "step",
        });
    }

    let mut priced = Vec::with_capacity(sections.len());
    for mut section in sections {
        section.tokens = estimate_tokens(&section.text);
        section.bytes = section.text.len();
        priced.push(section);
    }

    let text = priced
        .iter()
        .map(|s| format!("<{}>\n{}\n</{}>", s.id, s.text, s.id))
        .collect::<Vec<_>>()
        .join("\n\n");

    Snapshot {
        id: sha256::hex(text.as_bytes())[..16].to_string(),
        tokens: estimate_tokens(&text),
        bytes: text.len(),
        sections: priced,
        text,
        env,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionDelta {
    pub id: String,
    pub change: &'static str,
    pub tokens: i64,
}

pub fn diff_snapshots(a: Option<&Snapshot>, b: &Snapshot) -> Vec<SectionDelta> {
    let mut out = Vec::new();
    let empty: Vec<Section> = Vec::new();
    let before = a.map(|s| &s.sections).unwrap_or(&empty);
    for s in &b.sections {
        match before.iter().find(|p| p.id == s.id) {
            None => out.push(SectionDelta {
                id: s.id.to_string(),
                change: "added",
                tokens: s.tokens as i64,
            }),
            Some(prev) if prev.bytes != s.bytes || prev.tokens != s.tokens => {
                out.push(SectionDelta {
                    id: s.id.to_string(),
                    change: "changed",
                    tokens: s.tokens as i64 - prev.tokens as i64,
                })
            }
            _ => {}
        }
    }
    for p in before {
        if !b.sections.iter().any(|s| s.id == p.id) {
            out.push(SectionDelta {
                id: p.id.to_string(),
                change: "removed",
                tokens: -(p.tokens as i64),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;

    #[test]
    fn snapshot_is_deterministic_and_priced() {
        let ws = std::env::temp_dir();
        let tools = registry();
        let history = vec![Message::user("你好")];
        let a = build_snapshot(&ws, &tools, &history, None, None, "", &[]);
        let b = build_snapshot(&ws, &tools, &history, None, None, "", &[]);
        let meta_a: Vec<_> = a.sections.iter().map(|s| (s.id, s.bytes)).collect();
        let meta_b: Vec<_> = b.sections.iter().map(|s| (s.id, s.bytes)).collect();
        assert_eq!(meta_a, meta_b, "section sizes must be stable between builds");
        assert_eq!(a.id, b.id);
        assert!(a.tokens > 0);
        assert!(a.sections.iter().any(|s| s.id == "env"));
    }

    #[test]
    fn missing_workspace_is_reported_not_hidden() {
        let ws = Path::new("Z:/definitely/not/here");
        let env = inspect_environment(ws);
        assert!(!env.workspace_ok);
        assert!(env.render().contains("⚠"));
    }

    #[test]
    fn diff_reports_added_and_changed() {
        let ws = std::env::temp_dir();
        let tools = registry();
        let a = build_snapshot(&ws, &tools, &[Message::user("a")], None, None, "", &[]);
        let b = build_snapshot(&ws, &tools, &[Message::user("a"), Message::note("b")], None, None, "", &[]);
        let d = diff_snapshots(Some(&a), &b);
        assert!(d.iter().any(|x| x.id == "history" && x.change == "changed"));
    }
}
