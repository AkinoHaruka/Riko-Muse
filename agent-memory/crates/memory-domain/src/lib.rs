//! 作用域、来源、状态机与字符串规范化规则（doc/04、doc/13）。
//! 纯 Rust，不依赖存储或网络。

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 所有读写的固定作用域（doc/04 §1）。由服务端从令牌解析，不接受正文覆盖。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    pub tenant_id: String,
    pub user_id: String,
}

/// 事件来源（doc/04 §1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub host_id: String,
    pub agent_id: String,
    pub session_id: String,
}

/// 证据引用。start/end 指向存储的 UTF-8 原文；没有可靠跨度时保留整条消息引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub evidence_id: String,
    pub start_byte: Option<u32>,
    pub end_byte: Option<u32>,
}

/// 记忆种类（doc/04 §3）：TencentDB chat L1 与 Hindsight world/experience 的交集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Fact,
    Preference,
    Instruction,
    Episode,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Instruction => "instruction",
            MemoryKind::Episode => "episode",
        }
    }
}

/// memories 状态（doc/04 §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryStatus {
    Active,
    Superseded,
    Expired,
    Forgotten,
}

/// memory_candidates 状态（doc/04 §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStatus {
    Candidate,
    Held,
    Rejected,
}

/// 来源类别（doc/01 §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    UserExplicit,
    AssistantObserved,
    ToolOutput,
    ModelInferred,
    ManualEdit,
}

/// `normalize_v1`：Unicode NFKC → Unicode 小写 → 连续空白折叠为一个空格 → 首尾去空白。
/// 不删除中文标点，不做同义词替换（doc/13 §1）。
pub fn normalize_v1(s: &str) -> String {
    let nfkc: String = s.nfkc().collect();
    fold_whitespace(&nfkc.to_lowercase())
}

/// 仅折叠连续空白为一个空格并去首尾空白（doc/12 §5：claim = 折叠空白后的 quote）。
/// 不做 NFKC、不做小写——保存的 claim 保持用户原话的字面。
pub fn fold_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_ws {
                out.push(' ');
            }
            last_was_ws = true;
        } else {
            out.push(ch);
            last_was_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// `claim_sha256 = SHA256(UTF8(kind + "\0" + normalize_v1(claim)))`（doc/13 §1）。
pub fn claim_sha256(kind: MemoryKind, claim: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(normalize_v1(claim).as_bytes());
    hex::encode(hasher.finalize())
}

/// quote 验证辅助：quote 必须是 content 的连续 UTF-8 原文子串。
/// 返回字节偏移（start_byte, end_byte）。
pub fn find_quote_span(content: &str, quote: &str) -> Option<(usize, usize)> {
    content
        .find(quote)
        .map(|start| (start, start + quote.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_and_folds() {
        assert_eq!(normalize_v1("  我  喜欢　Rust  "), "我 喜欢 rust");
        assert_eq!(normalize_v1("ＡＢＣ　ｄｅｆ"), "abc def");
    }

    #[test]
    fn claim_hash_is_stable() {
        let a = claim_sha256(MemoryKind::Instruction, "以后回答我用中文");
        let b = claim_sha256(MemoryKind::Instruction, " 以后回答我用中文 ");
        assert_eq!(a, b);
        let c = claim_sha256(MemoryKind::Fact, "以后回答我用中文");
        assert_ne!(a, c);
    }

    #[test]
    fn quote_span_finds_contiguous_bytes() {
        let content = "我说：以后回答我用中文，好吗";
        let (s, e) = find_quote_span(content, "以后回答我用中文").unwrap();
        assert_eq!(&content[s..e], "以后回答我用中文");
    }
}
