//! Contracts 2 and 3: one call, one effect set; one effect, one decision.
//!
//! A tool never gets a "permission mode". It declares the effects a *specific*
//! call will have (which file, which command, which host), and policy maps those
//! effects to allow / ask / deny using readable data rules. An approval request
//! therefore always carries the effects plus the argument preview.

use crate::json::Json;
use std::path::{Path, PathBuf};

pub const FS_READ: &str = "fs.read";
pub const FS_WRITE: &str = "fs.write";
pub const PROC_EXEC: &str = "proc.exec";
pub const NET_FETCH: &str = "net.fetch";
pub const SECRET_READ: &str = "secret.read";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    pub fn severity(self) -> u8 {
        match self {
            Decision::Allow => 0,
            Decision::Ask => 1,
            Decision::Deny => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }

    pub fn parse(s: &str) -> Option<Decision> {
        match s {
            "allow" => Some(Decision::Allow),
            "ask" => Some(Decision::Ask),
            "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub kind: String,
    pub target: String,
}

impl Effect {
    pub fn new(kind: &str, target: impl Into<String>) -> Effect {
        Effect {
            kind: kind.to_string(),
            target: target.into(),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} {}", self.kind, self.target)
    }

    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("kind", Json::from(self.kind.as_str())),
            ("target", Json::from(self.target.as_str())),
        ])
    }

    /// 从日志里的效果对象还原（`ui audit` 用它重判历史）
    pub fn from_json(j: &Json) -> Option<Effect> {
        Some(Effect {
            kind: j.str_at("kind")?.to_string(),
            target: j.str_at("target")?.to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub effect: String,
    pub pattern: String,
    pub decision: Decision,
    pub note: Option<String>,
    /// `true` 时 pattern 按效果 target 的**原文**匹配，不解释 glob。
    /// 会话内审批规则用它：命令/路径里本来就可能有 `*`、`?`，把一次授权
    /// 当成通配符会把「批准这条」放大成「批准这一族」。
    pub literal: bool,
}

impl Rule {
    pub fn new(effect: &str, pattern: &str, decision: Decision) -> Rule {
        Rule {
            effect: effect.to_string(),
            pattern: pattern.to_string(),
            decision,
            note: None,
            literal: false,
        }
    }

    pub fn with_note(mut self, note: &str) -> Rule {
        self.note = Some(note.to_string());
        self
    }

    pub fn from_json(j: &Json) -> Option<Rule> {
        let effect = j.str_at("effect")?.to_string();
        let pattern = j.str_at("pattern")?.to_string();
        let decision = Decision::parse(j.str_at("decision")?)?;
        Some(Rule {
            effect,
            pattern,
            decision,
            note: j.str_at("note").map(str::to_string),
            literal: j.bool_at("literal").unwrap_or(false),
        })
    }

    pub fn to_json(&self) -> Json {
        let mut items = vec![
            ("effect", Json::from(self.effect.as_str())),
            ("pattern", Json::from(self.pattern.as_str())),
            ("decision", Json::from(self.decision.name())),
        ];
        if let Some(note) = &self.note {
            items.push(("note", Json::from(note.as_str())));
        }
        if self.literal {
            items.push(("literal", Json::Bool(true)));
        }
        Json::obj(items)
    }
}

fn normalize(s: &str) -> Vec<char> {
    s.replace('\\', "/").to_lowercase().chars().collect()
}

/// `**` crosses separators, `*` does not, `?` is one char. Case-insensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    matches_at(&normalize(pattern), &normalize(text))
}

fn matches_at(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' if p.len() > 1 && p[1] == '*' => {
            let rest = &p[2..];
            if rest.first() == Some(&'/') {
                // `**/` may also match zero directory levels
                for i in 0..=t.len() {
                    if matches_at(&rest[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            } else {
                (0..=t.len()).any(|i| matches_at(rest, &t[i..]))
            }
        }
        '*' => {
            let rest = &p[1..];
            for i in 0..=t.len() {
                if i > 0 && t[i - 1] == '/' {
                    break;
                }
                if matches_at(rest, &t[i..]) {
                    return true;
                }
            }
            false
        }
        '?' => !t.is_empty() && matches_at(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && matches_at(&p[1..], &t[1..]),
    }
}

/// Specificity = how much of the pattern is literal; longer wins.
fn specificity(pattern: &str) -> usize {
    match pattern.find(['*', '?']) {
        None => pattern.len() + 1000,
        Some(i) => i,
    }
}

#[derive(Debug)]
pub struct Verdict {
    pub decision: Decision,
    pub reasons: Vec<(Effect, Decision, String)>,
    pub reason: String,
}

#[derive(Debug)]
pub struct Policy {
    pub rules: Vec<Rule>,
    pub workspace: PathBuf,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, workspace: &Path) -> Policy {
        Policy {
            rules,
            workspace: workspace.to_path_buf(),
        }
    }

    pub fn defaults(workspace: &Path) -> Vec<Rule> {
        let ws = workspace.to_string_lossy().replace('\\', "/");
        let mut rules = vec![
            Rule::new(FS_READ, &format!("{}/**", ws), Decision::Allow),
            Rule::new(FS_WRITE, &format!("{}/**", ws), Decision::Allow),
            // 工具可能把「整个 workspace」当作读目标（搜索就是这样），
            // 而 `<ws>/**` 匹配不到 `<ws>` 本身——不补这条，搜索永远被拒。
            Rule::new(FS_READ, ws.as_str(), Decision::Allow).with_note("workspace 根目录本身"),
        ];
        // 受保护目录：模型侧的工具永远读不到也改不了。内核自己读，不经过工具。
        //
        // 注意 `/**` 与目录本身是两条不同的规则：glob 的 `**` 至少要吃掉
        // 一个字符，`<ws>/secrets/**` 匹配不到 `<ws>/secrets`，于是
        // `tree(path="secrets")` / `grep(path="secrets")` 会落进 `<ws>/**`
        // 的 allow。目录本身必须单独 Deny（读 + 写）。
        for dir in crate::protected::PROTECTED_DIRS {
            let dir = format!("{}/{}", ws, dir);
            for effect in [FS_READ, FS_WRITE] {
                rules.push(
                    Rule::new(effect, &dir, Decision::Deny)
                        .with_note("受保护目录本身：工具不允许访问"),
                );
                // 直接构造的 Effect 可能保留尾斜杠（工具内部会规范化，
                // 但策略不能假设调用方一定走 Ctx::resolve）。
                rules.push(
                    Rule::new(effect, &format!("{}/", dir), Decision::Deny)
                        .with_note("受保护目录本身（尾斜杠）：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**", dir), Decision::Deny)
                        .with_note("受保护目录：工具不允许访问"),
                );
            }
        }
        rules.extend([
            Rule::new(FS_WRITE, "**", Decision::Deny).with_note("workspace 之外的写入"),
            Rule::new(FS_READ, "**", Decision::Deny).with_note("workspace 之外的读取"),
            Rule::new(PROC_EXEC, "**", Decision::Ask).with_note("执行命令"),
            Rule::new(NET_FETCH, "**", Decision::Ask).with_note("网络访问"),
            Rule::new(SECRET_READ, "**", Decision::Ask).with_note("读取密钥"),
        ]);
        rules
    }

    pub fn explain(&self, effect: &Effect) -> (Decision, String) {
        let mut best: Option<(&Rule, usize)> = None;
        for rule in &self.rules {
            if rule.effect != effect.kind {
                continue;
            }
            let hit = if rule.literal {
                rule.pattern == effect.target
            } else {
                glob_match(&rule.pattern, &effect.target)
            };
            if !hit {
                continue;
            }
            // 原文规则按「整个 pattern 都是字面量」计分，否则命令里的 `*`
            // 会同时压低估分并放大匹配范围。
            let score = if rule.literal {
                rule.pattern.len() + 1000
            } else {
                specificity(&rule.pattern)
            };
            if best.map(|(_, s)| score >= s).unwrap_or(true) {
                best = Some((rule, score));
            }
        }
        match best {
            Some((rule, _)) => (
                rule.decision,
                rule.note.clone().unwrap_or_else(|| "匹配规则".to_string()),
            ),
            None => (Decision::Deny, "没有匹配的规则（默认拒绝）".to_string()),
        }
    }

    pub fn decide(&self, effects: &[Effect]) -> Verdict {
        let mut overall = Decision::Allow;
        let mut reasons = Vec::new();
        for effect in effects {
            let (decision, reason) = self.explain(effect);
            if decision.severity() > overall.severity() {
                overall = decision;
            }
            reasons.push((effect.clone(), decision, reason));
        }
        let reason = reasons
            .iter()
            .find(|(_, d, _)| *d == overall)
            .map(|(_, _, r)| r.clone())
            .unwrap_or_else(|| "没有效果".to_string());
        Verdict {
            decision: overall,
            reasons,
            reason,
        }
    }

    /// 加一条会话级审批规则。调用点（一次批准）只能提供「这一次的效果」，
    /// 所以 pattern 按 target 原文精确匹配——命令里的 `*`、`?` 是用户看到并
    /// 批准的字符，不是通配符。需要 glob 语义的规则请直接传给 `Policy::new`。
    pub fn add_rule(&mut self, mut rule: Rule) {
        rule.literal = true;
        self.rules.insert(0, rule);
    }

    pub fn to_json(&self) -> Json {
        Json::Obj(vec![(
            "rules".to_string(),
            Json::Arr(self.rules.iter().map(Rule::to_json).collect()),
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_semantics() {
        assert!(glob_match("/x/ws/**", "/x/ws/a/b.txt"));
        assert!(glob_match("**/*.js", "a/b/c.js"));
        assert!(glob_match("**/*.js", "c.js"));
        assert!(!glob_match("*.js", "a/c.js"));
        assert!(glob_match("src/?ain.rs", "SRC/main.rs"));
    }

    #[test]
    fn most_specific_rule_wins() {
        let policy = Policy::new(
            vec![
                Rule::new(FS_WRITE, "**", Decision::Deny),
                Rule::new(FS_WRITE, "/ws/src/**", Decision::Allow),
            ],
            Path::new("/ws"),
        );
        let inside = Effect::new(FS_WRITE, "/ws/src/main.rs");
        let outside = Effect::new(FS_WRITE, "/etc/passwd");
        assert_eq!(policy.explain(&inside).0, Decision::Allow);
        assert_eq!(policy.explain(&outside).0, Decision::Deny);
    }

    #[test]
    fn overall_decision_is_most_severe() {
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        let verdict = policy.decide(&[
            Effect::new(FS_READ, "/ws/a.txt"),
            Effect::new(PROC_EXEC, "rm -rf /"),
        ]);
        assert_eq!(verdict.decision, Decision::Ask);
    }

    #[test]
    fn unknown_effect_is_denied() {
        let policy = Policy::new(vec![], Path::new("/ws"));
        assert_eq!(policy.decide(&[Effect::new("weird", "x")]).decision, Decision::Deny);
    }

    #[test]
    fn workspace_root_itself_is_readable() {
        // fs_search 的效果目标是 workspace 根，而 `<ws>/**` 匹配不到它。
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        assert_eq!(policy.explain(&Effect::new(FS_READ, "/ws")).0, Decision::Allow);
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws/src/a.rs")).0,
            Decision::Allow
        );
    }

    #[test]
    fn secrets_are_denied_even_inside_the_workspace() {
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws/secrets/providers.toml")).0,
            Decision::Deny
        );
        assert_eq!(
            policy.explain(&Effect::new(FS_WRITE, "/ws/secrets/x.toml")).0,
            Decision::Deny
        );
    }

    #[test]
    fn secrets_directory_itself_is_denied() {
        // `<ws>/secrets/**` 匹配不到目录本身；`grep(path="secrets")` /
        // `tree(path="secrets")` 的效果目标正是这个目录。没有这条 Deny，
        // 它会落进 `<ws>/**` 的 allow。
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        for kind in [FS_READ, FS_WRITE] {
            assert_eq!(
                policy.explain(&Effect::new(kind, "/ws/secrets")).0,
                Decision::Deny,
                "{} /ws/secrets 必须被拒",
                kind
            );
            assert_eq!(
                policy.explain(&Effect::new(kind, "/ws/secrets/")).0,
                Decision::Deny,
                "{} /ws/secrets/ 必须被拒",
                kind
            );
        }
        // 根目录本身仍然允许：fs_search 的效果目标就是它。
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws")).0,
            Decision::Allow
        );
    }

    #[test]
    fn protected_dir_rules_follow_the_shared_list() {
        // 清单是唯一来源：往 PROTECTED_DIRS 里加目录，策略自动跟着拒。
        // 如果策略层又写死了一份 "secrets"，这条测试不会发现；但它至少
        // 保证清单里的每一项都同时有「目录本身」和「目录内容」的 Deny。
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        for dir in crate::protected::PROTECTED_DIRS {
            for path in [format!("/ws/{}", dir), format!("/ws/{}/x", dir)] {
                for kind in [FS_READ, FS_WRITE] {
                    assert_eq!(
                        policy.explain(&Effect::new(kind, &path)).0,
                        Decision::Deny,
                        "{} {} 必须被拒",
                        kind,
                        path
                    );
                }
            }
        }
    }
}
