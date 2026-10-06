//! V2-S1 记忆域（doc7/04）：身份 scope 与记忆选择是两件事。
//! 纯 Rust 类型，不依赖存储或网络。

/// 缺省主域名。旧数据、未绑定会话、域功能关闭时全部归属此域。
pub const USER_MAIN_DOMAIN: &str = "user_main";

/// 一个请求的域上下文：写域唯一，读域集包含写域。
/// 由服务端从令牌身份、可信宿主会话绑定与已配置策略解析；不接受正文覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainScope {
    /// 写域：本次请求产生的记忆/候选/作业/派生行全部归属该域。
    pub write: String,
    /// 读域集：所有读路径只在这些域内取数（恒含写域）。
    pub read: Vec<String>,
}

impl DomainScope {
    /// 缺省主域上下文：域功能关闭或未配置时的唯一合法值，
    /// 查询结果与 schema 14 行为一致。
    pub fn user_main() -> Self {
        DomainScope {
            write: USER_MAIN_DOMAIN.to_string(),
            read: vec![USER_MAIN_DOMAIN.to_string()],
        }
    }

    /// 显式构造；写域自动并入读域集，读域集去重。
    pub fn new(write: impl Into<String>, read: Vec<String>) -> Self {
        let write = write.into();
        let mut read = read;
        if !read.iter().any(|d| d == &write) {
            read.push(write.clone());
        }
        DomainScope { write, read }
    }

    /// 后台作业上下文：作业行所属域；side 域按 V2 策略继承 user_main 读取。
    pub fn for_job(job_domain: &str) -> Self {
        if job_domain == USER_MAIN_DOMAIN {
            DomainScope::user_main()
        } else {
            DomainScope::new(job_domain, vec![USER_MAIN_DOMAIN.to_string()])
        }
    }

    pub fn allows_read(&self, domain: &str) -> bool {
        self.read.iter().any(|d| d == domain)
    }

    /// V2-S1 读域集闭包（doc7/04 §2.3、§3）：
    /// `{D}` ∪（D 为 side 时 `user_main`）∪ 已授权域 ∪ `{写域}`。
    /// 写域恒并入读域集，保证「写域 ∈ 读域集」不变式；顺序稳定、去重。
    pub fn resolve(selected: &str, selected_is_side: bool, grants: &[String], write: &str) -> Self {
        let mut read: Vec<String> = Vec::new();
        push_unique(&mut read, selected);
        if selected_is_side {
            push_unique(&mut read, USER_MAIN_DOMAIN);
        }
        for g in grants {
            push_unique(&mut read, g);
        }
        push_unique(&mut read, write);
        DomainScope {
            write: write.to_string(),
            read,
        }
    }

    /// 读域集的 JSON 数组，供 `domain_id IN (SELECT value FROM json_each(?N))` 绑定。
    pub fn read_json(&self) -> String {
        let mut out = String::from("[");
        for (i, d) in self.read.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            for ch in d.chars() {
                match ch {
                    '"' | '\\' => out.push('\\'),
                    _ => {}
                }
                out.push(ch);
            }
            out.push('"');
        }
        out.push(']');
        out
    }
}

fn push_unique(read: &mut Vec<String>, domain: &str) {
    if !read.iter().any(|d| d == domain) {
        read.push(domain.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_main_is_single_domain() {
        let d = DomainScope::user_main();
        assert_eq!(d.write, "user_main");
        assert_eq!(d.read, vec!["user_main"]);
        assert!(d.allows_read("user_main"));
        assert!(!d.allows_read("side_a"));
        assert_eq!(d.read_json(), "[\"user_main\"]");
    }

    #[test]
    fn side_inherits_main_and_write_in_read() {
        let d = DomainScope::for_job("side_a");
        assert_eq!(d.write, "side_a");
        assert!(d.allows_read("side_a") && d.allows_read("user_main"));
        assert!(!d.allows_read("side_b"));
        let d2 = DomainScope::new("side_a", vec![]);
        assert_eq!(d2.read, vec!["side_a"]);
    }

    #[test]
    fn resolve_closes_over_main_and_grants() {
        // 主域：只有 user_main。
        let d = DomainScope::resolve("user_main", false, &[], "user_main");
        assert_eq!(d.read, vec!["user_main"]);
        // side A：A + main；写域恒在集合内。
        let d = DomainScope::resolve("side_a", true, &[], "side_a");
        assert_eq!(d.read, vec!["side_a", "user_main"]);
        // side A 另授权 side B，且写域是 main：不变式仍成立。
        let d = DomainScope::resolve("side_a", true, &["side_b".to_string()], "user_main");
        assert_eq!(d.read, vec!["side_a", "user_main", "side_b"]);
        assert!(d.allows_read("side_b") && d.allows_read("user_main"));
        // 去重。
        let d = DomainScope::resolve(
            "side_a",
            true,
            &["user_main".into(), "side_a".into()],
            "side_a",
        );
        assert_eq!(d.read, vec!["side_a", "user_main"]);
    }

    #[test]
    fn read_json_escapes() {
        let d = DomainScope::new("a\"b", vec!["c\\d".into()]);
        assert_eq!(d.read_json(), "[\"c\\\\d\",\"a\\\"b\"]");
    }
}
