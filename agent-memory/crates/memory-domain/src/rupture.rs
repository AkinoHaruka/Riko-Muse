//! doc7（Riko-Muse）M2：rupture（纠正/裂痕）信号的确定性匹配规则。
//! 纯 Rust，不依赖存储或网络；与 normalize_v1 同层。
//! 规则清单见 doc7/01 §2；修改清单须新建 RUPTURE_CUES_V2，禁止原地改 V1。

/// rupture 信号匹配结果：字节 span 指向存储的 UTF-8 原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuptureMatch {
    pub signal: &'static str,
    pub cue: &'static str,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// RUPTURE_CUES_V1（doc7/01 §2，冻结清单）：(字面量, 信号名)。
/// 设计取向宁窄勿宽：只收直接纠正/不满/边界表述；误报由 repair 线程人工关线兜底。
pub const RUPTURE_CUES_V1: &[(&str, &str)] = &[
    ("不对", "correction"),
    ("不是这样", "correction"),
    ("不是的", "correction"),
    ("你听错了", "correction"),
    ("你说错了", "correction"),
    ("你记错了", "correction"),
    ("你理解错", "correction"),
    ("你又", "recurrence"),
    ("别这样", "boundary"),
    ("不要再", "boundary"),
];

/// 扫描 `RUPTURE_CUES_V1`：每个字面量取首个出现位置；同一信号多条命中只保留最早一条
/// （一个信号一轮一次）。按信号名去重后返回按 start_byte 升序的结果。
pub fn rupture_matches(content: &str) -> Vec<RuptureMatch> {
    let mut best_by_signal: Vec<RuptureMatch> = Vec::new();
    for (cue, signal) in RUPTURE_CUES_V1 {
        let Some(start) = content.find(cue) else {
            continue;
        };
        let m = RuptureMatch {
            signal,
            cue,
            start_byte: start,
            end_byte: start + cue.len(),
        };
        match best_by_signal.iter_mut().find(|b| b.signal == *signal) {
            Some(b) if m.start_byte < b.start_byte => *b = m,
            Some(_) => {}
            None => best_by_signal.push(m),
        }
    }
    best_by_signal.sort_by_key(|m| m.start_byte);
    best_by_signal
}

/// V2 检测器版本号（doc7/08 §3）。历史事件保留 \`rupture_v1\`，统计不跨版本同比。
pub const DETECTOR_VERSION_V1: &str = "rupture_v1";
pub const DETECTOR_VERSION_V2: &str = "rupture_v2";

/// RUPTURE_CUES_V2 = V1 ∪ 漏检表述（doc-handoff/25 的 FN：真实抱怨没有 cue）。
/// V1 清单保持冻结，V2 只新增，不改 V1 语义。
pub const RUPTURE_CUES_V2: &[(&str, &str)] = &[
    ("不对", "correction"),
    ("不是这样", "correction"),
    ("不是的", "correction"),
    ("你听错了", "correction"),
    ("你说错了", "correction"),
    ("你记错了", "correction"),
    ("你理解错", "correction"),
    ("你说错", "correction"),
    ("你搞错", "correction"),
    ("你弄错", "correction"),
    ("你又", "recurrence"),
    ("别这样", "boundary"),
    ("不要再", "boundary"),
];

/// cue 指向谁（doc7/08 §3）。只有 \`agent_correction\` 会开线或强化 repair 线程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuptureTarget {
    AgentCorrection,
    SelfCorrection,
    ThirdParty,
    Neutral,
}

impl RuptureTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            RuptureTarget::AgentCorrection => "agent_correction",
            RuptureTarget::SelfCorrection => "self_correction",
            RuptureTarget::ThirdParty => "third_party",
            RuptureTarget::Neutral => "neutral",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedMatch {
    pub signal: &'static str,
    pub cue: &'static str,
    pub start_byte: usize,
    pub end_byte: usize,
    pub target: RuptureTarget,
}

/// 第三人引语标记：这些出现即认为 cue 是在转述别人。
const THIRD_PARTY_QUOTES: &[&str] = &[
    "他说",
    "她说",
    "他们说",
    "她们说",
    "别人说",
    "某人说",
    "他告诉",
    "她告诉",
    "他提到",
    "她提到",
    "他认为",
    "她认为",
    "他说的",
    "她说的",
];

/// 自我修正标记：cue 说的是用户自己错，不是 Agent 错（V13 的 FP 修复）。
const SELF_CORRECTION_MARKS: &[&str] = &[
    "我说错",
    "我记错",
    "我搞错",
    "我弄错",
    "我理解错",
    "是我说错",
    "是我记错",
    "是我搞错",
    "是我不对",
    "我错了",
    "我错了的",
];

/// 取 cue 前后各 24 个字符的窗口（不按逗号切，否则「不对，是我记错了」会被切成两段）。
fn window_around(content: &str, start: usize, end: usize) -> String {
    const PAD: usize = 24;
    let before: String = content[..start]
        .chars()
        .rev()
        .take(PAD)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = content[end..].chars().take(PAD).collect();
    // 窗口必须包含 cue 本身：像「你记错了」这种 cue 自带第二人称，
    // 若只看前后文，cue 在句首时就会漏判成 neutral。
    format!("{before}{}{after}", &content[start..end])
}

/// \`rupture_v2\` 分类（doc7/08 §3）：只看 cue 附近窗口与先后关系，纯函数、无模型。
///
/// 顺序：第三人引语 → 自我修正 → 第二人称（对 Agent）→ 中性。
/// 顺序本身就是语义：转述与自我修正都要**先于**「出现你就算 Agent 错」。
pub fn classify_target(content: &str, start: usize, end: usize) -> RuptureTarget {
    let w = window_around(content, start, end);
    if THIRD_PARTY_QUOTES.iter().any(|q| w.contains(q)) {
        return RuptureTarget::ThirdParty;
    }
    if SELF_CORRECTION_MARKS.iter().any(|q| w.contains(q)) {
        return RuptureTarget::SelfCorrection;
    }
    if w.contains('你') || w.contains('您') {
        return RuptureTarget::AgentCorrection;
    }
    RuptureTarget::Neutral
}

/// 扫描 \`RUPTURE_CUES_V2\` 并逐条分类。按信号名去重保留最早一条（与 V1 同语义）。
pub fn rupture_matches_v2(content: &str) -> Vec<ClassifiedMatch> {
    let mut best: Vec<ClassifiedMatch> = Vec::new();
    for (cue, signal) in RUPTURE_CUES_V2 {
        let Some(start) = content.find(cue) else {
            continue;
        };
        let end = start + cue.len();
        let m = ClassifiedMatch {
            signal,
            cue,
            start_byte: start,
            end_byte: end,
            target: classify_target(content, start, end),
        };
        match best.iter_mut().find(|b| b.signal == *signal) {
            Some(b) if m.start_byte < b.start_byte => *b = m,
            Some(_) => {}
            None => best.push(m),
        }
    }
    best.sort_by_key(|m| m.start_byte);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_correction_with_byte_span() {
        let content = "你记错了，我说的是十点。";
        let ms = rupture_matches(content);
        assert_eq!(ms.len(), 1);
        let m = &ms[0];
        assert_eq!(m.signal, "correction");
        assert_eq!(m.cue, "你记错了");
        assert_eq!(&content[m.start_byte..m.end_byte], "你记错了");
    }

    #[test]
    fn same_signal_keeps_earliest_occurrence_only() {
        let content = "不对，这样不对。";
        let ms = rupture_matches(content);
        let corrections: Vec<_> = ms.iter().filter(|m| m.signal == "correction").collect();
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].start_byte, 0);
    }

    #[test]
    fn distinct_signals_both_reported() {
        let content = "你又这样，别这样了。";
        let ms = rupture_matches(content);
        let mut signals: Vec<_> = ms.iter().map(|m| m.signal).collect();
        signals.sort();
        assert_eq!(signals, vec!["boundary", "recurrence"]);
    }

    #[test]
    fn v2_separates_self_correction_from_agent_correction() {
        // V13 的 FP：自我纠正不得报成 Agent rupture。
        let ms = rupture_matches_v2("不对，是我记错了，应该是十点。");
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].target, RuptureTarget::SelfCorrection);
        // 真·对 Agent 的纠正仍然是 agent_correction。
        let ms = rupture_matches_v2("你记错了，我说的是十点。");
        assert_eq!(ms[0].target, RuptureTarget::AgentCorrection);
    }

    #[test]
    fn v2_catches_the_missing_complaint_phrasing() {
        // V13 的 FN：doc-handoff/25 的「上次你说错导致损失」在 V1 清单里漏检。
        assert!(rupture_matches("上次你说错导致损失").is_empty());
        let ms = rupture_matches_v2("上次你说错导致损失");
        assert!(!ms.is_empty(), "V2 必须能检出这条真实抱怨");
        assert_eq!(ms[0].target, RuptureTarget::AgentCorrection);
    }

    #[test]
    fn v2_marks_quotes_and_neutral_talk() {
        let ms = rupture_matches_v2("他说你记错了，其实没错。");
        assert_eq!(ms[0].target, RuptureTarget::ThirdParty);
        let ms = rupture_matches_v2("这个方案不对，我们再想想。");
        assert_eq!(ms[0].target, RuptureTarget::Neutral);
    }

    #[test]
    fn no_match_returns_empty() {
        assert!(rupture_matches("好的，我明白了，明天见。").is_empty());
    }
}
