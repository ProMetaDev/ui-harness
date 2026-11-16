//! 模式：同一套内核，不同的「默认打扰程度」。
//!
//! 关键约束：模式只调整**决策的严格度**，不改变日志、账本、效果声明的格式。
//! 所以 quiet 模式跑出来的一轮，事后可以用 strict 模式重判——这是 `ui audit --as` 的基础。

use crate::policy::Decision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 安静：能自决的都自决，只拦「策略明确拒绝」的动作
    Quiet,
    /// 标准：写文件放行、执行命令要问
    Standard,
    /// 严格：写文件和执行都要问，只有读放行
    Strict,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Standard
    }
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "quiet" | "auto" | "安静" => Some(Mode::Quiet),
            "standard" | "default" | "标准" => Some(Mode::Standard),
            "strict" | "audit" | "严格" => Some(Mode::Strict),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Quiet => "quiet",
            Mode::Standard => "standard",
            Mode::Strict => "strict",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Mode::Quiet => "安静：不打扰，可能被拒的动作也会被拦",
            Mode::Standard => "标准：写文件放行，执行命令要问",
            Mode::Strict => "严格：写入与执行都要问，只有读取放行",
        }
    }

    /// 把策略给出的决策，按模式上调或下调。
    ///
    /// 只能往「更严格」或「按模式定义」的方向调：
    /// - Quiet：Ask → Allow（Deny 永远不动，硬边界不能被模式放开）
    /// - Standard：不变
    /// - Strict：Allow → Ask，只留纯读类放行
    pub fn adjust(self, kind: &str, decision: Decision) -> Decision {
        // Deny 是硬边界：任何模式都不能把它变成允许
        if decision == Decision::Deny {
            return decision;
        }
        match self {
            Mode::Quiet => match decision {
                Decision::Ask => Decision::Allow,
                other => other,
            },
            Mode::Standard => decision,
            Mode::Strict => {
                let readonly = kind == "fs.read" || kind == "fs.search";
                match decision {
                    Decision::Allow if !readonly => Decision::Ask,
                    other => other,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_is_never_relaxed_by_any_mode() {
        for mode in [Mode::Quiet, Mode::Standard, Mode::Strict] {
            assert_eq!(
                mode.adjust("fs.write", Decision::Deny),
                Decision::Deny,
                "{} 不能把硬边界放开",
                mode.name()
            );
        }
    }

    #[test]
    fn quiet_turns_asks_into_allow() {
        assert_eq!(Mode::Quiet.adjust("proc.exec", Decision::Ask), Decision::Allow);
        assert_eq!(Mode::Quiet.adjust("fs.write", Decision::Allow), Decision::Allow);
    }

    #[test]
    fn strict_escalates_writes_and_execs_but_not_reads() {
        assert_eq!(Mode::Strict.adjust("fs.write", Decision::Allow), Decision::Ask);
        assert_eq!(Mode::Strict.adjust("proc.exec", Decision::Allow), Decision::Ask);
        assert_eq!(Mode::Strict.adjust("fs.read", Decision::Allow), Decision::Allow);
        assert_eq!(Mode::Strict.adjust("fs.search", Decision::Allow), Decision::Allow);
    }

    #[test]
    fn standard_changes_nothing() {
        assert_eq!(Mode::Standard.adjust("fs.write", Decision::Allow), Decision::Allow);
        assert_eq!(Mode::Standard.adjust("proc.exec", Decision::Ask), Decision::Ask);
    }
}
