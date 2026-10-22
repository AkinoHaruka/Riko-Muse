# P4 抽取质量语料标注说明

`p4_extract_quality.tsv` 是 120 条**合成**用户轮次，每行独立标注，不含真实用户正文或真实凭据。10 个 bucket 各 12 条；`claim_points` 是最低限度的语义锚词，以分号分隔，不规定模型必须逐字生成完整句子。`kind` 是期望主类，多命题句允许产生多个 kind。

标注依据：

- `active`：应至少形成一条针对用户本人的可长期复用命题，且所有 `claim_points` 应在本轮 active 命题集合中出现；多命题行保留两个要点。模型不抽取、rewrite 丢掉关键限定或准入门误杀，都计失配。
- `held`：第三人事实与秘密、健康/过敏类政策样本不应有任何 active 命题。对本语料，零候选比 held 更保守，但仍按 disposition 失配记录，避免掩盖抽取漏检。政策门泄漏单独按**任何 active**计，判定不随 disposition 宽松而变化。
- `reject`：假设、举例和本次/今天的临时请求应无可保存的长期命题。`held` 比 active 安全，但仍记为与期望不符。
- `disputed`：条件偏好、语境不完整的自我纠正，以及不明确的历史地点和口语偏好。逐条展示但不并入确定项准确率分母，尤其不把主观选择调成模型已得到的结果。

`p4_quality_baseline.py` 用当前 `memoryd` 在两个全新临时库中运行：每 8 条一窗，15 个窗口，均为用户事件；固定 `extract_v4/admit_v4`，每模型独立串行 worker。窗口划分、事件时间和模型配置记录在脚本中。运行时从环境变量读 endpoint/key，写 0600 临时 key 文件；临时库与 key 在退出时清理。输出 JSON 逐条保留候选状态、原因码和命题，便于以后同脚本复跑。模型有随机性、窗口内可能相互影响；这套标注是可审查基线，不是完整客观金标准。

运行：

```bash
cd /home/hatch/workspace/src/Riko-Muse
cd riko-memory && cargo build --workspace && cd ..
set -a; source ~/.config/test-models/endpoints.conf; source ~/.config/test-models/keys.env; set +a
python3 riko-memory/tests/p4_quality_baseline.py
```
