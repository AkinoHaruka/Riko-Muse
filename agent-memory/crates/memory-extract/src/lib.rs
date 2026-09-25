//! 模型提取协议与 `extract_v1` 候选准入（doc/13 §3–5）。
//!
//! 模型负责提出候选，不负责权限、作用域、状态转换或删除；Rust 内核是唯一提交者。
//! 窗口输入只含当前认证用户的本次窗口事件（doc/13 §4）。

use serde::Deserialize;

/// 系统提示词（doc/13 §4）。响应 Schema 逐字写进提示词——真实模型（尤其 4B 级）
/// 无法从散文约束可靠猜出字段名，缺 schema 会自造字段或包 Markdown 围栏（2026-09-25 实测）。
pub const EXTRACT_SYSTEM_PROMPT: &str = "\
从给定对话提取可能对未来 Agent 有持续用途的用户事实、偏好、长期指令和事件。
只引用 role=user 且 source_kind=user 的 event_id。
quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息。
临时请求、假设、引用他人的话、助手推断不要提取。没有合格内容时输出空数组。
只输出 JSON，不输出解释或 Markdown。
响应必须是如下形状，字段名逐字一致、不增不减；occurred_at、valid_until、confidence 可省略：
{\"candidates\":[{\"source_event_id\":\"<event_id>\",\"quote\":\"<逐字连续原文>\",\"kind\":\"fact|preference|instruction|episode\",\"occurred_at\":null,\"valid_until\":null,\"confidence\":0.9}]}
没有合格内容时输出 {\"candidates\":[]}。";

pub const EXTRACT_PROMPT_VERSION: &str = memory_contract::EXTRACT_PROMPT_VERSION;

/// 模型响应 Schema（doc/13 §4）。confidence 仅作诊断，不能驱动准入。
#[derive(Debug, Clone, Deserialize)]
pub struct Extraction {
    pub candidates: Vec<ModelCandidate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCandidate {
    pub source_event_id: String,
    pub quote: String,
    pub kind: String,
    pub occurred_at: Option<String>,
    pub valid_until: Option<String>,
    pub confidence: Option<f64>,
}

/// 窗口内可供校验的事件（由 store 层加载）。
#[derive(Debug, Clone)]
pub struct WindowEvent {
    pub id: String,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub content: String,
}

/// 准入结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Active,
    Held(&'static str),
    Rejected(&'static str),
}

/// 模型调用错误。
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("模型响应不是合法 JSON")]
    BadJson,
    #[error("模型调用失败: {0}")]
    Transport(String),
    #[error("模型超时")]
    Timeout,
}

/// 一次成功调用的模型输出（doc2/05 §2：usage 有则记录，无则 NULL 不估算）。
#[derive(Debug, Clone)]
pub struct ExtractOutput {
    pub content: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

/// 模型提供者协议。OpenAI 兼容客户端在 memory-server 侧实现并注入 worker。
pub trait ExtractModel: Send {
    /// system=EXTRACT_SYSTEM_PROMPT；user=按 seq 排序的事件 JSON。
    /// 返回原始响应文本与可选用量（由内核解析校验）。
    fn extract(
        &self,
        system: &str,
        user: &str,
    ) -> impl std::future::Future<Output = Result<ExtractOutput, ExtractError>> + Send;
}

/// 解析模型输出为 Extraction。Markdown 代码围栏只做格式归一化（真实 provider
/// 常见行为，提示词压不住），剥除后仍走严格 schema 校验（deny_unknown_fields），
/// 准入闸门不受影响。
pub fn parse_extraction(content: &str) -> Result<Extraction, String> {
    let t = content.trim();
    let stripped = if let Some(rest) = t.strip_prefix("```") {
        // 跳过 ```json 之类的语言标注行；要求结尾有围栏才算包裹。
        let first_break = rest.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &rest[first_break..];
        let body = body.trim_end();
        match body.rfind("```") {
            Some(i) if body[..i].trim().starts_with('{') => body[..i].trim(),
            _ => t,
        }
    } else {
        t
    };
    serde_json::from_str(stripped).map_err(|e| e.to_string())
}

/// doc/13 §5 的确定性准入规则第 1～7 步（第 8、9 步需查库，由 store 层完成）。
/// 依次判断，第一条不满足就停在 held 或 rejected 并写 reason code。
pub fn admit(c: &ModelCandidate, events: &[WindowEvent]) -> Admission {
    use Admission::*;
    // 1. 来源必须属于本窗口、role=user、source_kind=user。
    let Some(ev) = events.iter().find(|e| e.id == c.source_event_id) else {
        return Rejected("BAD_SOURCE");
    };
    if ev.role != "user" || ev.source_kind != "user" {
        return Rejected("BAD_SOURCE");
    }
    // 2. quote 必须是该事件正文的连续原文子串。
    if !ev.content.contains(&c.quote) {
        return Rejected("QUOTE_MISMATCH");
    }
    // 3. quote 规范化后非空、长度 1～512 个 Unicode 标量字符。
    if memory_domain::fold_whitespace(&c.quote).is_empty()
        || c.quote.chars().count() > memory_contract::QUOTE_MAX_CHARS
    {
        return Rejected("INVALID_QUOTE");
    }
    // 5. 语境不确定（保守；不尝试理解全部语言）。
    if context_uncertain(&c.quote) {
        return Held("CONTEXT_UNCERTAIN");
    }
    // 6. 明确表达规则。
    if !explicit_enough(&c.quote) {
        return Held("NOT_EXPLICIT");
    }
    // 7. 敏感内容保守闸门。
    if sensitive(&c.quote) {
        return Held("SENSITIVE");
    }
    Active
}

/// 一次性/假设/转述词（doc/13 §5.5）。
fn context_uncertain(quote: &str) -> bool {
    let q = quote.to_lowercase();
    const ONCE_ZH: [&str; 5] = ["如果", "假如", "比如", "这次", "本次"];
    const ONCE_EN: [&str; 4] = ["if ", "for example", "this time", "just today"];
    if ONCE_ZH.iter().any(|w| q.contains(w)) {
        return true;
    }
    let padded = format!(" {q} ");
    ONCE_EN.iter().any(|w| padded.contains(w))
}

/// 明确表达规则（doc/13 §5.6）：匹配时先对前缀做大小写折叠，保存的 quote 保持原样。
fn explicit_enough(quote: &str) -> bool {
    let q = quote.trim().to_lowercase();
    // 长期指令：含任一指令词。
    const INSTRUCTION_ZH: [&str; 4] = ["以后", "总是", "从现在起", "记住"];
    const INSTRUCTION_EN: [&str; 3] = ["always", "from now on", "remember"];
    if INSTRUCTION_ZH.iter().any(|w| q.contains(w)) {
        return true;
    }
    if INSTRUCTION_EN.iter().any(|w| q.contains(w)) {
        return true;
    }
    // 稳定自我陈述：以任一前缀开始。
    const SELF_ZH: [&str; 5] = ["我叫", "我是", "我住在", "我喜欢", "我不喜欢"];
    const SELF_EN: [&str; 6] =
        ["my name is", "i am a", "i am an", "i live", "i like", "i dislike"];
    if SELF_ZH.iter().any(|p| q.starts_with(p)) {
        return true;
    }
    if SELF_EN.iter().any(|p| q.starts_with(p)) {
        return true;
    }
    // 职业表达：以「我在」开始且前 20 个 Unicode 标量字符中出现「工作」。
    if q.starts_with("我在") {
        let head: String = q.chars().take(20).collect();
        return head.contains("工作");
    }
    false
}

/// 敏感检测（doc/13 §5.7）：明显内容的保守闸门，不声称覆盖所有敏感数据。
/// 手写匹配（避免正则依赖）：关键词（大小写不敏感）→ 可选空白 → : = ： → 可选空白 → 值。
fn sensitive(quote: &str) -> bool {
    let lower = quote.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let bytes_len = chars.len();

    // 模式 1：password / passwd / 密码 → 分隔符 → 非空白值 ≥4
    for key in ["password", "passwd", "密码"] {
        let kc: Vec<char> = key.chars().collect();
        for start in 0..bytes_len.saturating_sub(kc.len() - 1) {
            if chars[start..].starts_with(&kc) {
                if let Some(value_len) = value_after_sep(&chars, start + kc.len()) {
                    if value_len >= 4 {
                        return true;
                    }
                }
            }
        }
    }
    // 模式 2：api key 变体 / token → 分隔符 → [A-Za-z0-9_-] ≥12
    for key in ["api_key", "api-key", "api key", "apikey", "token"] {
        let kc: Vec<char> = key.chars().collect();
        for start in 0..bytes_len.saturating_sub(kc.len() - 1) {
            if chars[start..].starts_with(&kc) {
                if let Some(value_len) = value_after_sep(&chars, start + kc.len()) {
                    if value_len >= 12 {
                        return true;
                    }
                }
            }
        }
    }
    // 模式 3：sk- 后跟 ≥12 个 [A-Za-z0-9_-]
    let sk: Vec<char> = "sk-".chars().collect();
    for start in 0..bytes_len.saturating_sub(sk.len() - 1) {
        if chars[start..].starts_with(&sk) {
            let mut n = 0;
            for &c in &chars[start + sk.len()..] {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    n += 1;
                } else {
                    break;
                }
            }
            if n >= 12 {
                return true;
            }
        }
    }
    // 字面词。
    ["身份证", "银行卡", "诊断", "病历"].iter().any(|w| quote.contains(w))
}

/// 从 pos 起：跳空白 → 若遇 : = ： 则再跳其后的空白 → 数连续非空白字符数（无分隔符则 None）。
fn value_after_sep(chars: &[char], pos: usize) -> Option<usize> {
    let mut i = pos;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    if ![':', '=', '：'].contains(&chars[i]) {
        return None;
    }
    i += 1;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let mut n = 0;
    while i + n < chars.len() && !chars[i + n].is_whitespace() {
        n += 1;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(content: &str) -> WindowEvent {
        WindowEvent {
            id: "e1".into(),
            role: "user".into(),
            source_kind: "user".into(),
            occurred_at: "2026-09-24T12:00:00Z".into(),
            content: content.into(),
        }
    }

    fn cand(quote: &str) -> ModelCandidate {
        ModelCandidate {
            source_event_id: "e1".into(),
            quote: quote.into(),
            kind: "instruction".into(),
            occurred_at: None,
            valid_until: None,
            confidence: Some(0.9),
        }
    }

    #[test]
    fn bad_source_and_quote_mismatch_are_rejected() {
        let events = vec![ev("以后回答我用中文")];
        let mut c = cand("以后回答我用中文");
        c.source_event_id = "nope".into();
        assert_eq!(admit(&c, &events), Admission::Rejected("BAD_SOURCE"));

        let c2 = ModelCandidate { quote: "我喜欢咖啡".into(), ..cand("x") };
        assert_eq!(admit(&c2, &events), Admission::Rejected("QUOTE_MISMATCH"));
    }

    #[test]
    fn assistant_event_cannot_support_candidate() {
        let events = vec![WindowEvent {
            id: "e1".into(),
            role: "assistant".into(),
            source_kind: "assistant".into(),
            occurred_at: "2026-09-24T12:00:00Z".into(),
            content: "用户应该喜欢中文".into(),
        }];
        assert_eq!(admit(&cand("用户应该喜欢中文"), &events), Admission::Rejected("BAD_SOURCE"));
    }

    #[test]
    fn one_shot_request_held() {
        let events = vec![ev("这次翻译成英文")];
        let c = ModelCandidate { kind: "episode".into(), ..cand("这次翻译成英文") };
        assert_eq!(admit(&c, &events), Admission::Held("CONTEXT_UNCERTAIN"));
    }

    #[test]
    fn not_explicit_held() {
        let events = vec![ev("昨天挺累的")];
        let c = ModelCandidate { kind: "episode".into(), ..cand("昨天挺累的") };
        assert_eq!(admit(&c, &events), Admission::Held("NOT_EXPLICIT"));
    }

    #[test]
    fn explicit_instruction_and_self_statement_active() {
        let events = vec![ev("以后回答我用中文")];
        assert_eq!(admit(&cand("以后回答我用中文"), &events), Admission::Active);
        let events2 = vec![ev("我叫洛溪")];
        let c = ModelCandidate { kind: "fact".into(), ..cand("我叫洛溪") };
        assert_eq!(admit(&c, &events2), Admission::Active);
        let events3 = vec![ev("I live in Wuhan")];
        let c3 = ModelCandidate { kind: "fact".into(), ..cand("I live in Wuhan") };
        assert_eq!(admit(&c3, &events3), Admission::Active);
    }

    #[test]
    fn occupation_rule() {
        let events = vec![ev("我在腾讯工作三年了")];
        let c = ModelCandidate { kind: "fact".into(), ..cand("我在腾讯工作三年了") };
        assert_eq!(admit(&c, &events), Admission::Active);
        let events2 = vec![ev("我在家里休息")];
        let c2 = ModelCandidate { kind: "episode".into(), ..cand("我在家里休息") };
        assert_eq!(admit(&c2, &events2), Admission::Held("NOT_EXPLICIT"));
    }

    #[test]
    fn sensitive_held() {
        let events = vec![ev("记住我的密码：abc12345")];
        let c = ModelCandidate { kind: "instruction".into(), ..cand("记住我的密码：abc12345") };
        assert_eq!(admit(&c, &events), Admission::Held("SENSITIVE"));
        // 单独谈论「密码」一词不被阻断。
        let events2 = vec![ev("我不喜欢密码学课")];
        let c2 = ModelCandidate { kind: "preference".into(), ..cand("我不喜欢密码学课") };
        assert_eq!(admit(&c2, &events2), Admission::Active);
    }

    #[test]
    fn json_parse_enforces_schema() {
        let good = r#"{"candidates":[{"source_event_id":"e1","quote":"以后用中文","kind":"instruction","occurred_at":null,"valid_until":null,"confidence":0.9}]}"#;
        let parsed: Extraction = serde_json::from_str(good).unwrap();
        assert_eq!(parsed.candidates.len(), 1);
        let empty = r#"{"candidates":[]}"#;
        assert!(serde_json::from_str::<Extraction>(empty).is_ok());
        let extra = r#"{"candidates":[{"source_event_id":"e1","quote":"q","kind":"fact","occurred_at":null,"valid_until":null,"confidence":1,"evil":1}]}"#;
        assert!(serde_json::from_str::<Extraction>(extra).is_err());
    }

    #[test]
    fn parse_extraction_strips_code_fence_but_keeps_strict_schema() {
        // 真实 provider（SiliconFlow Qwen3.5-4B 实测）即使被明令禁止仍包 ```json 围栏。
        let fenced = "```json\n{\"candidates\":[{\"source_event_id\":\"e1\",\"quote\":\"以后用中文\",\"kind\":\"instruction\"}]}\n```";
        let parsed = parse_extraction(fenced).unwrap();
        assert_eq!(parsed.candidates.len(), 1);
        assert_eq!(parsed.candidates[0].quote, "以后用中文");
        // 干净 JSON 原样通过。
        assert!(parse_extraction("{\"candidates\":[]}").is_ok());
        // 围栏剥除后仍严格校验：自造字段拒绝。
        let fenced_bad = "```json\n{\"candidates\":[{\"source_event_id\":\"e1\",\"fact\":\"改写\"}]}\n```";
        assert!(parse_extraction(fenced_bad).is_err());
        // 纯垃圾仍失败。
        assert!(parse_extraction("不是 JSON").is_err());
    }
}
