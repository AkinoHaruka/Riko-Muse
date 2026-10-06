//! V2-P1 稳定引用（doc7/05 §3）。
//!
//! 稳定引用由**服务端**生成，模型不得自造。它不是内容指纹：\`claim_sha256\` 随正文变化，
//! 不能当外部地址；这里的 ref 以 (tenant,user,domain,memory_id,version) 寻址，
//! 解析后必须重新按认证 scope 与读域集鉴权。

/// 稳定引用前缀。
pub const MEMORY_REF_PREFIX: &str = "riko://memory/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRef {
    pub tenant_id: String,
    pub user_id: String,
    pub domain_id: String,
    pub memory_id: String,
    /// 生成引用时的版本；正文始终取自规范表当前行，版本仅用于对照。
    pub version: i64,
}

/// 生成稳定引用：\`riko://memory/<tenant>/<user>/<domain>/<memory_id>@<version>\`。
pub fn memory_stable_ref(
    tenant_id: &str,
    user_id: &str,
    domain_id: &str,
    memory_id: &str,
    version: i64,
) -> String {
    format!("{MEMORY_REF_PREFIX}{tenant_id}/{user_id}/{domain_id}/{memory_id}@{version}")
}

/// 解析稳定引用。段数必须恰好为 4（tenant/user/domain/memory_id@version），
/// 任一段为空或版本非正整数即返回 None——不接受「看起来像引用」的任意字符串。
pub fn parse_memory_ref(input: &str) -> Option<MemoryRef> {
    let rest = input.strip_prefix(MEMORY_REF_PREFIX)?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 4 {
        return None;
    }
    let (tenant_id, user_id, domain_id, tail) = (parts[0], parts[1], parts[2], parts[3]);
    if tenant_id.is_empty() || user_id.is_empty() || domain_id.is_empty() {
        return None;
    }
    let (memory_id, version) = tail.rsplit_once('@')?;
    if memory_id.is_empty() {
        return None;
    }
    let version: i64 = version.parse().ok()?;
    if version <= 0 {
        return None;
    }
    Some(MemoryRef {
        tenant_id: tenant_id.to_string(),
        user_id: user_id.to_string(),
        domain_id: domain_id.to_string(),
        memory_id: memory_id.to_string(),
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ref_round_trips() {
        let r = memory_stable_ref("t", "u", "user_main", "01a1-mem", 3);
        assert_eq!(r, "riko://memory/t/u/user_main/01a1-mem@3");
        let parsed = parse_memory_ref(&r).unwrap();
        assert_eq!(parsed.tenant_id, "t");
        assert_eq!(parsed.user_id, "u");
        assert_eq!(parsed.domain_id, "user_main");
        assert_eq!(parsed.memory_id, "01a1-mem");
        assert_eq!(parsed.version, 3);
    }

    #[test]
    fn malformed_refs_are_rejected() {
        for bad in [
            "",
            "01a1-mem",
            "riko://memory/t/u/user_main@3",          // 段数不足
            "riko://memory/t/u/user_main/id@3/extra", // 段数过多
            "riko://memory//u/user_main/id@3",        // 空 tenant
            "riko://memory/t/u/user_main/@3",         // 空 memory_id
            "riko://memory/t/u/user_main/id@0",       // 版本非正
            "riko://memory/t/u/user_main/id@-1",
            "riko://memory/t/u/user_main/id@x",
            "riko://memory/t/u/user_main/id",   // 缺版本
            "http://memory/t/u/user_main/id@1", // 非本 scheme
        ] {
            assert!(parse_memory_ref(bad).is_none(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn max_bytes_hash_is_not_a_ref() {
        // claim_sha256 只是内容指纹，不能当稳定地址。
        let hash = "a".repeat(64);
        assert!(parse_memory_ref(&hash).is_none());
    }
}
