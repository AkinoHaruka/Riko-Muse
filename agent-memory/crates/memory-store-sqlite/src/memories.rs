//! 记忆读写：remember/get/search/compose/correct/forget 与派生索引维护。
//! 卡 3 填充实现；本模块同时承载 doc/11 §3 的事务顺序规则。

use crate::Store;

// 占位：卡 3 在此实现 remember/get/search/compose 与 FTS/grams 维护。
// 事务顺序遵守 doc/11 §3：规范事务先提交（memory+evidence+revision+audit+dirty=1），
// 随后独立索引事务维护 FTS/grams，失败保留 dirty 不回滚规范记忆。
impl Store {}
