//! 全屏帧：把状态渲染成「恰好 height 行、每行恰好 width 显示列」的字符网格。
//!
//! 纯函数，不碰终端。CJK 占两列，所以这里所有宽度都按**显示列**算，
//! 不是按 `chars().count()`——否则中文一多，右边框就会歪。
//!
//! 每行由若干 `Span` 组成，`Span` 带一个语义样式（正常/次要/强调/警告/错误）。
//! 颜色由 `term.rs` 翻译成 ANSI；本模块只负责「哪一段是什么语义」。

use ui_core::time::human_duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Normal,
    Dim,
    Accent,
    Warn,
    Err,
    AccentStrong,
}

#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Span {
        Span {
            text: text.into(),
            style,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub spans: Vec<Span>,
}

impl Row {
    pub fn plain(text: impl Into<String>) -> Row {
        Row {
            spans: vec![Span::new(text, Style::Normal)],
        }
    }

    pub fn of(spans: Vec<Span>) -> Row {
        Row { spans }
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| disp_width(&s.text)).sum()
    }

    /// 补空格到指定列宽；超宽则截断（截断处补 …）。
    pub fn pad_to(&self, width: usize) -> Row {
        let cur = self.width();
        if cur == width {
            return self.clone();
        }
        if cur > width {
            return self.truncate_to(width);
        }
        let mut row = self.clone();
        row.spans.push(Span::new(" ".repeat(width - cur), Style::Normal));
        row
    }

    pub fn truncate_to(&self, width: usize) -> Row {
        if self.width() <= width {
            return self.clone();
        }
        let budget = width.saturating_sub(1);
        let mut out = Vec::new();
        let mut used = 0;
        for span in &self.spans {
            if used >= budget {
                break;
            }
            let mut kept = String::new();
            let mut w = 0;
            for c in span.text.chars() {
                let cw = char_width(c);
                if used + w + cw > budget {
                    break;
                }
                kept.push(c);
                w += cw;
            }
            used += w;
            if !kept.is_empty() {
                out.push(Span::new(kept, span.style));
            }
        }
        out.push(Span::new("…", Style::Dim));
        Row { spans: out }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    Tool,
    Warn,
    Info,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
    /// 第二行缩进内容：工具调用的结果摘要、错误的下一步建议等。
    pub detail: Option<String>,
    /// 失败的调用用错误色，而不是和成功调用一个颜色。
    pub failed: bool,
}

impl Line {
    fn new(kind: Kind, text: String, detail: Option<String>) -> Line {
        Line {
            kind,
            text,
            detail,
            failed: false,
        }
    }

    pub fn user(t: impl Into<String>) -> Line {
        Line::new(Kind::User, t.into(), None)
    }
    pub fn assistant(t: impl Into<String>) -> Line {
        Line::new(Kind::Assistant, t.into(), None)
    }
    pub fn tool_call(name: &str, ok: bool, detail: impl Into<String>) -> Line {
        let mut line = Line::new(
            Kind::Tool,
            format!("{}  [{}]", name, if ok { "ok" } else { "error" }),
            Some(detail.into()),
        );
        line.failed = !ok;
        line
    }
    pub fn warn(t: impl Into<String>) -> Line {
        Line::new(Kind::Warn, t.into(), None)
    }
    pub fn info(t: impl Into<String>) -> Line {
        Line::new(Kind::Info, t.into(), None)
    }

    /// 8 列定宽 ASCII 标签：终端里 CJK 前缀会让列歪，标签必须定宽。
    fn label(&self) -> &'static str {
        match self.kind {
            Kind::User => "[user]  ",
            Kind::Assistant => "[agent] ",
            Kind::Tool => "[tool]  ",
            Kind::Warn => "[warn]  ",
            Kind::Info => "        ",
        }
    }

    fn label_style(&self) -> Style {
        match self.kind {
            Kind::User => Style::Accent,
            Kind::Assistant => Style::Dim,
            Kind::Tool => Style::Dim,
            Kind::Warn => Style::Warn,
            Kind::Info => Style::Dim,
        }
    }

    fn body_style(&self) -> Style {
        if self.failed {
            return Style::Err;
        }
        match self.kind {
            Kind::Warn => Style::Warn,
            Kind::Info => Style::Dim,
            _ => Style::Normal,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub tool: String,
    pub effects: Vec<String>,
    pub args: String,
}

/// 右栏「当前步」的一段快照：谁、多少 tok、多少字节。
#[derive(Debug, Clone)]
pub struct RailSection {
    pub id: String,
    pub owner: String,
    pub tokens: u64,
}

#[derive(Debug, Clone)]
pub struct RailSnapshot {
    pub id: String,
    pub tokens: u64,
    pub bytes: usize,
    pub sections: Vec<RailSection>,
}

impl RailSnapshot {
    pub fn from_snapshot(snapshot: &ui_core::prompt::Snapshot) -> RailSnapshot {
        RailSnapshot {
            id: snapshot.id.clone(),
            tokens: snapshot.tokens,
            bytes: snapshot.bytes,
            sections: snapshot
                .sections
                .iter()
                .map(|s| RailSection {
                    id: s.id.to_string(),
                    owner: s.owner.to_string(),
                    tokens: s.tokens,
                })
                .collect(),
        }
    }
}

/// 一次工具调用的策略结论。`call_id` 用来在用户批准/拒绝后原地改结论。
#[derive(Debug, Clone)]
pub struct RailDecision {
    pub call_id: String,
    pub tool: String,
    pub decision: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct RightRail {
    /// 本轮第几步（每收到一份快照 +1）。
    pub step: usize,
    pub snapshot: Option<RailSnapshot>,
    pub decisions: Vec<RailDecision>,
}

impl RightRail {
    /// 新一轮开始：右栏只讲「当前这一轮」，不把上一轮的决策混进来。
    pub fn begin_turn(&mut self) {
        self.step = 0;
        self.snapshot = None;
        self.decisions.clear();
    }

    pub fn push_decision(
        &mut self,
        call_id: &str,
        tool: &str,
        decision: &str,
        reason: &str,
    ) {
        self.decisions.push(RailDecision {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            decision: decision.to_string(),
            reason: reason.to_string(),
        });
        // 终端一屏放不下太多；保留最近的，避免无限涨。
        if self.decisions.len() > 24 {
            self.decisions.remove(0);
        }
    }

    /// 审批完成后把 `ask` 改成用户的实际结论。
    pub fn resolve(&mut self, call_id: &str, decision: &str) {
        for d in self.decisions.iter_mut().rev() {
            if d.call_id == call_id {
                d.decision = decision.to_string();
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScreenState {
    pub workspace: String,
    pub model: String,
    pub tokens: u64,
    pub token_limit: u64,
    pub calls: u64,
    pub call_limit: u64,
    pub wall_ms: u64,
    pub lines: Vec<Line>,
    /// 从底部往上滚了多少行（0 = 贴底）。
    pub scroll: usize,
    pub approval: Option<Approval>,
    pub input: String,
    pub hint: String,
    /// 右栏：宽终端（≥100 列）才渲染。
    pub rail: RightRail,
}

impl Default for ScreenState {
    fn default() -> Self {
        ScreenState {
            workspace: ".".into(),
            model: "scripted".into(),
            tokens: 0,
            token_limit: 200_000,
            calls: 0,
            call_limit: 60,
            wall_ms: 0,
            lines: Vec::new(),
            scroll: 0,
            approval: None,
            input: String::new(),
            hint: "/status · /policy · /quit".into(),
            rail: RightRail::default(),
        }
    }
}

/// 显示列宽：CJK 与常见全角符号算 2，组合符算 0。
pub fn disp_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if cp < 0x20 {
        // 控制字符不占列；显式换行由 wrap_cols 先行切开
        return 0;
    }
    if (0x0300..=0x036F).contains(&cp) {
        return 0;
    }
    let wide = (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0x303E).contains(&cp)
        || (0x3041..=0x33FF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xA000..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1F64F).contains(&cp)
        || (0x1F900..=0x1F9FF).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp);
    if wide {
        2
    } else {
        1
    }
}

/// 按显示列截断，不切开宽字符；截断时补 `…`。
pub fn truncate_cols(s: &str, max: usize) -> String {
    if disp_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn pad_cols(s: &str, width: usize) -> String {
    let w = disp_width(s);
    if w >= width {
        truncate_cols(s, width)
    } else {
        format!("{}{}", s, " ".repeat(width - w))
    }
}

/// 按显示列换行：先按显式 `\n` 硬切，再对每段做软换行。
pub fn wrap_cols(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for para in s.split('\n') {
        if para.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut w = 0;
        for c in para.chars() {
            let cw = char_width(c);
            if cw == 0 {
                continue;
            }
            if w + cw > width {
                rows.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(c);
            w += cw;
        }
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn rule_row(width: usize) -> Row {
    let body = if width < 2 {
        "─".repeat(width)
    } else {
        format!("├{}┤", "─".repeat(width - 2))
    };
    Row::plain(body).pad_to(width)
}

fn header_rows(state: &ScreenState, width: usize) -> Vec<Row> {
    let brand = Row::of(vec![
        Span::new("U:I", Style::AccentStrong),
        Span::new(format!("  v{}", env!("CARGO_PKG_VERSION")), Style::Dim),
        Span::new("  ·  Unleash your imagination", Style::Dim),
    ]);
    let used = brand.width();
    let ws_label = "ws: ";
    let room = width.saturating_sub(used + ws_label.len() + 2);
    let ws = if room > 8 {
        truncate_cols(&state.workspace, room)
    } else {
        String::new()
    };
    let mut title = brand;
    if !ws.is_empty() {
        title.spans.push(Span::new("   ", Style::Dim));
        title.spans.push(Span::new(ws_label, Style::Dim));
        title.spans.push(Span::new(ws, Style::Normal));
    }

    let status = status_row(state);

    vec![title.pad_to(width), status.pad_to(width), rule_row(width)]
}

fn status_row(state: &ScreenState) -> Row {
    let bar = render_bar(state.tokens, state.token_limit, 8);
    let ratio = if state.token_limit == 0 {
        0.0
    } else {
        state.tokens as f64 / state.token_limit as f64
    };
    let bar_style = if ratio > 0.9 {
        Style::Err
    } else if ratio > 0.75 {
        Style::Warn
    } else {
        Style::Accent
    };
    Row::of(vec![
        Span::new(state.model.clone(), Style::Dim),
        Span::new(" │ tok ", Style::Dim),
        Span::new(state.tokens.to_string(), Style::Normal),
        Span::new(format!("/{}", state.token_limit), Style::Dim),
        Span::new(" ", Style::Normal),
        Span::new(bar, bar_style),
        Span::new(" │ calls ", Style::Dim),
        Span::new(state.calls.to_string(), Style::Normal),
        Span::new(format!("/{}", state.call_limit), Style::Dim),
        Span::new(" │ ", Style::Dim),
        Span::new(human_duration(state.wall_ms), Style::Dim),
        Span::new(" │ ", Style::Dim),
        Span::new("$0.000", Style::Dim),
    ])
}

fn render_bar(used: u64, limit: u64, width: usize) -> String {
    if limit == 0 {
        return "·".repeat(width);
    }
    let ratio = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let mut s = String::new();
    for i in 0..width {
        s.push(if i < filled { '█' } else { '·' });
    }
    s
}

/// 生成整屏。每一行都恰好 `width` 显示列。
const RAIL_WIDTH: usize = 34;
const RAIL_MIN_TOTAL: usize = 100;

pub fn frame(state: &ScreenState, width: usize, height: usize) -> Vec<Row> {
    let width = width.max(20);
    let height = height.max(6);

    // 「当前步」右栏：只在宽终端出现。窄屏宁可不要，也不能挤压正文。
    let show_rail = width >= RAIL_MIN_TOTAL;
    let rail_w = if show_rail { RAIL_WIDTH } else { 0 };
    let main_w = if show_rail {
        width.saturating_sub(rail_w + 1)
    } else {
        width
    };

    let header = header_rows(state, width);

    let input_line = if state.input.is_empty() {
        Row::of(vec![Span::new("› ", Style::Accent)])
    } else {
        Row::of(vec![
            Span::new("› ", Style::Accent),
            Span::new(state.input.clone(), Style::Normal),
        ])
    };
    let footer = vec![
        rule_row(width),
        input_line.pad_to(width),
        Row::plain(state.hint.clone()).pad_to(width),
    ];

    let body_rows = height.saturating_sub(header.len() + footer.len()).max(1);

    // 正文：每条逻辑行按「8 列标签 + 正文」包成显示行；换说话人时留一个空行。
    let body_w = main_w.saturating_sub(8).max(8);
    let mut visual: Vec<Row> = Vec::new();
    let mut prev: Option<Kind> = None;
    for line in &state.lines {
        if let Some(p) = prev {
            if p != line.kind && !(p == Kind::Tool && line.kind == Kind::Tool) {
                visual.push(Row::plain(""));
            }
        }
        prev = Some(line.kind);

        let label = line.label();
        for (i, row) in wrap_cols(&line.text, body_w).iter().enumerate() {
            let lead = if i == 0 { label } else { "        " };
            visual.push(Row::of(vec![
                Span::new(lead, if i == 0 { line.label_style() } else { Style::Normal }),
                Span::new(row.clone(), line.body_style()),
            ]));
        }
        if let Some(detail) = &line.detail {
            for row in wrap_cols(detail, body_w) {
                visual.push(Row::of(vec![
                    Span::new("        ", Style::Dim),
                    Span::new(row, Style::Dim),
                ]));
            }
        }
    }

    let total = visual.len();
    let offset = state.scroll.min(total.saturating_sub(1));
    // 内容不满一屏时顶部对齐（像看日志一样往下长）；满了才取底部窗口。
    let mut body: Vec<Row> = if total <= body_rows {
        let mut v = visual.clone();
        v.resize(body_rows, Row::plain(""));
        v
    } else {
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(body_rows);
        visual[start..end].to_vec()
    };
    body.truncate(body_rows);

    let mut rows: Vec<Row> = Vec::new();
    rows.extend(header);
    if show_rail {
        let rail = rail_rows(&state.rail, state, rail_w, body_rows);
        for (i, row) in body.iter().enumerate() {
            let mut spans = row.pad_to(main_w).spans;
            spans.push(Span::new("│", Style::Dim));
            spans.extend(rail[i].spans.clone());
            rows.push(Row::of(spans));
        }
    } else {
        rows.extend(body.into_iter().map(|r| r.pad_to(width)));
    }
    rows.extend(footer);

    // 审批浮层：不透明，盖住正文中部
    if let Some(approval) = &state.approval {
        let box_w = width.saturating_sub(6).min(64).max(24);
        let content = approval_rows(approval, box_w);
        let box_h = content.len();
        let top = (height.saturating_sub(box_h)) / 2;
        let left = (width - box_w) / 2;
        for (i, row) in content.iter().enumerate() {
            let y = top + i;
            if y >= rows.len() {
                break;
            }
            let mut with_margin = Row::of(vec![Span::new(" ".repeat(left), Style::Normal)]);
            with_margin.spans.extend(row.spans.clone());
            rows[y] = with_margin.pad_to(width);
        }
    }

    rows.truncate(height);
    while rows.len() < height {
        rows.push(Row::plain(" ".repeat(width)));
    }
    rows
}

fn rail_rule(width: usize) -> Row {
    Row::of(vec![Span::new("─".repeat(width), Style::Dim)])
}

fn decision_row(d: &RailDecision, width: usize) -> Row {
    let style = match d.decision.as_str() {
        "deny" => Style::Err,
        "ask" => Style::Warn,
        _ => Style::Dim,
    };
    let mut row = Row::of(vec![
        Span::new(format!(" {:<5} ", d.decision), style),
        Span::new(format!("{:<9}", truncate_cols(&d.tool, 9)), Style::Normal),
    ]);
    let room = width.saturating_sub(row.width() + 1);
    if room > 3 && !d.reason.is_empty() {
        let reason = d.reason.replace('\n', " ");
        row.spans.push(Span::new(
            truncate_cols(reason.trim(), room),
            Style::Dim,
        ));
    }
    row
}

/// 右栏「当前步」：快照分段（契约 1）+ 决策时间线（契约 3）+ 账本。
///
/// 返回恰好 `height` 行、每行 `width` 列。放不下时优先保住结论与账本，
/// 决策从最旧的开始丢，并明确写「更早 N 条」——不假装全都看得见。
fn rail_rows(rail: &RightRail, state: &ScreenState, width: usize, height: usize) -> Vec<Row> {
    let tok = rail
        .snapshot
        .as_ref()
        .map(|s| s.tokens)
        .unwrap_or(state.tokens);
    let title = if rail.step == 0 {
        " 当前步 · 等待第一步".to_string()
    } else {
        format!(" 当前步 {} · {} tok", rail.step, tok)
    };

    let mut top: Vec<Row> = vec![
        Row::plain(""),
        Row::of(vec![Span::new(title, Style::Accent)]),
        rail_rule(width),
    ];
    if let Some(s) = &rail.snapshot {
        top.push(Row::of(vec![Span::new(
            format!(" 快照 {} · {} B", truncate_cols(&s.id, 12), s.bytes),
            Style::Dim,
        )]));
        for section in s.sections.iter().take(6) {
            top.push(Row::of(vec![
                Span::new(
                    format!(" {:<8}", truncate_cols(&section.id, 8)),
                    Style::Normal,
                ),
                Span::new(
                    format!("{:<7}", truncate_cols(&section.owner, 7)),
                    Style::Dim,
                ),
                Span::new(format!("{:>5} tok", section.tokens), Style::Dim),
            ]));
        }
    } else {
        top.push(Row::of(vec![Span::new(
            " 还没有快照：发一条消息后出现",
            Style::Dim,
        )]));
    }
    top.push(rail_rule(width));
    top.push(Row::of(vec![Span::new(" 决策", Style::Dim)]));

    let bottom: Vec<Row> = vec![
        rail_rule(width),
        Row::of(vec![Span::new(" 账本", Style::Dim)]),
        Row::of(vec![Span::new(
            format!(" tok {}/{}", state.tokens, state.token_limit),
            Style::Dim,
        )]),
        Row::of(vec![Span::new(
            format!(" calls {}/{}", state.calls, state.call_limit),
            Style::Dim,
        )]),
        Row::of(vec![Span::new(
            format!(" {} · ${:.3}", human_duration(state.wall_ms), 0.0),
            Style::Dim,
        )]),
    ];

    let room = height.saturating_sub(top.len() + bottom.len());
    let mut out = top;
    if rail.decisions.is_empty() {
        if room > 0 {
            out.push(Row::of(vec![Span::new(
                " 本轮还没有工具调用",
                Style::Dim,
            )]));
        }
    } else if room > 0 {
        let mut shown = rail.decisions.len().min(room);
        let hidden = rail.decisions.len() - shown;
        if hidden > 0 && room >= 2 {
            out.push(Row::of(vec![Span::new(
                format!(" … 更早 {} 条", hidden),
                Style::Dim,
            )]));
            shown = room - 1;
        }
        for d in &rail.decisions[rail.decisions.len() - shown..] {
            out.push(decision_row(d, width));
        }
    }
    // 账本贴底：位置稳定，长决策列表不会把它挤走。
    let bottom_at = height.saturating_sub(bottom.len());
    while out.len() < bottom_at {
        out.push(Row::plain(""));
    }
    out.extend(bottom);

    out.truncate(height);
    while out.len() < height {
        out.push(Row::plain(""));
    }
    out.iter().map(|row| row.pad_to(width)).collect()
}

/// 纯文本版本：测试、`ui preview`、非 tty 输出都用它。
pub fn frame_text(state: &ScreenState, width: usize, height: usize) -> Vec<String> {
    frame(state, width, height)
        .iter()
        .map(|r| r.text())
        .collect()
}

fn approval_rows(a: &Approval, box_w: usize) -> Vec<Row> {
    let inner = box_w.saturating_sub(2);
    let mut out = Vec::new();

    let border = |left: char, right: char| {
        Row::of(vec![
            Span::new(left.to_string(), Style::Accent),
            Span::new("─".repeat(inner), Style::Accent),
            Span::new(right.to_string(), Style::Accent),
        ])
        .pad_to(box_w)
    };

    let line = |text: &str, style: Style| {
        Row::of(vec![
            Span::new("│", Style::Accent),
            Span::new(pad_cols(text, inner), style),
            Span::new("│", Style::Accent),
        ])
        .pad_to(box_w)
    };

    out.push(border('┌', '┐'));
    out.push(line(
        &format!(" 需要批准：{}", a.tool),
        Style::AccentStrong,
    ));
    out.push(line("", Style::Normal));
    out.push(line(" Effect:", Style::Dim));
    for e in &a.effects {
        out.push(line(&format!("   {}", e), Style::Normal));
    }
    out.push(line(" Args (preview):", Style::Dim));
    for l in a.args.lines().take(4) {
        out.push(line(&format!("   {}", l), Style::Normal));
    }
    out.push(line("", Style::Normal));
    out.push(line(" [y] 本次   [a] 本会话总是   [n] 拒绝", Style::Dim));
    out.push(border('└', '┘'));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> ScreenState {
        let mut s = ScreenState::default();
        s.workspace = "demo".into();
        s.model = "scripted-demo".into();
        s.tokens = 1719;
        s.calls = 3;
        s.lines = vec![
            Line::user("让 greet 处理空名字：空字符串时不该拼出 \"hi \"。"),
            Line::assistant("先读一下 greet.js 看现状。"),
            Line::tool_call("fs_read", true, "→ 1  export function greet(name) {"),
            Line::assistant("空字符串会拼出 hi ，改成兜底。"),
            Line::tool_call("fs_edit", true, "→ 已编辑 demo/greet.js（1 处）"),
            Line::warn("历史段已占 144 tok，接近本轮预算的 30%"),
        ];
        s
    }

    #[test]
    fn every_frame_is_exactly_the_requested_size() {
        for (w, h) in [(60, 16), (80, 24), (100, 30), (40, 10)] {
            let rows = frame(&demo(), w, h);
            assert_eq!(rows.len(), h, "height at {}x{}", w, h);
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(row.width(), w, "row {} at {}x{}: {:?}", i, w, h, row.text());
            }
        }
    }

    #[test]
    fn cjk_counts_as_two_columns() {
        assert_eq!(disp_width("abc"), 3);
        assert_eq!(disp_width("中文"), 4);
        assert_eq!(disp_width("a中b"), 4);
        assert_eq!(disp_width("─"), 1);
    }

    #[test]
    fn wraps_on_explicit_newlines() {
        let rows = wrap_cols("第一行\n第二行很长需要换行", 4);
        assert!(rows.len() >= 3, "got {:?}", rows);
        assert!(rows.iter().all(|r| !r.contains('\n')));
        assert_eq!(rows[0], "第一");
        assert_eq!(rows[1], "行");
    }

    #[test]
    fn truncation_never_splits_a_wide_char() {
        let s = truncate_cols("中文字符", 5);
        assert!(disp_width(&s) <= 5, "{:?} -> {}", s, disp_width(&s));
        assert!(s.ends_with('…'));
    }

    #[test]
    fn roles_carry_their_own_style() {
        let rows = frame(&demo(), 80, 24);
        let user = rows
            .iter()
            .find(|r| r.text().starts_with("[user]"))
            .expect("user row");
        assert_eq!(user.spans[0].style, Style::Accent);
        let tool = rows
            .iter()
            .find(|r| r.text().starts_with("[tool]"))
            .expect("tool row");
        assert_eq!(tool.spans[0].style, Style::Dim);
        // 品牌那一行必须是强调色，否则整屏没有视觉落点
        assert_eq!(rows[0].spans[0].style, Style::AccentStrong);
    }

    #[test]
    fn speakers_are_separated_by_a_blank_row() {
        let rows = frame(&demo(), 80, 24);
        let texts: Vec<String> = rows.iter().map(|r| r.text()).collect();
        let user_at = texts.iter().position(|t| t.starts_with("[user]")).unwrap();
        let agent_at = texts.iter().position(|t| t.starts_with("[agent]")).unwrap();
        assert!(
            texts[user_at + 1..agent_at].iter().all(|t| t.trim().is_empty()),
            "换说话人之间应留空行: {:?}",
            &texts[user_at..=agent_at]
        );
    }

    #[test]
    fn approval_overlay_is_drawn_and_stays_inside_the_frame() {
        let mut s = demo();
        s.approval = Some(Approval {
            tool: "fs_write".into(),
            effects: vec!["fs.write /etc/hosts".into()],
            args: "{\n  \"path\": \"/etc/hosts\"\n}".into(),
        });
        let rows = frame(&s, 80, 24);
        assert_eq!(rows.len(), 24);
        let joined: String = rows.iter().map(|r| r.text()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("需要批准"));
        assert!(joined.contains("fs.write /etc/hosts"));
        assert!(joined.contains("[y] 本次"));
        for row in &rows {
            assert_eq!(row.width(), 80);
        }
    }

    fn demo_with_rail() -> ScreenState {
        let mut s = demo();
        s.rail.step = 2;
        s.rail.snapshot = Some(RailSnapshot {
            id: "8f2a1c7d9e0b4a12".into(),
            tokens: 1719,
            bytes: 6840,
            sections: vec![
                RailSection {
                    id: "core".into(),
                    owner: "kernel".into(),
                    tokens: 520,
                },
                RailSection {
                    id: "history".into(),
                    owner: "session".into(),
                    tokens: 980,
                },
            ],
        });
        s.rail.push_decision("c1", "fs_read", "allow", "读取 demo/greet.js");
        s.rail.push_decision("c2", "fs_edit", "allow", "编辑 demo/greet.js");
        s.rail.push_decision("c3", "shell", "ask", "执行 cargo test");
        s
    }

    #[test]
    fn right_rail_appears_only_on_wide_terminals() {
        let wide: String = frame_text(&demo_with_rail(), 120, 30).join("\n");
        assert!(wide.contains("当前步 2"), "{}", wide);
        assert!(wide.contains("快照 8f2a1c7d9"), "{}", wide);
        assert!(wide.contains("history"), "{}", wide);
        assert!(wide.contains("fs_edit"), "{}", wide);
        assert!(wide.contains("cargo test"), "{}", wide);

        let narrow: String = frame_text(&demo_with_rail(), 99, 30).join("\n");
        assert!(!narrow.contains("当前步"), "99 列不应出现右栏:\n{}", narrow);
    }

    #[test]
    fn right_rail_keeps_every_row_exactly_wide() {
        for (w, h) in [(100, 24), (120, 30), (140, 18)] {
            let rows = frame_text(&demo_with_rail(), w, h);
            assert_eq!(rows.len(), h);
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(
                    disp_width(row),
                    w,
                    "row {} at {}x{}: {:?}",
                    i,
                    w,
                    h,
                    row
                );
            }
        }
    }

    #[test]
    fn right_rail_reports_hidden_older_decisions_instead_of_pretending() {
        let mut s = demo_with_rail();
        for i in 0..39 {
            s.rail.push_decision(&format!("x{}", i), "fs_read", "allow", "读取文件");
        }
        s.rail
            .push_decision("x39", "fs_read", "allow", "读取最后一个文件");
        assert_eq!(s.rail.decisions.len(), 24, "内存里只保留最近 24 条");
        let text = frame_text(&s, 120, 24).join("\n");
        assert!(text.contains("更早"), "决策放不下时要明说:\n{}", text);
        // 最新的决策必须在，最旧的可以不在
        assert!(text.contains("读取最后一个文件"));
    }

    #[test]
    fn scrolling_from_the_bottom_keeps_the_newest_lines() {
        let mut s = demo();
        s.scroll = 0;
        let rows = frame(&s, 80, 12);
        let joined: String = rows.iter().map(|r| r.text()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("接近本轮预算"), "newest line must be visible");
    }
}
