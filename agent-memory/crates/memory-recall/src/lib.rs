//! 检索与上下文编译：FTS5（拉丁文）+ Unicode 二元字索引（中文）+ RRF 融合 + 有界上下文。
//!
//! 词法规则（doc/13 §6）：拉丁文按字母/数字切词；中文/非空格文字按连续段的 Unicode
//! 二元字组；1 字查询走有界子串降级。所有读路径按规范表状态过滤。

use memory_domain::normalize_v1;

/// RRF 融合公式 `sum(1/(60+rank))`（doc/13 §6，取自 Hindsight fusion.py 的思想）。
pub fn rrf_score(ranks: &[u32]) -> f64 {
    ranks
        .iter()
        .map(|&rank| 1.0 / (memory_contract::RRF_K + rank as f64))
        .sum()
}

/// 拉丁文切词：连续字母/数字段转小写（doc/13 §6）。
pub fn latin_tokens(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() && !is_cjk(ch) {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 中文/字母数字段的 Unicode 二元字组（char 粒度）。
/// 分段规则：连续汉字为一段，连续字母数字为一段；其他字符为分隔。
/// 段的长度 < 2 时不产出 gram。重复的 gram 由调用方去重（memory_grams 主键亦去重）。
pub fn cjk_bigrams(query: &str) -> Vec<String> {
    let normalized = normalize_v1(query);
    let mut grams = Vec::new();
    let mut seg = String::new();
    let mut seg_kind: Option<bool> = None; // Some(true)=汉字段, Some(false)=字母数字段
    let mut chars = normalized.chars().peekable();
    while let Some(ch) = chars.next() {
        let kind = if is_cjk(ch) {
            Some(true)
        } else if ch.is_alphanumeric() {
            Some(false)
        } else {
            None
        };
        if kind.is_some() && kind == seg_kind {
            seg.push(ch);
        } else {
            flush_segment(&seg, &mut grams);
            seg.clear();
            if let Some(k) = kind {
                seg.push(ch);
                seg_kind = Some(k);
            } else {
                seg_kind = None;
            }
        }
    }
    flush_segment(&seg, &mut grams);
    grams.sort();
    grams.dedup();
    grams
}

fn flush_segment(seg: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = seg.chars().collect();
    if chars.len() < 2 {
        return;
    }
    for w in chars.windows(2) {
        let mut g = String::with_capacity(8);
        g.push(w[0]);
        g.push(w[1]);
        out.push(g);
    }
}

/// 是否汉字（CJK 统一表意文字，含扩展 A 常用区）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}

/// 归一化后只剩 1 个字符（走 1 字有界子串降级路径）。
pub fn is_single_char_query(query: &str) -> bool {
    normalize_v1(query).chars().count() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_matches_reference_formula() {
        let s = rrf_score(&[1]);
        assert!((s - 1.0 / 61.0).abs() < 1e-12);
        let s2 = rrf_score(&[1, 3]);
        assert!((s2 - (1.0 / 61.0 + 1.0 / 63.0)).abs() < 1e-12);
    }

    #[test]
    fn latin_tokens_split_on_punctuation() {
        assert_eq!(
            latin_tokens("My Name is Alice!"),
            vec!["my", "name", "is", "alice"]
        );
        assert_eq!(latin_tokens("中文 english"), vec!["english"]);
    }

    #[test]
    fn bigrams_cover_chinese_and_mixed() {
        let g = cjk_bigrams("我的语言偏好");
        assert!(g.contains(&"我的".to_string()));
        assert!(g.contains(&"语言".to_string()));
        assert!(g.contains(&"偏好".to_string()));
        let g2 = cjk_bigrams("中文english混合");
        assert!(g2.contains(&"中文".to_string()));
        assert!(g2.contains(&"en".to_string()));
    }

    #[test]
    fn single_char_detection() {
        assert!(is_single_char_query("猫"));
        assert!(!is_single_char_query("猫咪"));
    }
}
