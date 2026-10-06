# 08｜实施交接 Prompt

下面的文字可复制给后续 Harness。具体契约以本目录 01—07 为准，源码事实以实施时的 checkout 为准。

```text
你负责 C:\TRAE\Agent-Memory 的 doc5 记忆质量规则实施。先读 AGENTS.md、doc-handoff/08、doc5/README.md 与 01—07，再按 doc5/06 每卡核对当前源码、HEAD 和 git status。用户已暂停真实模型试验：本轮不要调用 SiliconFlow 或其他外部模型；固定候选、临时 SQLite、离线 compose 和本地固定响应可以用于必要验收，但不能写成真实模型质量提升。

先单独修 doc-handoff/08 的两个 doc4 契约偏差，再按 D5-1—D5-6 实施。核心产品决定是：一候选一命题、最短连续原文；有限的第一人称稳定自述可自动 active；第三人、健康、未来/短期状态和宽 quote 自动 held；凭据绝不 active；memory_remember 对健康/第三人要核最新用户消息中的直接保存指令，对时间性内容仍拒绝永久保存。instruction 名额不变，fact/preference 不常驻注入。

新作业固定 extract_v3/admit_v2；旧作业保持原 Prompt 与 admit_v1。新建 0004 迁移，禁止修改 0001—0003。Store::open 会自动升级，真实用户库的 backup/doctor 不能被当成只读操作；用只读快照和副本做演练，不对 dana/realtest 原库自动升级。记忆的 scope、证据、forget 抑制与 DSH 薄适配器边界保持服务端裁决。

按 doc5/07 的正反例逐项验证，尤其验证直接 remember 不能绕过 held 规则、旧作业重试不切换准入版本、跨用户不可见、遗忘不复活。每卡报告代码存在/确定性测试/临时库或 HTTP/真实 DSH/真实模型/部署各档状态；没做就写未验证。不要改 EverOS、hindsight、tencentdb-agent-memory 或 deepseek-harness。只暂存明确本项目路径，禁止 git add .，不得提交密钥、真实用户库或私密正文。
```
