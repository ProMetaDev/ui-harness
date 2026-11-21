//! 契约 1 的延伸：历史可以被重写，但不能被悄悄丢掉。
//!
//! 压缩只做三件事，全部确定性、可复现、不额外调用模型：
//!   1. 保住第一条用户消息（任务锚点）；
//!   2. 把被折叠的较早消息压成一条摘要（条数、工具调用统计、失败次数）；
//!   3. 保住最近 N 条原文。
//!
//! 换来的代价必须记账：`compact` 事件写进 journal，前端据此显示「折叠了多少」。

use crate::budget::estimate_tokens;
use crate::prompt::Message;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct CompactPolicy {
    /// 历史超过这么多 token 就折叠（默认 12k）
    pub max_history_tokens: u64,
    /// 保留最近多少条原文
    pub keep_recent: usize,
    /// 是否保留第一条用户消息作为任务锚点
    pub keep_first_user: bool,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        CompactPolicy {
            max_history_tokens: 12_000,
            keep_recent: 8,
            keep_first_user: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactOutcome {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub dropped: usize,
    pub digest: String,
    pub finished: bool,
    /// `model` = 模型写的摘要；`digest` = 确定性摘要（回落）
    pub strategy: &'static str,
    /// 模型摘要失败时的原因；成功或未启用时为 None
    pub error: Option<String>,
}

/// 模型摘要的能力。失败必须可回落，不能因此让上下文超预算继续跑。
pub trait Summarizer {
    fn summarize(&mut self, dropped: &[Message]) -> Result<String, String>;
}

/// 历史里所有文本（与 prompt 渲染一致，保证度量口径相同）
pub fn history_tokens(history: &[Message]) -> u64 {
    let mut total = 0u64;
    for m in history {
        total += estimate_tokens(&m.content) + 8;
    }
    total
}

fn first_user_index(history: &[Message]) -> Option<usize> {
    history.iter().position(|m| m.role == "user")
}

/// 从工具消息里统计调用次数与失败次数。工具的渲染格式是 `[name] ok|error: …`。
fn tool_stats(msgs: &[Message]) -> Vec<(String, usize, usize)> {
    let mut map: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for m in msgs {
        if m.role != "tool" {
            continue;
        }
        let rest = match m.content.strip_prefix('[') {
            Some(r) => r,
            None => continue,
        };
        let name = match rest.split_once(']') {
            Some((n, _)) => n.trim().to_string(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let entry = map.entry(name).or_insert((0, 0));
        if m.content.contains("] error") {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
    }
    map.into_iter()
        .map(|(name, (ok, fail))| (name, ok, fail))
        .collect()
}

fn build_digest(dropped: &[Message], before_tokens: u64) -> String {
    let stats = tool_stats(dropped);
    let tools = if stats.is_empty() {
        "（没有工具调用）".to_string()
    } else {
        stats
            .iter()
            .map(|(name, ok, fail)| {
                if *fail > 0 {
                    format!("{}×{}（失败 {}）", name, ok + fail, fail)
                } else {
                    format!("{}×{}", name, ok)
                }
            })
            .collect::<Vec<_>>()
            .join("、")
    };

    // 只留计数会丢事实（名字、约束、决定）。把被折叠的用户问题摘出来，
    // 这是最便宜也最有用的一层：不调模型，却能保住"用户要过什么"。
    let asked: Vec<String> = dropped
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| {
            let flat = m.content.replace('\n', " ");
            let cut: String = flat.chars().take(40).collect();
            if flat.chars().count() > 40 {
                format!("{}…", cut)
            } else {
                cut
            }
        })
        .collect();
    let asked_line = if asked.is_empty() {
        String::new()
    } else {
        let tail: Vec<&String> = asked.iter().rev().take(3).rev().collect();
        format!(
            " 用户曾问：{}。",
            tail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" / ")
        )
    };

    let assistant_turns = dropped.iter().filter(|m| m.role == "assistant").count();
    format!(
        "[已折叠 {} 条 · 约 {} tok · assistant {} 次 · 工具 {}。{}原文可查 journal]",
        dropped.len(),
        before_tokens,
        assistant_turns,
        tools,
        asked_line
    )
}

/// 超阈值就折叠一次。返回 None 表示不需要动（或折了也不会变小）。
pub fn compact(history: &mut Vec<Message>, policy: &CompactPolicy) -> Option<CompactOutcome> {
    compact_with(history, policy, None)
}

/// 带摘要器的版本：模型摘要失败就回落到确定性摘要，并把失败原因带出去。
pub fn compact_with(
    history: &mut Vec<Message>,
    policy: &CompactPolicy,
    mut summarizer: Option<&mut dyn Summarizer>,
) -> Option<CompactOutcome> {
    let before = history_tokens(history);
    if before <= policy.max_history_tokens {
        return None;
    }
    // keep_recent 来自配置/CLI：`usize::MAX + 2` 会 debug 溢出 panic，release
    // 里 `len - keep_recent` 下溢后切片越界。先夹到历史长度，语义等价于
    // "全保留" → 不折叠。
    let keep_recent = policy.keep_recent.min(history.len());
    if history.len() <= keep_recent + 2 {
        return None; // 太短，折叠没意义
    }

    // 当前轮的 user message 是任务合同：路径、约束、“不要动 X”都在里面。
    // 不能因为它滑出 keep_recent 窗口就被折成 40 字摘要。折叠范围最多到
    // 最后一条 user message 之前。
    let cut = history.len() - keep_recent;
    let first_user = first_user_index(history);
    let anchor = if policy.keep_first_user {
        first_user.filter(|i| *i < cut)
    } else {
        None
    };
    // 当前轮 user message 必须原样保留，但它后面的工具结果可以折叠。
    // 不能把 cut 直接压到 user 下标：那会把当前轮所有工具结果都留下，
    // 等于拒绝压缩；也不能把它折进 40 字摘要。
    let protect = history
        .iter()
        .rposition(|m| m.role == "user")
        .filter(|i| *i < cut && Some(*i) != anchor);

    let dropped: Vec<Message> = history
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != anchor && Some(*i) != protect && *i < cut)
        .map(|(_, m)| m.clone())
        .collect();

    if dropped.is_empty() {
        return None;
    }

    let digest = build_digest(&dropped, before);
    let (digest, strategy, error) = match summarizer.as_mut() {
        None => (digest, "digest", None),
        Some(s) => match s.summarize(&dropped) {
            Ok(text) if !text.trim().is_empty() => {
                (format!("[模型摘要] {}", text.trim()), "model", None)
            }
            Ok(_) => (digest, "digest", Some("模型摘要为空".to_string())),
            Err(e) => (digest, "digest", Some(e)),
        },
    };
    let mut next: Vec<Message> = Vec::with_capacity(keep_recent + 3);
    if let Some(i) = anchor {
        next.push(history[i].clone());
    }
    if let Some(i) = protect {
        next.push(history[i].clone());
    }
    next.push(Message::note(digest.clone()));
    next.extend(history[cut..].iter().cloned());

    let after = history_tokens(&next);
    // 折叠必须真的变小。摘要本身也有成本，压不动就别动——否则会出现
    // "265 → 288" 这种负收益，越折越大。
    if after + 8 >= before {
        return None;
    }
    *history = next;

    Some(CompactOutcome {
        before_tokens: before,
        after_tokens: after,
        dropped: dropped.len(),
        digest,
        finished: true,
        strategy,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> Message {
        Message::user(t)
    }
    fn agent(t: &str) -> Message {
        Message::assistant(t)
    }
    fn tool(name: &str, ok: bool, body: &str) -> Message {
        Message::tool(name, ok, body)
    }

    struct GoodSummary;
    impl Summarizer for GoodSummary {
        fn summarize(&mut self, _d: &[Message]) -> Result<String, String> {
            Ok("用户在做一个演示；部署分支 release/2026；已完成 greet 修复。".to_string())
        }
    }

    struct BrokenSummary;
    impl Summarizer for BrokenSummary {
        fn summarize(&mut self, _d: &[Message]) -> Result<String, String> {
            Err("provider 500".to_string())
        }
    }

    fn big_history() -> Vec<Message> {
        let mut h = vec![user("第一个任务")];
        for i in 0..30 {
            h.push(agent(&format!("第 {} 段说明 {}", i, "a".repeat(150))));
            h.push(tool("fs_read", i % 7 != 0, &"b".repeat(150)));
        }
        h
    }

    fn tight() -> CompactPolicy {
        CompactPolicy {
            max_history_tokens: 300,
            keep_recent: 4,
            keep_first_user: true,
        }
    }

    #[test]
    fn model_summary_replaces_the_digest() {
        let mut h = big_history();
        let out = compact_with(&mut h, &tight(), Some(&mut GoodSummary)).expect("compact");
        assert_eq!(out.strategy, "model");
        assert!(out.error.is_none());
        assert!(out.digest.starts_with("[模型摘要]"), "{}", out.digest);
        assert!(
            h.iter().any(|m| m.role == "note" && m.content.contains("release/2026")),
            "摘要必须进历史"
        );
    }

    #[test]
    fn model_failure_falls_back_to_deterministic_digest() {
        let mut h = big_history();
        let out = compact_with(&mut h, &tight(), Some(&mut BrokenSummary)).expect("compact");
        assert_eq!(out.strategy, "digest");
        assert_eq!(out.error.as_deref(), Some("provider 500"));
        assert!(out.digest.contains("已折叠"), "回落摘要仍要说明折叠了什么");
        assert!(
            h.iter().any(|m| m.role == "note" && m.content.contains("已折叠")),
            "回落摘要必须进历史，不能因为模型失败就不折叠"
        );
    }

    #[test]
    fn refuses_to_compact_when_it_would_not_shrink() {
        // 只比阈值多一点、消息都很短：折叠后加上摘要反而更大，必须放弃。
        let mut h = vec![user("任务")];
        for i in 0..5 {
            h.push(agent(&format!("短句 {}", i)));
        }
        let before = history_tokens(&h);
        let out = compact(
            &mut h,
            &CompactPolicy {
                max_history_tokens: before.saturating_sub(1),
                keep_recent: 4,
                keep_first_user: true,
            },
        );
        assert!(out.is_none(), "没变短就不该折叠");
        assert_eq!(history_tokens(&h), before, "历史不应被改动");
    }

    #[test]
    fn digest_keeps_what_the_user_asked() {
        let mut h = vec![user("第一个任务")];
        h.push(agent("好的"));
        // 关键事实出现在「后来」的一轮里，必须靠摘要保住（锚点只保住第一条）
        h.push(user("我的名字是小林，请记住"));
        for _ in 0..20 {
            h.push(agent(&"a".repeat(200)));
            h.push(tool("fs_read", true, &"b".repeat(200)));
        }
        compact(
            &mut h,
            &CompactPolicy {
                max_history_tokens: 300,
                keep_recent: 4,
                keep_first_user: true,
            },
        )
        .expect("compact");
        assert!(
            h.iter().any(|m| m.content.contains("小林")),
            "当前轮用户事实必须原样留在历史里，而不是只进 40 字摘要"
        );
    }

    #[test]
    fn does_nothing_below_the_threshold() {
        let mut h = vec![user("hi"), agent("hello")];
        let out = compact(&mut h, &CompactPolicy::default());
        assert!(out.is_none());
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn keeps_anchor_and_recent_window() {
        let mut h = vec![user("原始任务：修好 greet")];
        for i in 0..40 {
            h.push(agent(&format!("第 {} 次说明，占一些 token 用来说明情况。", i)));
            h.push(tool("fs_read", true, &"x".repeat(200)));
        }
        let before = history_tokens(&h);
        let out = compact(
            &mut h,
            &CompactPolicy {
                max_history_tokens: 500,
                keep_recent: 6,
                keep_first_user: true,
            },
        )
        .expect("should compact");

        assert!(out.after_tokens < out.before_tokens);
        assert!(out.dropped > 0);
        assert_eq!(h[0].content, "原始任务：修好 greet", "任务锚点必须留下");
        assert_eq!(h[1].role, "note");
        assert_eq!(h.len(), 1 + 1 + 6, "锚点 + 摘要 + 最近 6 条");
        assert!(h[1].content.contains("fs_read"), "摘要要含工具统计");
        assert!(before > out.after_tokens);
    }

    #[test]
    fn digest_reports_failures() {
        let mut h = vec![user("任务")];
        for _ in 0..30 {
            h.push(tool("shell", true, &"y".repeat(120)));
            h.push(tool("shell", false, &"z".repeat(120)));
            h.push(agent(&"a".repeat(120)));
        }
        let out = compact(
            &mut h,
            &CompactPolicy {
                max_history_tokens: 300,
                keep_recent: 4,
                keep_first_user: true,
            },
        )
        .expect("compact");
        assert!(out.digest.contains("失败"), "摘要应报告失败次数: {}", out.digest);
    }

    #[test]
    fn is_idempotent_when_already_small() {
        let mut h = vec![user("任务")];
        for _ in 0..30 {
            h.push(agent(&"a".repeat(200)));
        }
        let policy = CompactPolicy {
            max_history_tokens: 200,
            keep_recent: 4,
            keep_first_user: true,
        };
        let first = compact(&mut h, &policy).expect("first compaction");
        let second = compact(&mut h, &policy);
        assert!(first.after_tokens < first.before_tokens);
        assert!(
            second.is_none() || second.unwrap().after_tokens < first.after_tokens,
            "二次压缩不能把历史变大"
        );
    }
}
