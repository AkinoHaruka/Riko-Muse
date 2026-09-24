//! 模型提取协议与 `extract_v1` Prompt 的输入输出校验（doc/13 §3–5）。
//!
//! 卡 0 仅建立 crate；模型客户端、窗口任务与候选准入在卡 4 实现。
//! 模型负责提出候选，不负责权限、作用域、状态转换或删除（doc/13 引言）。

use memory_contract::EXTRACT_PROMPT_VERSION;

/// 系统提示词约束（doc/13 §4）。模型必须只引用 role=user 且 source_kind=user 的 event_id；
/// quote 必须逐字连续；没有合格内容时输出空数组；只输出 JSON。
pub const EXTRACT_SYSTEM_PROMPT: &str = "\
从给定对话提取可能对未来 Agent 有持续用途的用户事实、偏好、长期指令和事件。
只引用 role=user 且 source_kind=user 的 event_id。
quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息。
临时请求、假设、引用他人的话、助手推断不要提取。没有合格内容时输出空数组。
只输出 JSON，不输出解释或 Markdown。";

pub fn prompt_version() -> &'static str {
    EXTRACT_PROMPT_VERSION
}
