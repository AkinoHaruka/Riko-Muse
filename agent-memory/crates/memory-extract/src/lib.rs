//! 模型提取协议与 `extract_v1` 候选准入（doc/13 §3–5）。
//!
//! 模型负责提出候选，不负责权限、作用域、状态转换或删除；Rust 内核是唯一提交者。
//! 窗口输入只含当前认证用户的本次窗口事件（doc/13 §4）。

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

/// extract_v1 原始系统提示词（doc/13 §4）。仅供按作业行版本处理历史作业
/// （doc2/05 §3：保留老版本）；对真实推理模型有缺 Schema 的已知缺陷，勿用于新作业。
pub const EXTRACT_SYSTEM_PROMPT_V1: &str = "从给定对话提取可能对未来 Agent 有持续用途的用户事实、偏好、长期指令和事件。
只引用 role=user 且 source_kind=user 的 event_id。
quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息。
临时请求、假设、引用他人的话、助手推断不要提取。没有合格内容时输出空数组。
只输出 JSON，不输出解释或 Markdown。";

/// 当前系统提示词（extract_v2，doc/13 §4）。响应 Schema 逐字写进提示词——真实模型（尤其 4B 级）
/// 无法从散文约束可靠猜出字段名，缺 schema 会自造字段或包 Markdown 围栏（2026-09-25 实测）。
/// 事实类引导与示例：真实模型对平叙事实系统性漏提取（两个独立窗口空候选，2026-09-25
/// 日常观察 F2），对指令/偏好句式正常——示例给出事实也应提取的明确信号。示例内容
/// 不会被误收：quote 须为真实窗口用户消息的连续原文，示例句不在窗口内，准入必拒。
pub const EXTRACT_SYSTEM_PROMPT: &str = "\
从给定对话提取可能对未来 Agent 有持续用途的用户事实、偏好、长期指令和事件。
用户主动陈述的个人情况是典型事实，应当提取：居住地、职业与专业领域、家庭成员及其重要节点、正在使用的工具或技术栈、稳定的生活习惯。
只引用 role=user 且 source_kind=user 的 event_id。
quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息。
临时请求、假设、引用他人的话、助手推断不要提取。没有合格内容时输出空数组。
只输出 JSON，不输出解释或 Markdown。
示例（仅演示形状与逐字要求，勿照抄进结果）：某用户消息的 event_id 为 e1、正文为「我在杭州做后端开发，平时主要写 Rust。」，则应提取 {\"source_event_id\":\"e1\",\"quote\":\"我在杭州做后端开发，平时主要写 Rust\",\"kind\":\"fact\"}。
响应必须是如下形状，字段名逐字一致、不增不减；occurred_at、valid_until、confidence 可省略：
{\"candidates\":[{\"source_event_id\":\"<event_id>\",\"quote\":\"<逐字连续原文>\",\"kind\":\"fact|preference|instruction|episode\",\"occurred_at\":null,\"valid_until\":null,\"confidence\":0.9}]}
没有合格内容时输出 {\"candidates\":[]}。";

pub const EXTRACT_PROMPT_VERSION: &str = memory_contract::EXTRACT_PROMPT_VERSION;

/// extract_v3（doc5/02）：一候选一命题、最短连续原文，与 admit_v2 配套（doc5/03 §2）。
/// 响应 Schema 与 v2 完全一致（严格 schema，额外字段按 BAD_JSON 处理）；输入序列化、
/// 分窗预算与空数组语义均不变。宽 quote 应留作 held，而不是以错粒度激活；
/// 提示词明示不得改写主语、不得为保全主语跨越另一命题。
pub const EXTRACT_SYSTEM_PROMPT_V3: &str = "\
从给定对话提取可能对未来 Agent 有持续用途的用户事实、偏好、长期指令和事件。
每个候选只能包含一个可独立纠错、遗忘或过期的用户命题。
同一条用户消息里有两个各自成立的命题时，输出两条候选：它们可以共用同一个 event_id，
但每条 quote 必须分别是该消息原文中互不重叠的连续片段。
不要为了补全主语把两个命题拼进一条 quote，也不要改写用户原话：
原话是「平时主要写 Rust」时 quote 必须保持原样，不得补写主语变成「我主要写 Rust」。
不确定某个命题能否独立成立时，宁可整句引用，交给内核判定。
用户主动陈述的个人情况是典型事实：居住地、职业与专业领域、正在使用的工具或技术栈、稳定的生活习惯。
只引用 role=user 且 source_kind=user 的 event_id。
quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息。
临时请求、假设、引用他人的话、助手推断不要提取。
只输出 JSON，不输出解释或 Markdown。
示例（仅演示一候选一命题与逐字要求，勿照抄进结果）：某用户消息的 event_id 为 e1、正文为「我在杭州做后端开发。我主要写 Rust。」，则应提取 {\"candidates\":[{\"source_event_id\":\"e1\",\"quote\":\"我在杭州做后端开发\",\"kind\":\"fact\"},{\"source_event_id\":\"e1\",\"quote\":\"我主要写 Rust\",\"kind\":\"fact\"}]}。
响应必须是如下形状，字段名逐字一致、不增不减；occurred_at、valid_until、confidence 可省略：
{\"candidates\":[{\"source_event_id\":\"<event_id>\",\"quote\":\"<逐字连续原文>\",\"kind\":\"fact|preference|instruction|episode\",\"occurred_at\":null,\"valid_until\":null,\"confidence\":0.9}]}
没有合格内容时输出 {\"candidates\":[]}。";

/// 按作业行 prompt_version 分派系统提示词；未知版本返回 None（worker 显式失败，
/// 不用"最新规则"处理旧作业，doc2/05 §3）。extract_v2 字符串与行为冻结（doc5/02 §1）。
pub fn system_prompt_for(version: &str) -> Option<&'static str> {
    match version {
        memory_contract::EXTRACT_PROMPT_VERSION_V1 => Some(EXTRACT_SYSTEM_PROMPT_V1),
        memory_contract::EXTRACT_PROMPT_VERSION_V2 => Some(EXTRACT_SYSTEM_PROMPT),
        memory_contract::EXTRACT_PROMPT_VERSION_V3 => Some(EXTRACT_SYSTEM_PROMPT_V3),
        _ => None,
    }
}

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

/// 窗口事件的模型输入 JSON 形状（doc/13 §4）。服务端分窗预算与 worker 实际发送
/// 必须共用同一 builder，保证 32 KiB 预算按真实模型输入字节计算（doc4/03 §1—2）。
pub fn window_event_json(e: &WindowEvent) -> serde_json::Value {
    serde_json::json!({
        "event_id": e.id, "role": e.role, "source_kind": e.source_kind,
        "time": e.occurred_at, "text": e.content
    })
}

/// 单事件在输入数组中的序列化字节数（含对象本体与分隔逗号；偏保守，宁可早分窗）。
/// 序列化失败返回 usize::MAX，使其按超限处理而非静默塞入窗口。
pub fn serialized_event_size(e: &WindowEvent) -> usize {
    serde_json::to_vec(&window_event_json(e))
        .map(|b| b.len() + 1)
        .unwrap_or(usize::MAX)
}

/// 整窗序列化为模型输入字符串（数组 JSON，与 worker 实际发送逐字节一致）。
pub fn serialize_window_events(events: &[WindowEvent]) -> Result<String, String> {
    let items: Vec<serde_json::Value> = events.iter().map(window_event_json).collect();
    serde_json::to_string(&items).map_err(|e| format!("窗口输入序列化失败: {e}"))
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
///
/// **版本冻结（doc5/03 §1/§5）**：此函数即 `admit_v1`，历史作业（含手动 retry）
/// 一律继续用它，行为不得改变；新规则见 [`admit_v2`]。
pub fn admit_v1(c: &ModelCandidate, events: &[WindowEvent]) -> Admission {
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

/// admit_v2（doc5/03 §2 固定顺序）：返回仅 Active/Held(reason)/Rejected(reason)，
/// 第一项失败决定状态。规则 1—11 在本函数；12—15（抑制源/去重/冲突/提交）在
/// memory-store-sqlite 的 save_candidate。每次先判无权/无效来源，再看正文内容。
pub fn admit_v2(c: &ModelCandidate, events: &[WindowEvent]) -> Admission {
    use Admission::*;
    // 1. 来源必须属于本窗口、role=user、source_kind=user（先于内容，避免跨 scope 泄露）。
    let Some(ev) = events.iter().find(|e| e.id == c.source_event_id) else {
        return Rejected("BAD_SOURCE");
    };
    if ev.role != "user" || ev.source_kind != "user" {
        return Rejected("BAD_SOURCE");
    }
    // 2. quote 是同一来源正文的连续原文。
    if !ev.content.contains(&c.quote) {
        return Rejected("QUOTE_MISMATCH");
    }
    // 3. 规范化为空或超过 512 Unicode 标量字符。
    if memory_domain::fold_whitespace(&c.quote).is_empty()
        || c.quote.chars().count() > memory_contract::QUOTE_MAX_CHARS
    {
        return Rejected("INVALID_QUOTE");
    }
    // 4. 假设、转述、引用、一次性语境（含旧文档有而 v1 码遗漏的「今天先」）。
    if context_uncertain(&c.quote) || c.quote.contains("今天先") {
        return Held("CONTEXT_UNCERTAIN");
    }
    // 5. 可剥离口语前缀与两个独立命题（样本 A06 钉前缀优先记 NON_MINIMAL_QUOTE）。
    if non_minimal_quote(&c.quote) {
        return Held("NON_MINIMAL_QUOTE");
    }
    if multi_claim(&c.quote) {
        return Held("MULTI_CLAIM");
    }
    // 6. fact/preference 缺明确归属主体（第三人主语不在此停，继续到规则 9）。
    if matches!(c.kind.as_str(), "fact" | "preference") && unclear_subject(&c.quote) {
        return Held("UNCLEAR_SUBJECT");
    }
    // 7. 凭据内容：任何路径不得 active。
    if secret_like(&c.quote) {
        return Held("SECRET");
    }
    // 8. 健康/过敏/诊断/病历等敏感个人信息。
    if sensitive_health(&c.quote) {
        return Held("SENSITIVE");
    }
    // 9. 第三人事实、家庭成员信息。
    if has_third_person_marker(&c.quote) {
        return Held("THIRD_PARTY");
    }
    // 10. 未来节点、相对时间或明显短期状态。
    if temporal_marker(&c.quote) {
        return Held("TEMPORAL");
    }
    // 11. kind 与形状不符 → KIND_MISMATCH；未命中允许句式 → NOT_EXPLICIT。
    match explicit_shape(&c.quote) {
        None => Held("NOT_EXPLICIT"),
        Some(shape) if shape.kind() == c.kind.as_str() => Active,
        Some(_) => Held("KIND_MISMATCH"),
    }
}

/// 按作业行 admission_version 分派准入规则（doc5/03 §1）；未知版本返回 None，
/// worker 对其确定性 dead（UNKNOWN_ADMISSION_VERSION），不退回"最新规则"。
pub fn admit_for(
    admission_version: &str,
    c: &ModelCandidate,
    events: &[WindowEvent],
) -> Option<Admission> {
    match admission_version {
        memory_contract::ADMISSION_VERSION_V1 => Some(admit_v1(c, events)),
        memory_contract::ADMISSION_VERSION_V2 => Some(admit_v2(c, events)),
        _ => None,
    }
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

// ---- admit_v2 候选粒度与阻断词判定（doc5/02 §2、doc5/03 §6—7）----
// 以下全部是保守的确定性检查：只负责阻止自动 active，不声称完备的自然语言理解；
// 无法识别的复杂句保持 held。保存的 quote/claim 原文不变，匹配用工作副本。

/// 宽 quote：两个可独立维护的命题（doc5/03 §7）。两个完整子句由 `，,；;。` 分隔
/// 且后句以「我/平时主要写/还/也」开始，即视为第二命题起点。分隔符不含 ASCII '.'，
/// 避免小数与英文专有名词误判；姓名地址中的普通逗号若后句不是命题起点则不判宽。
fn multi_claim(quote: &str) -> bool {
    for sep in ['，', ',', '；', ';', '。'] {
        let mut from = 0;
        while let Some(pos) = quote[from..].find(sep) {
            let abs = from + pos + sep.len_utf8();
            if abs >= quote.len() {
                break;
            }
            let rest = quote[abs..].trim_start();
            if rest.starts_with('我')
                || rest.starts_with("平时主要写")
                || rest.starts_with('还')
                || rest.starts_with('也')
            {
                return true;
            }
            from = abs;
        }
    }
    false
}

/// 可剥离口语前缀（doc5/03 §7）：quote 含该前缀则 held:NON_MINIMAL_QUOTE；
/// 模型可另行返回不含前缀的逐字 span，Rust 不改写 quote。
fn non_minimal_quote(quote: &str) -> bool {
    let lower = quote.trim().to_lowercase();
    ["提醒一下，", "顺便说一句，", "对了，", "by the way, "]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// 第一人称完整起点（中文按字面，英文大小写折叠）。
fn has_first_person_subject(q_trim_lower: &str) -> bool {
    q_trim_lower.starts_with('我')
        || q_trim_lower.starts_with("i ")
        || q_trim_lower.starts_with("i'")
        || q_trim_lower.starts_with("my ")
}

/// 第三人标记（doc5/03 §7 有限词表，字面包含）。只用于保守拦截；
/// 「我们家小孩」不能算第一人称用户事实。
fn has_third_person_marker(quote: &str) -> bool {
    ["我姐", "我哥", "我妈", "我爸", "我们家小孩", "小孩", "孩子", "家人", "同事"]
        .iter()
        .any(|w| quote.contains(w))
}

/// 缺明确归属主体（doc5/03 §7）：fact/preference 片段既无第一人称主语也无第三人
/// 主语 → held:UNCLEAR_SUBJECT，不能用同事件别处的「我」补 claim。
/// 已识别第三人主语的不在此停（继续到 THIRD_PARTY）。
fn unclear_subject(quote: &str) -> bool {
    let q = quote.trim().to_lowercase();
    !has_first_person_subject(&q) && !has_third_person_marker(quote)
}

/// 未来节点/短期状态/一次性语境词（doc5/03 §7）。命中即 held:TEMPORAL（CONTEXT_UNCERTAIN
/// 已先在 admit_v2 规则 4 处理「如果/假如/比如/这次/本次/今天先」）。
fn temporal_marker(quote: &str) -> bool {
    ["今年", "明年", "下周", "下个月", "即将", "准备", "今天", "最近", "暂时", "坏了"]
        .iter()
        .any(|w| quote.contains(w))
}

/// 健康敏感词（doc5/03 §7 有限词表）。命中即 held:SENSITIVE；不声称覆盖所有健康表达。
fn sensitive_health(quote: &str) -> bool {
    ["过敏", "诊断", "病历", "疾病", "吃药", "服药", "身份证", "银行卡"]
        .iter()
        .any(|w| quote.contains(w))
}

/// 凭据内容（doc5/03 §7）：继承 v1 sensitive() 的密码/token/API key/sk- 值形态，
/// 命中即 held:SECRET——自动提取与 memory_remember 直写均不得 active（doc5/04 §2）。
/// 与 v1 的差异只是不再包含健康/证件字面词（后者归 SENSITIVE）；不把值打印到日志。
fn secret_like(quote: &str) -> bool {
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
    false
}

/// admit_v2 明确句式（doc5/03 §6 最小受支持集合）。quote 匹配到哪个 kind 的形状。
/// 对 store 层公开：属性冲突键提取复用同一形状识别，不跨层复制名单（doc5/03 §7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplicitShape {
    Instruction,
    Preference,
    FactName,
    FactResidence,
    FactOccupation,
    FactPrimaryPractice,
}

impl ExplicitShape {
    /// 该形状所属的候选 kind（doc5/03 §6）：fact 形状包含 name/residence/occupation/
    /// primary-practice；kind 与形状冲突时记 KIND_MISMATCH，不自动改 kind。
    pub fn kind(self) -> &'static str {
        match self {
            ExplicitShape::Instruction => "instruction",
            ExplicitShape::Preference => "preference",
            ExplicitShape::FactName
            | ExplicitShape::FactResidence
            | ExplicitShape::FactOccupation
            | ExplicitShape::FactPrimaryPractice => "fact",
        }
    }
}

/// 职业/角色尾词（doc5/03 §6）：`我在X做Y` 的 Y 必须以其一结尾；`我是X` 仅当 X
/// 以其一结尾才可自动 active。集中一处，不跨层复制。
const OCCUPATION_TAILS: [&str; 8] = [
    "开发", "工程师", "设计师", "教师", "老师", "研究员", "产品经理", "数据分析",
];

/// 句末一个标点（doc5/03 §6：每项在去除句末一个标点后匹配）。
const TRAILING_PUNCT: [char; 12] = ['。', '，', '；', '！', '？', '.', ',', ';', '!', '?', '~', '～'];

/// X 的受限片段校验：非空、1—64 Unicode 标量、不含会引入第二命题/新句的标点。
fn bounded_fragment(x: &str) -> bool {
    let n = x.chars().count();
    n > 0
        && n <= 64
        && !x.chars().any(|c| {
            matches!(c, '，' | ',' | '；' | ';' | '。' | '.' | '!' | '？' | '?' | '！' | '\n' | '\r')
        })
}

/// 对工作副本执行 trim + NFKC + 小写，并去除句末一个标点（doc5/03 §6 前言）。
/// 只用于匹配；保存的 quote/claim 原文不变。
fn normalize_shape_input(quote: &str) -> String {
    let nfkc: String = quote.trim().nfkc().collect();
    let mut s = nfkc.to_lowercase();
    if let Some(last) = s.chars().last() {
        if TRAILING_PUNCT.contains(&last) {
            s.pop();
        }
    }
    s
}

/// admit_v2 的明确句式识别（doc5/03 §6 最小受支持集合）。返回命中的形状；
/// 未命中返回 None（调用方按 kind 记 NOT_EXPLICIT 或 KIND_MISMATCH）。
/// 匹配基于规范化工作副本；句式之外默认 held，不用规则填补所有语言。
pub fn explicit_shape(quote: &str) -> Option<ExplicitShape> {
    let s = normalize_shape_input(quote);
    // 长期指令：以固定指令词起始，后面必须有非空要求（doc5/03 §6）。
    for p in ["以后", "从现在起", "请总是", "请记住", "记住", "always", "from now on", "remember"] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::Instruction);
            }
        }
    }
    // 稳定偏好（英文要求 "i like "/"i dislike " 带空格；过去式等形状外一律不放行）。
    for p in ["我喜欢", "我不喜欢", "i like ", "i dislike "] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::Preference);
            }
        }
    }
    // fact/name。
    for p in ["我叫", "my name is "] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::FactName);
            }
        }
    }
    // fact/residence。「我在杭州做后端开发」不说明住在杭州——residence 只认我住在。
    for p in ["我住在", "i live in "] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::FactResidence);
            }
        }
    }
    // fact/primary-practice：引用自身必须含「我」（我主要写X / 我平时主要写X）。
    for p in ["我主要写", "我平时主要写"] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::FactPrimaryPractice);
            }
        }
    }
    // fact/occupation-new：我在X做Y，Y 以职业尾词结尾（doc5/03 §6）。
    if let Some(rest) = s.strip_prefix("我在") {
        if let Some(do_pos) = rest.rfind("做") {
            let (x, y) = (&rest[..do_pos], &rest[do_pos + "做".len()..]);
            if !x.is_empty() && OCCUPATION_TAILS.iter().any(|t| y.ends_with(t)) {
                return Some(ExplicitShape::FactOccupation);
            }
        }
        // fact/occupation-old：我在X工作（沿旧形状；X 非空受限）。
        if let Some(work_pos) = rest.find("工作") {
            if work_pos > 0 {
                return Some(ExplicitShape::FactOccupation);
            }
        }
    }
    // 我是X：仅在 X 以职业尾词结尾时算 occupation（doc5/03 §6）；其余不放行。
    if let Some(rest) = s.strip_prefix("我是") {
        if OCCUPATION_TAILS.iter().any(|t| rest.ends_with(t)) {
            return Some(ExplicitShape::FactOccupation);
        }
    }
    // 英文 I am a/an X 沿用旧形状。
    for p in ["i am a ", "i am an "] {
        if let Some(rest) = s.strip_prefix(p) {
            if bounded_fragment(rest) {
                return Some(ExplicitShape::FactOccupation);
            }
        }
    }
    None
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
        assert_eq!(admit_v1(&c, &events), Admission::Rejected("BAD_SOURCE"));

        let c2 = ModelCandidate { quote: "我喜欢咖啡".into(), ..cand("x") };
        assert_eq!(admit_v1(&c2, &events), Admission::Rejected("QUOTE_MISMATCH"));
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
        assert_eq!(admit_v1(&cand("用户应该喜欢中文"), &events), Admission::Rejected("BAD_SOURCE"));
    }

    #[test]
    fn one_shot_request_held() {
        let events = vec![ev("这次翻译成英文")];
        let c = ModelCandidate { kind: "episode".into(), ..cand("这次翻译成英文") };
        assert_eq!(admit_v1(&c, &events), Admission::Held("CONTEXT_UNCERTAIN"));
    }

    #[test]
    fn not_explicit_held() {
        let events = vec![ev("昨天挺累的")];
        let c = ModelCandidate { kind: "episode".into(), ..cand("昨天挺累的") };
        assert_eq!(admit_v1(&c, &events), Admission::Held("NOT_EXPLICIT"));
    }

    #[test]
    fn explicit_instruction_and_self_statement_active() {
        let events = vec![ev("以后回答我用中文")];
        assert_eq!(admit_v1(&cand("以后回答我用中文"), &events), Admission::Active);
        let events2 = vec![ev("我叫洛溪")];
        let c = ModelCandidate { kind: "fact".into(), ..cand("我叫洛溪") };
        assert_eq!(admit_v1(&c, &events2), Admission::Active);
        let events3 = vec![ev("I live in Wuhan")];
        let c3 = ModelCandidate { kind: "fact".into(), ..cand("I live in Wuhan") };
        assert_eq!(admit_v1(&c3, &events3), Admission::Active);
    }

    #[test]
    fn occupation_rule() {
        let events = vec![ev("我在腾讯工作三年了")];
        let c = ModelCandidate { kind: "fact".into(), ..cand("我在腾讯工作三年了") };
        assert_eq!(admit_v1(&c, &events), Admission::Active);
        let events2 = vec![ev("我在家里休息")];
        let c2 = ModelCandidate { kind: "episode".into(), ..cand("我在家里休息") };
        assert_eq!(admit_v1(&c2, &events2), Admission::Held("NOT_EXPLICIT"));
    }

    #[test]
    fn sensitive_held() {
        let events = vec![ev("记住我的密码：abc12345")];
        let c = ModelCandidate { kind: "instruction".into(), ..cand("记住我的密码：abc12345") };
        assert_eq!(admit_v1(&c, &events), Admission::Held("SENSITIVE"));
        // 单独谈论「密码」一词不被阻断。
        let events2 = vec![ev("我不喜欢密码学课")];
        let c2 = ModelCandidate { kind: "preference".into(), ..cand("我不喜欢密码学课") };
        assert_eq!(admit_v1(&c2, &events2), Admission::Active);
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

    #[test]
    fn v3_prompt_dispatched_by_version_and_v1_v2_frozen() {
        // doc5/02 §1：v1/v2 字符串原样保留；v3 按作业行版本分派。
        assert_eq!(system_prompt_for("extract_v1"), Some(EXTRACT_SYSTEM_PROMPT_V1));
        assert_eq!(system_prompt_for("extract_v2"), Some(EXTRACT_SYSTEM_PROMPT));
        assert_eq!(system_prompt_for("extract_v3"), Some(EXTRACT_SYSTEM_PROMPT_V3));
        assert_eq!(system_prompt_for("extract_v0"), None);
        // v3 契约要点写进提示词：一候选一命题、逐字连续、不改写主语、空数组合法。
        assert!(EXTRACT_SYSTEM_PROMPT_V3.contains("一个可独立纠错、遗忘或过期的用户命题"));
        assert!(EXTRACT_SYSTEM_PROMPT_V3.contains("不得补写主语"));
        assert!(EXTRACT_SYSTEM_PROMPT_V3.contains("{\"candidates\":[]}"));
        // 响应 Schema 与 v2 完全一致：v3 仍按同一严格 schema 解析。
        let parsed = parse_extraction(
            "{\"candidates\":[{\"source_event_id\":\"e1\",\"quote\":\"q\",\"kind\":\"fact\"}]}",
        )
        .unwrap();
        assert_eq!(parsed.candidates.len(), 1);
    }

    #[test]
    fn multi_claim_detects_two_independent_propositions() {
        // doc5/02 §2 / doc5/03 §7：后句以我/平时主要写/还/也开始 → 宽 quote。
        assert!(multi_claim("我在杭州做后端开发，平时主要写 Rust"));
        assert!(multi_claim("我叫洛溪，我住在杭州"));
        assert!(multi_claim("我喜欢 Rust；还喜欢 Go"));
        assert!(multi_claim("我在杭州做后端开发。我主要写 Rust"));
        // 姓名地址中的普通逗号不是命题起点；ASCII '.' 不是分隔符（小数不拆）。
        assert!(!multi_claim("我在杭州，滨江区上班"));
        assert!(!multi_claim("我用的还是 3.5 版本"));
        // 但真有第二命题时（即使句中含小数）仍判宽。
        assert!(multi_claim("版本 3.5 发布了，我主要用 Rust"));
        assert!(!multi_claim("我在杭州做后端开发"));
    }

    #[test]
    fn non_minimal_prefix_held_not_rewritten() {
        // doc5/02 §2：口语前缀与命题可分 → held；Rust 不改写 quote。
        assert!(non_minimal_quote("提醒一下，我在杭州做后端开发"));
        assert!(non_minimal_quote("对了，我对花生过敏"));
        assert!(non_minimal_quote("By The Way, I like Rust"));
        assert!(!non_minimal_quote("我在杭州做后端开发"));
    }

    #[test]
    fn unclear_subject_and_third_party_markers() {
        // doc5/03 §7：无第一人称且无第三人主语 → UNCLEAR_SUBJECT；
        // 第三人标记命中 → 不算 unclear（继续到 THIRD_PARTY）。
        assert!(unclear_subject("平时主要写 Rust"));
        assert!(unclear_subject("老笔记本电池坏了"));
        assert!(!unclear_subject("我主要写 Rust"));
        assert!(!unclear_subject("我们家小孩今年九月上小学"));
        assert!(!unclear_subject("我姐在成都教书"));
        // 第三人词表（有限集合，保守拦截）。
        assert!(has_third_person_marker("我们家小孩今年九月上小学一年级"));
        assert!(has_third_person_marker("我姐在成都教书"));
        assert!(!has_third_person_marker("我在杭州做后端开发"));
    }

    #[test]
    fn temporal_and_health_markers() {
        // doc5/03 §7：未来/短期词与健康词的有限集合。
        assert!(temporal_marker("我今年九月开始新工作"));
        assert!(temporal_marker("老笔记本电池坏了"));
        assert!(temporal_marker("我下周去上海"));
        assert!(!temporal_marker("我在杭州做后端开发"));
        assert!(sensitive_health("我对花生过敏"));
        assert!(sensitive_health("我有哮喘诊断"));
        assert!(!sensitive_health("我喜欢用暗色主题写代码"));
    }

    #[test]
    fn explicit_shapes_minimal_supported_set() {
        // doc5/03 §6 最小受支持集合（正例）。
        assert_eq!(explicit_shape("以后回答请用中文"), Some(ExplicitShape::Instruction));
        assert_eq!(explicit_shape("请记住每天备份"), Some(ExplicitShape::Instruction));
        assert_eq!(explicit_shape("我喜欢用暗色主题写代码"), Some(ExplicitShape::Preference));
        assert_eq!(explicit_shape("I like Rust"), Some(ExplicitShape::Preference));
        assert_eq!(explicit_shape("我叫洛溪"), Some(ExplicitShape::FactName));
        assert_eq!(explicit_shape("My name is 洛溪。"), Some(ExplicitShape::FactName));
        assert_eq!(explicit_shape("我住在杭州"), Some(ExplicitShape::FactResidence));
        assert_eq!(explicit_shape("I live in Hangzhou."), Some(ExplicitShape::FactResidence));
        assert_eq!(explicit_shape("我在杭州做后端开发"), Some(ExplicitShape::FactOccupation));
        assert_eq!(explicit_shape("我是软件工程师"), Some(ExplicitShape::FactOccupation));
        assert_eq!(explicit_shape("I am a software engineer"), Some(ExplicitShape::FactOccupation));
        assert_eq!(explicit_shape("我在腾讯工作三年了"), Some(ExplicitShape::FactOccupation));
        assert_eq!(explicit_shape("我主要写 Rust"), Some(ExplicitShape::FactPrimaryPractice));
        assert_eq!(explicit_shape("我平时主要写 Rust"), Some(ExplicitShape::FactPrimaryPractice));
        // 反例（doc5/03 §6 边界）。
        assert_eq!(explicit_shape("我在家做饭"), None, "做饭不是职业尾词");
        assert_eq!(explicit_shape("我是刚吃完饭"), None, "我是X 过宽，非职业尾词不放行");
        assert_eq!(explicit_shape("平时主要写 Rust"), None, "缺主语片段不是 explicit 形状");
        assert_eq!(explicit_shape("昨天挺累的"), None);
        // X 受限：空、超长、含第二命题标点。
        assert_eq!(explicit_shape("我喜欢"), None);
        assert_eq!(explicit_shape("我喜欢 Rust，也喜欢 Go"), None, "X 含第二命题标点不放行");
        let long_x = format!("我喜欢{}", "好".repeat(65));
        assert_eq!(explicit_shape(&long_x), None, "X 超 64 字符不放行");
    }

    #[test]
    fn shape_normalization_folds_case_and_trailing_punct_only() {
        // doc5/03 §6：匹配用 trim/NFKC/大小写折叠 + 去句末一个标点；不删中间词或否定词。
        assert_eq!(explicit_shape("I Like Rust."), Some(ExplicitShape::Preference));
        assert_eq!(explicit_shape("  我不喜欢加班  "), Some(ExplicitShape::Preference));
        // 否定词是形状的一部分：剥掉就反义，绝不匹配正向形状。
        assert_eq!(normalize_shape_input("我不喜欢加班"), "我不喜欢加班");
    }

    #[test]
    fn admit_v2_matrix_auto_extract() {
        // doc5/07 A 组自动提取样本（纯函数级；A20—A26 查库规则在 store 层测试）。
        let f = |quote: &str, kind: &str| {
            let events = vec![ev(quote)];
            let c = ModelCandidate { kind: kind.into(), ..cand(quote) };
            admit_v2(&c, &events)
        };
        use Admission::{Active, Held};
        assert_eq!(f("我在杭州做后端开发", "fact"), Active, "A01");
        assert_eq!(f("我主要写 Rust", "fact"), Active, "A02");
        assert_eq!(
            f("我在杭州做后端开发，平时主要写 Rust", "fact"),
            Held("MULTI_CLAIM"),
            "A03"
        );
        assert_eq!(f("我在杭州做后端开发", "fact"), Active, "A04（同 A01）");
        assert_eq!(f("平时主要写 Rust", "fact"), Held("UNCLEAR_SUBJECT"), "A05");
        assert_eq!(
            f("提醒一下，我在杭州做后端开发", "fact"),
            Held("NON_MINIMAL_QUOTE"),
            "A06"
        );
        assert_eq!(f("我在杭州做后端开发", "fact"), Active, "A07（同 A01）");
        assert_eq!(f("我喜欢用暗色主题写代码", "preference"), Active, "A08");
        assert_eq!(f("今天先用暗色主题", "preference"), Held("CONTEXT_UNCERTAIN"), "A09");
        assert_eq!(f("以后回答请用中文", "instruction"), Active, "A10");
        assert_eq!(
            f("我们家小孩今年九月上小学一年级", "fact"),
            Held("THIRD_PARTY"),
            "A11：固定顺序先记第三人"
        );
        assert_eq!(f("我对花生过敏", "fact"), Held("SENSITIVE"), "A12");
        assert_eq!(f("我姐在成都教书", "fact"), Held("THIRD_PARTY"), "A13");
        assert_eq!(f("老笔记本电池坏了", "fact"), Held("UNCLEAR_SUBJECT"), "A14");
        assert_eq!(f("我在家做饭", "fact"), Held("NOT_EXPLICIT"), "A15");
        assert_eq!(f("我今年九月开始新工作", "fact"), Held("TEMPORAL"), "A16");
        assert_eq!(f("我叫洛溪", "fact"), Active, "A17");
        assert_eq!(f("如果我住在成都就好了", "fact"), Held("CONTEXT_UNCERTAIN"), "A18");
        assert_eq!(f("我的 API key：sk-0123456789abcdef", "fact"), Held("SECRET"), "A19");
        assert_eq!(f("我喜欢暗色主题", "fact"), Held("KIND_MISMATCH"), "A27");
        // 我以后... 不因内部含「以后」被当作指令（doc5/03 §6 instruction 边界）。
        assert_eq!(f("我以后都回答中文", "instruction"), Held("NOT_EXPLICIT"));
    }


    #[test]
    fn admit_v2_source_and_quote_gates() {
        // A20/A21：来源与逐字闸门在 v2 顺序最前；长度上限沿既有规则。
        let events = vec![ev("我在杭州做后端开发"), ev2_assistant()];
        let mut c = cand("我在杭州做后端开发");
        c.kind = "fact".into();
        c.source_event_id = "e2".into();
        assert_eq!(admit_v2(&c, &events), Admission::Rejected("BAD_SOURCE"), "A20");
        let c2 = ModelCandidate {
            quote: "我在杭州担任后端工程师".into(),
            ..cand("x")
        };
        assert_eq!(
            admit_v2(&c2, &events),
            Admission::Rejected("QUOTE_MISMATCH"),
            "A21"
        );
        // 长度上限：quote 须先满足逐字（规则 2），再判 512 上限（规则 3）。
        let long_content = "长".repeat(513);
        let events_long = vec![ev(&long_content)];
        let c3 = ModelCandidate { quote: long_content.clone(), ..cand("x") };
        assert_eq!(admit_v2(&c3, &events_long), Admission::Rejected("INVALID_QUOTE"));
    }

    fn ev2_assistant() -> WindowEvent {
        WindowEvent {
            id: "e2".into(),
            role: "assistant".into(),
            source_kind: "assistant".into(),
            occurred_at: "2026-09-24T12:00:00Z".into(),
            content: "用户应该喜欢中文".into(),
        }
    }

    #[test]
    fn admit_v1_and_v2_diverge_on_same_candidate() {
        // doc5/03 §5：旧作业保持旧判定、新作业用新规则——同一候选两版结果不同。
        // v1 无职业尾词/主要实践形状：A01/A02 在 admit_v1 下 held:NOT_EXPLICIT。
        let events = vec![ev("我在杭州做后端开发。我主要写 Rust。")];
        let c1 = ModelCandidate { kind: "fact".into(), ..cand("我在杭州做后端开发") };
        assert_eq!(admit_v1(&c1, &events), Admission::Held("NOT_EXPLICIT"));
        assert_eq!(admit_v2(&c1, &events), Admission::Active);
        let c2 = ModelCandidate { kind: "fact".into(), ..cand("我主要写 Rust") };
        assert_eq!(admit_v1(&c2, &events), Admission::Held("NOT_EXPLICIT"));
        assert_eq!(admit_v2(&c2, &events), Admission::Active);
        // 分派：未知版本 None（worker 确定性 dead）。
        assert_eq!(admit_for("admit_v1", &c1, &events), Some(Admission::Held("NOT_EXPLICIT")));
        assert_eq!(admit_for("admit_v2", &c1, &events), Some(Admission::Active));
        assert_eq!(admit_for("admit_v0", &c1, &events), None);
    }
}
