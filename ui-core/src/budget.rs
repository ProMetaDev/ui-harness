//! Contract 4: one budget, one account.
//!
//! Token estimates are script-aware (CJK ~0.85 tok/char, ASCII ~0.26) instead of
//! "4 chars = 1 token", which silently underprices Chinese. Provider usage
//! replaces the estimate for the same span when it arrives. Exceeding a hard
//! limit is an error the loop stops on — never a silent "carry on over budget".

use crate::json::Json;
use std::fmt;

#[derive(Debug, Clone)]
pub struct BudgetExceeded {
    pub dimension: &'static str,
    pub used: f64,
    pub limit: f64,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "budget exceeded: {} ({:.0}/{:.0})",
            self.dimension, self.used, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Tokens,
    ToolCalls,
    WallMs,
    Usd,
}

impl Dim {
    pub fn name(self) -> &'static str {
        match self {
            Dim::Tokens => "tokens",
            Dim::ToolCalls => "tool_calls",
            Dim::WallMs => "wall_ms",
            Dim::Usd => "usd",
        }
    }
}

const CJK: &[(u32, u32)] = &[
    (0x2E80, 0x2FDF),
    (0x3000, 0x303F),
    (0x3040, 0x30FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xAC00, 0xD7AF),
    (0xF900, 0xFAFF),
    (0xFF00, 0xFFEF),
    (0x20000, 0x2FA1F),
];

fn is_cjk(cp: u32) -> bool {
    CJK.iter().any(|(lo, hi)| cp >= *lo && cp <= *hi)
}

/// Deterministic, replay-stable token estimate. Not exact: it is the number the
/// budget, the snapshot and the replay all agree on, until a provider speaks.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut t = 0.0f64;
    for ch in text.chars() {
        let cp = ch as u32;
        if cp < 0x80 {
            t += 0.26;
        } else if is_cjk(cp) {
            t += 0.85;
        } else {
            t += 0.55;
        }
    }
    t.ceil() as u64
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub tokens: u64,
    pub tool_calls: u64,
    pub wall_ms: u64,
    pub usd: f64,
    /// 单轮的步数上限。默认 24 是为了"别让模型乱跑"，
    /// 但长任务（浸泡、批量处理）需要显式调大：--max-steps。
    pub max_steps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            tokens: 200_000,
            tool_calls: 60,
            wall_ms: 30 * 60 * 1000,
            usd: 2.0,
            max_steps: 24,
        }
    }
}

impl Limits {
    pub fn from_json(j: &Json) -> Limits {
        let d = Limits::default();
        Limits {
            tokens: j.u64_at("maxTokens").unwrap_or(d.tokens),
            tool_calls: j.u64_at("maxToolCalls").unwrap_or(d.tool_calls),
            wall_ms: j.u64_at("maxWallMs").unwrap_or(d.wall_ms),
            usd: j.num_at("maxUsd").unwrap_or(d.usd),
            max_steps: j.u64_at("maxSteps").unwrap_or(d.max_steps as u64) as usize,
        }
    }
}

#[derive(Debug)]
pub struct Budget {
    pub limits: Limits,
    tokens: u64,
    calls: u64,
    usd: f64,
    started_ms: u64,
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Budget {
            limits,
            tokens: 0,
            calls: 0,
            usd: 0.0,
            started_ms: crate::time::now_ms(),
        }
    }

    /// 开始一次新 run：保留限额，清零消耗和墙钟起点。
    ///
    /// 长驻 worker 若不调这个，上一任务的 token/费用会继承到下一任务，
    /// 让预算条和取消后的重试都显示错误数字。
    pub fn reset(&mut self) {
        self.tokens = 0;
        self.calls = 0;
        self.usd = 0.0;
        self.started_ms = crate::time::now_ms();
    }

    pub fn used(&self, dim: Dim) -> f64 {
        match dim {
            Dim::Tokens => self.tokens as f64,
            Dim::ToolCalls => self.calls as f64,
            Dim::WallMs => self.elapsed_ms() as f64,
            Dim::Usd => self.usd,
        }
    }

    fn limit(&self, dim: Dim) -> f64 {
        match dim {
            Dim::Tokens => self.limits.tokens as f64,
            Dim::ToolCalls => self.limits.tool_calls as f64,
            Dim::WallMs => self.limits.wall_ms as f64,
            Dim::Usd => self.limits.usd,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        crate::time::now_ms().saturating_sub(self.started_ms)
    }

    /// Pre-flight check: throw before spending, so the loop never half-spends.
    pub fn assert_room(&self, dim: Dim, amount: f64) -> Result<(), BudgetExceeded> {
        let limit = self.limit(dim);
        let used = self.used(dim) + amount;
        if limit <= 0.0 {
            // 0/负数限额不是「无限」：旧行为让 `--max-tokens 0`、`--max-usd -1`
            // 静默关掉整条预算。没有预算就是一分钱也不许花（fail closed）。
            return if used > 0.0 {
                Err(BudgetExceeded {
                    dimension: dim.name(),
                    used,
                    limit,
                })
            } else {
                Ok(())
            };
        }
        if used > limit {
            return Err(BudgetExceeded {
                dimension: dim.name(),
                used,
                limit,
            });
        }
        Ok(())
    }

    pub fn charge(&mut self, dim: Dim, amount: f64) -> Result<(), BudgetExceeded> {
        self.assert_room(dim, amount)?;
        match dim {
            Dim::Tokens => self.tokens += amount as u64,
            Dim::ToolCalls => self.calls += amount as u64,
            Dim::Usd => self.usd += amount,
            Dim::WallMs => {}
        }
        Ok(())
    }

    /// Provider usage replaces our estimate for the same span of work.
    pub fn reconcile_tokens(&mut self, estimated: u64, actual: u64) {
        if actual == 0 {
            return;
        }
        // i128：u64 -> i64 的 `as` 会把 u64::MAX 变成 -1，一次离谱的 usage
        // 就能把账本"对账"回 0（预算从此永不触发）；近乎 i64::MAX 的回报还会
        // 在 debug 里直接溢出 panic。
        let next = self.tokens as i128 + actual as i128 - estimated as i128;
        self.tokens = next.clamp(0, u64::MAX as i128) as u64;
    }

    pub fn snapshot(&self) -> Json {
        Json::obj(vec![
            (
                "limits",
                Json::obj(vec![
                    ("tokens", Json::from(self.limits.tokens)),
                    ("toolCalls", Json::from(self.limits.tool_calls)),
                    ("wallMs", Json::from(self.limits.wall_ms)),
                    ("usd", Json::Num(self.limits.usd)),
                ]),
            ),
            (
                "used",
                Json::obj(vec![
                    ("tokens", Json::from(self.tokens)),
                    ("toolCalls", Json::from(self.calls)),
                    ("wallMs", Json::from(self.elapsed_ms())),
                    ("usd", Json::Num(self.usd)),
                ]),
            ),
        ])
    }

    /// Fraction consumed of the tightest dimension, for the status line.
    pub fn pressure(&self) -> f64 {
        let mut worst: f64 = 0.0;
        for dim in [Dim::Tokens, Dim::ToolCalls, Dim::WallMs, Dim::Usd] {
            let limit = self.limit(dim);
            if limit > 0.0 {
                worst = worst.max(self.used(dim) / limit);
            }
        }
        worst
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_costs_more_than_ascii_per_character() {
        let zh = estimate_tokens("让函数处理空名字");
        let en = estimate_tokens("abcdefgh");
        assert!(zh > en, "zh={} en={}", zh, en);
    }

    #[test]
    fn charges_and_refuses_over_limit() {
        let mut b = Budget::new(Limits {
            tokens: 100,
            tool_calls: 2,
            wall_ms: 60_000,
            usd: 1.0,
            max_steps: 24,
        });
        b.charge(Dim::Tokens, 60.0).unwrap();
        b.charge(Dim::ToolCalls, 2.0).unwrap();
        let err = b.charge(Dim::ToolCalls, 1.0).unwrap_err();
        assert_eq!(err.dimension, "tool_calls");
        assert!(b.charge(Dim::Tokens, 100.0).is_err());
    }

    #[test]
    fn provider_usage_replaces_estimate() {
        let mut b = Budget::new(Limits::default());
        b.charge(Dim::Tokens, 100.0).unwrap();
        b.reconcile_tokens(100, 40);
        assert_eq!(b.used(Dim::Tokens), 40.0);
    }
}
