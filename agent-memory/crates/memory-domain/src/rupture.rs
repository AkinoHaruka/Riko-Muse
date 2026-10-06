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
        match best_by_signal
            .iter_mut()
            .find(|b| b.signal == *signal)
        {
            Some(b) if m.start_byte < b.start_byte => *b = m,
            Some(_) => {}
            None => best_by_signal.push(m),
        }
    }
    best_by_signal.sort_by_key(|m| m.start_byte);
    best_by_signal
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
        let corrections: Vec<_> = ms
            .iter()
            .filter(|m| m.signal == "correction")
            .collect();
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
    fn no_match_returns_empty() {
        assert!(rupture_matches("好的，我明白了，明天见。").is_empty());
    }
}
