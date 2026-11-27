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
/// shell 命令串里出现了隐私路径。它只证明「这次调用可能读取隐私内容」，
/// 不证明子进程真的读了什么；因此按执行通道仲裁，不冒充读侧沙箱。
pub const READ_PRIVATE: &str = "read.private";

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
    /// 基线规则数量。会话审批规则追加在它后面，切换任务时只截掉这段尾巴。
    base_rules: usize,
}

impl Policy {
    pub fn new(rules: Vec<Rule>, workspace: &Path) -> Policy {
        let base_rules = rules.len();
        Policy {
            rules,
            workspace: workspace.to_path_buf(),
            base_rules,
        }
    }

    /// 丢弃本次会话里通过审批加上的规则，回到数据规则基线。
    ///
    /// `a` / “本次会话允许”只属于当前 run。长驻 worker 不重置的话，
    /// 上一任务的一次授权会悄悄批准下一任务。
    pub fn reset_session_rules(&mut self) {
        if self.rules.len() > self.base_rules {
            self.rules.truncate(self.base_rules);
        }
    }

    pub fn session_rule_count(&self) -> usize {
        self.rules.len().saturating_sub(self.base_rules)
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
        // 同一份清单还要吃掉**嵌套**的受保护目录：子项目里放一个 `src/.ui/`
        // 或 `pkg/.git/`，工具侧的 is_protected_path 会按目录段拒绝，策略层
        // 如果只写 `<ws>/.ui/**` 就慢了一步——两边必须是同一个边界，否则
        // 「谁说了算」取决于请求从哪条路进来。
        //
        // 放在 workspace allow 之后：`<ws>/**/.ui/**` 与 `<ws>/**` 的置信度
        // 相同（首个通配符都在第 4 个字符），同分取后者。
        for dir in crate::protected::PROTECTED_DIRS {
            for effect in [FS_READ, FS_WRITE] {
                rules.push(
                    Rule::new(effect, &format!("{}/**/{}", ws, dir), Decision::Deny)
                        .with_note("嵌套的受保护目录本身：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**/{}/", ws, dir), Decision::Deny)
                        .with_note("嵌套的受保护目录本身（尾斜杠）：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**/{}/**", ws, dir), Decision::Deny)
                        .with_note("嵌套的受保护目录：工具不允许访问"),
                );
            }
        }
        // 人在命令行上追加的隐私路径（`--private` / `UI_PRIVATE`）走**同一套**
        // 形状：目录本身、带尾斜杠、整棵子树，再加嵌套位置的变体。分开写两份
        // 判据就等于"结论取决于请求从哪条路进来"。
        for sub in crate::protected::private_subpaths(Path::new(ws.as_str())) {
            for effect in [FS_READ, FS_WRITE] {
                let dir = format!("{}/{}", ws, sub);
                rules.push(
                    Rule::new(effect, &dir, Decision::Deny)
                        .with_note("声明的隐私路径本身：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**", dir), Decision::Deny)
                        .with_note("声明的隐私路径：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**/{}", ws, sub), Decision::Deny)
                        .with_note("嵌套位置的声明隐私路径：工具不允许访问"),
                );
                rules.push(
                    Rule::new(effect, &format!("{}/**/{}/**", ws, sub), Decision::Deny)
                        .with_note("嵌套位置的声明隐私路径内容：工具不允许访问"),
                );
            }
        }
        // 受保护目录里的读取例外：长工具输出落在 `.ui/tool-output/`，上下文
        // 只留一小片，模型必须能把全文读回来（写不在例外里，见 protected.rs）。
        //
        // 必须放在上面的 Deny 之后：`explain` 对同分规则取「后者胜」，而
        // `<ws>/.ui/tool-output/*` 与 `<ws>/.ui/tool-output/**` 的置信度
        // 完全相同（两个段 + 两个通配符），放前面会被拒绝规则盖掉。
        for exc in crate::protected::PROTECTED_READ_EXCEPTIONS {
            let path = format!("{}/{}", ws, exc.trim_end_matches('/'));
            rules.push(
                Rule::new(FS_READ, &format!("{}/*", path), Decision::Allow)
                    .with_note("被截断的工具输出：允许读回全文"),
            );
        }
        rules.extend([
            Rule::new(FS_WRITE, "**", Decision::Deny).with_note("workspace 之外的写入"),
            Rule::new(FS_READ, "**", Decision::Deny).with_note("workspace 之外的读取"),
            Rule::new(PROC_EXEC, "**", Decision::Ask).with_note("执行命令"),
            Rule::new(NET_FETCH, "**", Decision::Ask).with_note("网络访问"),
            Rule::new(SECRET_READ, "**", Decision::Ask).with_note("读取密钥"),
            Rule::new(READ_PRIVATE, "**", Decision::Ask)
                .with_note("命令串可能读取隐私路径；只仲裁执行，不证明实际读取"),
        ]);
        rules
    }

    pub fn explain(&self, effect: &Effect) -> (Decision, String) {
        match self.best_rule(effect) {
            Some(index) => {
                let rule = &self.rules[index];
                (
                    rule.decision,
                    rule.note.clone().unwrap_or_else(|| "匹配规则".to_string()),
                )
            }
            None => (Decision::Deny, "没有匹配的规则（默认拒绝）".to_string()),
        }
    }

    /// 这条效果是否由**会话审批规则**放行（而不是基线数据规则）。
    ///
    /// 模式（strict）会把 Allow 一律抬成 Ask，用来挡住「随手写盘」。但用户
    /// 刚刚明确选过「本轮都允许」的效果不能再抬回去——抬回去等于那颗按钮
    /// 按下去没有任何作用，而屏幕上已经写过「已批准」。
    pub fn session_approved(&self, effect: &Effect) -> bool {
        match self.best_rule(effect) {
            Some(index) => index >= self.base_rules && self.rules[index].decision == Decision::Allow,
            None => false,
        }
    }

    /// 胜出规则的下标：置信度最高者，同分取**后者**（会话规则追加在基线之后）。
    fn best_rule(&self, effect: &Effect) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (index, rule) in self.rules.iter().enumerate() {
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
                best = Some((index, score));
            }
        }
        best.map(|(index, _)| index)
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
    ///
    /// 必须**追加**在基线之后，两个理由缺一不可：
    /// - `reset_session_rules` 认的是「会话规则 = 基线后面那段」，插到最前面
    ///   会把最后一条基线规则当成会话规则裁掉——安全边界先丢，命令授权还在。
    /// - `explain` 同分取后者，追加才能让「刚刚批准的那条」盖过更宽的默认规则
    ///   （比如同样字面量分数的 workspace 根 allow）。
    pub fn add_rule(&mut self, mut rule: Rule) {
        rule.literal = true;
        self.rules.push(rule);
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
    fn private_read_effect_defaults_to_ask_without_claiming_observation() {
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        let (decision, reason) = policy.explain(&Effect::new(READ_PRIVATE, "secrets"));
        assert_eq!(decision, Decision::Ask);
        assert!(reason.contains("不证明实际读取"), "{}", reason);
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

    #[test]
    fn kernel_state_and_git_objects_are_hidden_from_tools() {
        // 共享清单只保证「目录本身 + 一层内容」被拒；这里钉的是具体形状：
        // 日志、会话、内核配置、git 钩子，都是模型不该看见的控制面/历史。
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        for (effect, path) in [
            (FS_READ, "/ws/.ui/journal.jsonl"),
            (FS_READ, "/ws/.ui/sessions/abc.jsonl"),
            (FS_READ, "/ws/.ui/config.toml"),
            (FS_WRITE, "/ws/.ui/journal.jsonl"),
            (FS_READ, "/ws/.git/config"),
            (FS_WRITE, "/ws/.git/hooks/pre-commit"),
            (FS_READ, "/ws/src/.ui/journal.jsonl"),
        ] {
            assert_eq!(
                policy.explain(&Effect::new(effect, path)).0,
                Decision::Deny,
                "{} {} 必须被工具侧拒绝",
                effect,
                path
            );
        }
    }

    #[test]
    fn nested_protected_dirs_are_denied_too() {
        // 回归：策略原来只写 `<ws>/.ui/**`，子项目里的 `pkg/.git/config`
        // 落进 `<ws>/**` 的 allow。工具侧按「任一段是受保护目录」拒绝，
        // 两边必须同一个边界，否则结论取决于请求从哪条路进来。
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        for dir in crate::protected::PROTECTED_DIRS {
            for path in [
                format!("/ws/pkg/{}/x", dir),
                format!("/ws/a/b/c/{}/x.jsonl", dir),
                format!("/ws/{}", dir),
                format!("/ws/{}", dir.to_uppercase()),
            ] {
                for effect in [FS_READ, FS_WRITE] {
                    assert_eq!(
                        policy.explain(&Effect::new(effect, &path)).0,
                        Decision::Deny,
                        "大小写不敏感：{} {} 必须与工具侧一样被拒",
                        effect,
                        path
                    );
                    // 工具侧的判据必须给出同样的结论
                    assert_eq!(
                        crate::protected::is_protected_path(Path::new("/ws"), Path::new(&path)),
                        true,
                        "{} 在工具侧不受保护，两边的边界不一致",
                        path
                    );
                }
            }
        }
        // 例外不能被嵌套规则吃掉
        assert_eq!(
            policy
                .explain(&Effect::new(FS_READ, "/ws/.ui/tool-output/1.txt"))
                .0,
            Decision::Allow
        );
        // 嵌套的 tool-output 不在例外里（例外按 workspace 相对前缀写死一层）
        assert_eq!(
            policy
                .explain(&Effect::new(FS_READ, "/ws/pkg/.ui/tool-output/1.txt"))
                .0,
            Decision::Deny
        );
    }

    #[test]
    fn truncated_tool_output_is_readable_but_not_writable() {
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        let out = "/ws/.ui/tool-output/0001-shell.txt";
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, out)).0,
            Decision::Allow,
            "截断标记让模型 fs_read 全文，读必须免审"
        );
        assert_eq!(
            policy.explain(&Effect::new(FS_WRITE, out)).0,
            Decision::Deny,
            "往截断缓存里写 = 伪造下一轮的「原文」"
        );
        // 例外只覆盖那一层文件：目录本身、更深一层、蹭名字的兄弟都不放行
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws/.ui/tool-output")).0,
            Decision::Deny
        );
        assert_eq!(
            policy
                .explain(&Effect::new(FS_READ, "/ws/.ui/tool-output/x/y.txt"))
                .0,
            Decision::Deny
        );
        assert_eq!(
            policy
                .explain(&Effect::new(FS_READ, "/ws/.ui/tool-output-x/a.txt"))
                .0,
            Decision::Deny,
            "glob 的 `*` 不吃 `/`，不能匹配到 tool-output-x"
        );
    }

    #[test]
    fn declared_private_paths_get_the_same_shape_as_defaults() {
        let _g = crate::protected::private_test_guard();
        crate::protected::clear_private();
        // 一条多段、一条单段：多段要能在顶层和嵌套位置都被拒，
        // 单段同理。只测"有一条 Deny"不够——形状写错（漏了目录本身）
        // 就会让 `grep(path="vendor/keys")` 落进 `<ws>/**` 的 allow。
        crate::protected::add_private("vendor/keys").unwrap();
        crate::protected::add_private("id_rsa").unwrap();
        let policy = Policy::new(Policy::defaults(Path::new("/ws")), Path::new("/ws"));
        for target in [
            "/ws/vendor/keys",
            "/ws/vendor/keys/deploy.pem",
            "/ws/srv/vendor/keys",
            "/ws/srv/vendor/keys/deploy.pem",
            "/ws/.ssh/id_rsa",
            "/ws/x/.ssh/id_rsa",
        ] {
            for effect in [FS_READ, FS_WRITE] {
                assert_eq!(
                    policy.explain(&Effect::new(effect, target)).0,
                    Decision::Deny,
                    "声明的隐私路径 {}({}) 必须被拒",
                    effect,
                    target
                );
            }
        }
        // 相邻名字不能被误伤：多段声明不是"任何含 vendor 的路径"
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws/vendor/README.md")).0,
            Decision::Allow,
            "vendor 本身不在清单上，只有 vendor/keys 在"
        );
        // 与工具侧对齐：策略说拒的路径，`is_private_path` 也得说挡得住
        for target in ["/ws/vendor/keys/a", "/ws/.ssh/id_rsa"] {
            assert!(
                crate::protected::is_private_path(Path::new("/ws"), Path::new(target)),
                "{} 在策略侧被拒、在工具侧放行——边界取决于请求从哪条路进来",
                target
            );
        }
        crate::protected::clear_private();
    }

    #[test]
    fn a_session_approval_is_visible_as_a_session_approval() {
        // 模式会把 Allow 抬成 Ask；agent 需要分辨「这条 Allow 是基线给的
        // 还是用户刚批准的」，否则 strict 下的「本轮都允许」形同虚设。
        let ws = Path::new("/ws");
        let mut policy = Policy::new(Policy::defaults(ws), ws);
        let plain = Effect::new(FS_WRITE, "/ws/src/a.rs");
        assert_eq!(policy.explain(&plain).0, Decision::Allow);
        assert!(
            !policy.session_approved(&plain),
            "基线放行不算会话授权"
        );
        policy.add_rule(Rule::new(FS_WRITE, "/ws/src/b.rs", Decision::Allow));
        let approved = Effect::new(FS_WRITE, "/ws/src/b.rs");
        assert_eq!(policy.explain(&approved).0, Decision::Allow);
        assert!(policy.session_approved(&approved));
        // 会话授权是原文匹配，不是通配符
        assert!(
            !policy.session_approved(&Effect::new(FS_WRITE, "/ws/src/c.rs")),
            "批准 b.rs 不能顺便批准同目录的 c.rs"
        );
        // decision 不是 Allow 的会话规则不算「已批准」
        policy.add_rule(Rule::new(PROC_EXEC, "rm -rf /", Decision::Deny));
        assert!(
            !policy.session_approved(&Effect::new(PROC_EXEC, "rm -rf /")),
            "拒绝规则不能反过来免掉询问"
        );
    }

    #[test]
    fn session_rules_reset_with_the_run() {
        let ws = Path::new("/ws");
        let mut policy = Policy::new(Policy::defaults(ws), ws);
        let base = policy.rules.len();
        policy.add_rule(
            Rule::new(PROC_EXEC, "git status", Decision::Allow).with_note("本次会话中批准"),
        );
        assert_eq!(
            policy.explain(&Effect::new(PROC_EXEC, "git status")).0,
            Decision::Allow
        );

        // 长驻 worker 换新任务：上一轮的授权不能带过来，基线规则要原样保留。
        policy.reset_session_rules();
        assert_eq!(policy.session_rule_count(), 0);
        assert_eq!(policy.rules.len(), base);
        assert_eq!(
            policy.explain(&Effect::new(PROC_EXEC, "git status")).0,
            Decision::Ask,
            "上一个任务批准的命令必须重新征求同意"
        );
        assert_eq!(
            policy.explain(&Effect::new(FS_READ, "/ws/.ui/journal.jsonl")).0,
            Decision::Deny,
            "重置只裁会话规则，不能把安全边界一起裁掉"
        );
        // 幂等：没有会话规则时再重置一次不能出错
        policy.reset_session_rules();
        assert_eq!(policy.rules.len(), base);
    }
}
