# 07｜实施交接 Prompt

下面的文字可直接交给负责下一轮开发的 Harness。它是施工入口，具体规则仍以本目录 01—06 为准。

```text
你现在负责 C:\TRAE\Agent-Memory 的 doc4 可靠性迭代。请先读根 AGENTS.md、doc4/README.md、doc4/01—06，再读每张任务卡指定的当前源码。用户已暂停继续使用真实模型试验，本阶段不要调用 SiliconFlow 或其他真实模型，也不要把固定响应/本机单测说成真实模型验证。

按 D4-0 至 D4-7 连续实施；遇到真正阻碍时报告可复现事实，并继续处理不依赖该阻碍的卡。P0 是队列状态落地、退避、过期 running 恢复、跨 session 公平领取、数值窗口顺序和旧 generation 防提交。不要优先做 fact 自动 active、extract_v3、候选 promote、preference 常驻注入或 embedding，它们在 doc4/05 中有明确决策边界。

三个上游仓库 EverOS/、hindsight/、tencentdb-agent-memory/ 和官方 deepseek-harness/ 只读。agent-memory/ 是唯一实现目录。迁移 0001/0002 不回改；新增字段只用 0003。根目录不要 git add .；doc2/、doc3/ 当前未跟踪，未经单独决定不要顺手暂存。

每卡先核对当前 HEAD 和目标函数，按 doc4/06 的文件范围修改。迁移或协议变动同时更新规范及 README。按卡提供真实编译/本机行为检查输出与 Git 状态；使用临时数据库、固定响应及最小 DSH 适配器检查，不运行外部模型。每卡交接说明代码存在、测试通过、HTTP 通过、DSH 宿主复验、真实模型复验、部署这几档各自状态。不要在没有检查时写“完成”。

特别注意 doc3/C-4 的勘误：当前窗口读取错误会把 job 留在 running，并非自动重试到 dead。任何已 claim 的错误都必须落可诊断状态；一个 session 的阻塞不能让别的 session 饥饿。修复前先设计并保留对应确定性回归场景。
```
