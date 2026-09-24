//! 检索与上下文编译：FTS5（拉丁文）+ Unicode 二元字索引（中文）+ RRF 融合 + 有界上下文。
//!
//! 卡 0 仅建立 crate；搜索、compose 与索引维护在卡 3 实现。
//! 所有读路径必须按规范表状态过滤（doc/13 §6），不能依赖后台重建。

/// RRF 融合公式 `sum(1/(60+rank))`（doc/13 §6，取自 Hindsight fusion.py 的思想）。
pub fn rrf_score(ranks: &[u32]) -> f64 {
    ranks
        .iter()
        .map(|&rank| 1.0 / (memory_contract::RRF_K + rank as f64))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_matches_reference_formula() {
        // rank 从 1 开始：1/(60+1)
        let s = rrf_score(&[1]);
        assert!((s - 1.0 / 61.0).abs() < 1e-12);
        let s2 = rrf_score(&[1, 3]);
        assert!((s2 - (1.0 / 61.0 + 1.0 / 63.0)).abs() < 1e-12);
    }
}
