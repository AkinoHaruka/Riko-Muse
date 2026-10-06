//! SQLite 规范存储：连接管理、有序迁移、principals（doc/11）。
//!
//! SQLite 是首版唯一规范存储；WAL、外键、busy timeout 在打开时开启；
//! 所有迁移在事务内执行并记录 `schema_migrations` checksum。

use std::fs;
use std::path::{Path, PathBuf};

use memory_domain::ScopeKey;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库打开失败: {0}")]
    Open(#[from] rusqlite::Error),
    #[error(
        "当前构建缺少 ENABLE_FTS5，无法创建 memory_fts；请调整 Rust 构建特性，不能静默退化为无索引"
    )]
    Fts5Missing,
    #[error("迁移目录不可读: {0}")]
    MigrationsIo(String),
    #[error("迁移 {name} 内容已改变: 数据库记录 {recorded}，当前 {current}。禁止修改已发布的迁移")]
    MigrationChecksum {
        name: String,
        recorded: String,
        current: String,
    },
    #[error("迁移文件版本跳跃或重复: {0}")]
    MigrationOrder(String),
    #[error("extraction_jobs 存在 (tenant,user,host,session,through) 重复作业，停止迁移 0003（未删行、未做任何 DDL）：{ids}")]
    MigrationJobConflict { ids: String },
    #[error("principal ({tenant}, {user}) 已存在，拒绝覆盖")]
    PrincipalExists { tenant: String, user: String },
    #[error("principal ({tenant}, {user}) 不存在")]
    PrincipalNotFound { tenant: String, user: String },
    #[error("令牌输出文件已存在，拒绝覆盖: {0}")]
    TokenFileExists(String),
    #[error("写令牌文件失败: {0}")]
    TokenFileIo(String),
    #[error("事件键已存在但内容哈希不同（拒绝静默改写证据）")]
    EventConflict,
    #[error("证据不存在或不属于当前 scope")]
    EvidenceNotFound,
    #[error("记忆不存在或不属于当前 scope")]
    MemoryNotFound,
    #[error("引用的用户证据不是该会话最新用户事件")]
    StaleUserEvidence,
    #[error("quote 不是原文连续子串")]
    QuoteMismatch,
    #[error("窗口/状态冲突（through_event_seq 越界、乱序 flush 或窗口超限）")]
    StateConflict,
    #[error("提取窗口超过事件数/字节上限（同输入重试不变，确定性失败）")]
    WindowTooLarge,
    #[error("执行权已失效（claim_generation 不匹配或状态非 running），本次写入未生效")]
    StaleClaim,
    #[error("作业不存在或不属于当前 scope")]
    JobNotFound,
    #[error("版本冲突（乐观锁）")]
    VersionConflict,
    #[error("目标含糊：最近用户消息未明确指认该记忆")]
    AmbiguousTarget,
    #[error("Soul 正文超过 2000 个 Unicode 标量字符，拒绝导入")]
    SoulBodyTooLong,
    #[error("agent_id 不合法（须为 1—256 字符）")]
    InvalidAgentId,
    #[error("同一幂等键曾以不同请求体使用（IDEMPOTENCY_CONFLICT）")]
    IdempotencyConflict,
    #[error("幂等键不合法（1—128 个 ASCII [A-Za-z0-9._-]）")]
    InvalidIdempotencyKey,
    #[error("问题键不合法（1—64 个 ASCII [a-z0-9_]）")]
    InvalidQuestionKey,
    #[error("问题正文须 1—200 个 Unicode 标量字符")]
    InvalidQuestionText,
    #[error("页面字段不合法（title 1—80、正文 1—1200、来源非空）")]
    InvalidPageField,
    #[error("Dream 子 Agent 已达到本 job 的只读调用预算")]
    DreamReadBudgetExceeded,
    #[error("Dream adjudication 未完成每个候选的语义搜索")]
    DreamSearchIncomplete,
    #[error("问题不存在或不属于当前 scope")]
    QuestionNotFound,
    #[error("页面不存在或不属于当前 scope")]
    PageNotFound,
    #[error("输入已失效（来源版本/状态变化），整批不发布")]
    StaleInput,
    #[error("adjudication 输出必须对每个冻结候选恰好包含一项裁决")]
    InvalidAdjudicationCoverage,
    #[error("修复线程不存在或不属于当前 scope")]
    ThreadNotFound,
    #[error("记忆域不存在或不属于当前 scope")]
    DomainNotFound,
    #[error("记忆域已关闭，不能再作为读/写/绑定目标")]
    DomainClosed,
    #[error("user_main 是保留域名，不能创建或关闭")]
    DomainReserved,
    #[error("域名不合法（1—64 个 ASCII [A-Za-z0-9_-]，且不能是 user_main）")]
    InvalidDomainId,
    #[error("会话已绑定到其他域，拒绝静默改绑")]
    DomainBindingConflict,
    #[error("时间溢出: {0}")]
    Time(String),
}

pub mod adjudication;
pub mod alignment;
pub mod consolidation_jobs;
#[cfg(test)]
mod d65_tests;
#[cfg(test)]
mod d67_tests;
#[cfg(test)]
mod d68_tests;
#[cfg(test)]
mod d69_tests;
pub mod derived;
pub mod diagnostics;
pub mod domains;
pub mod dream_jobs;
pub mod dream_read;
pub mod evidence;
pub mod explain;
pub mod jobs;
pub mod lifecycle;
pub mod memories;
#[cfg(test)]
mod muse_tests;
pub mod pages;
#[cfg(test)]
mod probes;
pub mod purge;
pub mod resident;
pub mod semantic_index;
pub mod soul;
#[cfg(test)]
mod v2_d1_tests;
#[cfg(test)]
mod v2_domain_tests;
#[cfg(test)]
mod v2_p1_tests;

pub use diagnostics::{CandidateDetail, CandidateListItem, JobDetail, JobDoctorStats, JobListItem};
pub use evidence::IngestOutcome;
pub use jobs::{CandidateOutcome, FailOutcome, FlushOutcome, JobRow};
pub use memories::{
    has_forget_cue, has_history_cue, ComposeResult, CorrectOutcome, CorrectRequest, ForgetOutcome,
    ForgetRequest, MemoryRow, RememberOutcome, SearchHit,
};

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

pub(crate) fn now_rfc3339() -> Result<String, StoreError> {
    Ok(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// 面向 server/CLI 的当前 UTC RFC3339 时间（诊断展示与读路径 `now` 参数）。
pub fn now_rfc3339_pub() -> Result<String, StoreError> {
    now_rfc3339()
}

/// 迁移描述：版本号来自文件名前缀，sha256 为文件内容哈希。
struct Migration {
    version: u32,
    name: String,
    sql: String,
    sha256: String,
}

fn load_migrations(dir: &Path) -> Result<Vec<Migration>, StoreError> {
    let mut out = Vec::new();
    let entries =
        fs::read_dir(dir).map_err(|e| StoreError::MigrationsIo(format!("{dir:?}: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::MigrationsIo(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| StoreError::MigrationsIo(format!("非法迁移文件名: {path:?}")))?
            .to_string();
        let version: u32 = stem
            .split('_')
            .next()
            .and_then(|p| p.parse().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| StoreError::MigrationOrder(stem.clone()))?;
        let sql = fs::read_to_string(&path)
            .map_err(|e| StoreError::MigrationsIo(format!("读取 {path:?} 失败: {e}")))?;
        let sha256 = hex::encode(Sha256::digest(sql.as_bytes()));
        out.push(Migration {
            version,
            name: stem,
            sql,
            sha256,
        });
    }
    out.sort_by_key(|m| m.version);
    // 版本号必须严格递增无重复
    for (i, m) in out.iter().enumerate() {
        if m.version as usize != i + 1 {
            return Err(StoreError::MigrationOrder(format!(
                "期望版本 {}，实际 {}",
                i + 1,
                m.version
            )));
        }
    }
    Ok(out)
}

impl Store {
    /// 打开（必要时创建）数据库并应用全部未执行迁移。迁移失败则整体失败、不开放 HTTP。
    pub fn open(db_path: &Path, migrations_dir: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(db_path)?;
        Self::finish_open(conn, db_path, migrations_dir)
    }

    /// 内存库（仅测试与诊断用）。
    pub fn open_in_memory(migrations_dir: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::finish_open(conn, Path::new(":memory:"), migrations_dir)
    }

    fn finish_open(
        mut conn: Connection,
        db_path: &Path,
        migrations_dir: &Path,
    ) -> Result<Self, StoreError> {
        // 事务外先设 WAL；busy timeout 与 foreign_keys 随后常驻。
        let journal: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !journal.eq_ignore_ascii_case("wal") && db_path != Path::new(":memory:") {
            // 内存库返回 memory，属正常；文件库必须 WAL。
            return Err(StoreError::Open(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("无法启用 WAL，当前 journal_mode={journal}")),
            )));
        }
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;

        // FTS5 是 D-03 的硬要求，缺失必须明确失败。
        let fts5: i32 =
            conn.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
                r.get(0)
            })?;
        if fts5 != 1 {
            return Err(StoreError::Fts5Missing);
        }

        Self::migrate(&mut conn, migrations_dir)?;

        Ok(Store {
            conn,
            path: db_path.to_path_buf(),
        })
    }

    fn migrate(conn: &mut Connection, migrations_dir: &Path) -> Result<(), StoreError> {
        let migrations = load_migrations(migrations_dir)?;
        if migrations.is_empty() {
            return Ok(());
        }
        let applied: Vec<(u32, String)> = {
            // schema_migrations 可能尚不存在（首次启动）。
            let exists: i32 = conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
                [],
                |r| r.get(0),
            )?;
            if exists == 0 {
                Vec::new()
            } else {
                let mut stmt =
                    conn.prepare("SELECT version, sha256 FROM schema_migrations ORDER BY version")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        for m in &migrations {
            if let Some((_, recorded)) = applied.iter().find(|(v, _)| *v == m.version) {
                if recorded != &m.sha256 {
                    return Err(StoreError::MigrationChecksum {
                        name: m.name.clone(),
                        recorded: recorded.clone(),
                        current: m.sha256.clone(),
                    });
                }
                continue;
            }
            let tx = conn.transaction()?;
            // doc4/02 §1：0003 建 jobs_through_unique 前先检查既有数据是否已有同
            // (tenant,user,host,session,through) 的重复作业；有则列出精确 job ID 并中止，
            // 与迁移 DDL 同一事务，任何失败都不留部分修改。
            if m.name == "0003_job_recovery" {
                let ids: Option<String> = tx.query_row(
                    "SELECT group_concat(a.id, ',') FROM extraction_jobs a
                     WHERE EXISTS (
                       SELECT 1 FROM extraction_jobs b
                       WHERE b.tenant_id=a.tenant_id AND b.user_id=a.user_id
                         AND b.host_id=a.host_id AND b.session_id=a.session_id
                         AND b.through_event_seq=a.through_event_seq AND b.id<>a.id
                     )",
                    [],
                    |r| r.get::<_, Option<String>>(0),
                )?;
                if let Some(ids) = ids {
                    return Err(StoreError::MigrationJobConflict { ids });
                }
            }
            tx.execute_batch(&m.sql)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, name, sha256, applied_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![m.version, m.name, m.sha256, now_rfc3339()?],
            )?;
            // index_state 单例行由迁移执行器写入。
            tx.execute(
                "INSERT OR IGNORE INTO index_state (singleton, generation, dirty, updated_at) VALUES (1, 0, 0, ?1)",
                rusqlite::params![now_rfc3339()?],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn db_path(&self) -> &Path {
        &self.path
    }

    /// `memoryd principal add`：生成 32 字节随机令牌，base64url 写入只允许当前用户读取的新文件；
    /// 数据库只存 SHA-256 哈希；令牌只显示一次。
    pub fn principal_add(
        &mut self,
        tenant_id: &str,
        user_id: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token =
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
        self.insert_principal(tenant_id, user_id, &token, token_out)
    }

    /// `memoryd principal rotate-token`：换令牌，原令牌立即失效。
    pub fn principal_rotate_token(
        &mut self,
        tenant_id: &str,
        user_id: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM principals WHERE tenant_id=?1 AND user_id=?2",
                rusqlite::params![tenant_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(StoreError::PrincipalNotFound {
                tenant: tenant_id.to_string(),
                user: user_id.to_string(),
            });
        }
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token =
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE principals SET token_sha256=?3 WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![tenant_id, user_id, hash],
        )?;
        tx.commit()?;
        write_token_file(token_out, &token)
    }

    fn insert_principal(
        &mut self,
        tenant_id: &str,
        user_id: &str,
        token: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM principals WHERE tenant_id=?1 AND user_id=?2",
                rusqlite::params![tenant_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if exists {
            return Err(StoreError::PrincipalExists {
                tenant: tenant_id.to_string(),
                user: user_id.to_string(),
            });
        }
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let now = now_rfc3339()?;
        // V2-S1（doc7/04 §1.1）：principal 与其 user_main 域注册行必须同事务出现，
        // 否则新用户的域表里没有主域行。
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at) VALUES (?1, ?2, ?3, 'active', ?4)",
            rusqlite::params![tenant_id, user_id, hash, now],
        )?;
        // 0001—0014 的历史 schema 快照（迁移演练测试）没有 memory_domains；
        // 只有 0015 之后才写主域注册行，迁移测试因此仍可在旧版本库上建 principal。
        let has_domain_table: bool = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_domains'",
            [],
            |r| Ok(r.get::<_, i64>(0)? > 0),
        )?;
        if has_domain_table {
            tx.execute(
                "INSERT INTO memory_domains
                   (tenant_id, user_id, domain_id, kind, status, policy_version,
                    created_reason, created_at, updated_at)
                 VALUES (?1, ?2, 'user_main', 'user_main', 'active', 1, 'principal_create', ?3, ?3)
                 ON CONFLICT(tenant_id, user_id, domain_id) DO NOTHING",
                rusqlite::params![tenant_id, user_id, now],
            )?;
        }
        tx.commit()?;
        write_token_file(token_out, token)
    }

    /// 每个请求最前面调用：由令牌哈希查出 scope。disabled 或不存在返回 None（统一 401）。
    pub fn verify_token(&self, token: &str) -> Result<Option<ScopeKey>, StoreError> {
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let row = self
            .conn
            .query_row(
                "SELECT tenant_id, user_id FROM principals WHERE token_sha256=?1 AND status='active'",
                rusqlite::params![hash],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(tenant_id, user_id)| ScopeKey { tenant_id, user_id }))
    }

    /// 诊断：迁移状态。
    pub fn doctor_summary(&self) -> Result<String, StoreError> {
        let applied: u32 = self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?;
        let principals: u64 = self
            .conn
            .query_row("SELECT count(*) FROM principals", [], |r| r.get(0))?;
        let (gen, dirty): (u64, i64) = self.conn.query_row(
            "SELECT generation, dirty FROM index_state WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(format!(
            "schema_version={applied} principals={principals} index_generation={gen} index_dirty={dirty}"
        ))
    }
}

fn write_token_file(path: &Path, token: &str) -> Result<(), StoreError> {
    use std::io::Write;
    if path.exists() {
        return Err(StoreError::TokenFileExists(path.display().to_string()));
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| StoreError::TokenFileIo(format!("{}: {e}", path.display())))?;
    // 文件内容：单行令牌 + 换行；权限依赖 OS 默认（Windows 下无 POSIX 权限位，属已知差异）。
    f.write_all(token.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .map_err(|e| StoreError::TokenFileIo(format!("{}: {e}", path.display())))?;
    f.flush()
        .map_err(|e| StoreError::TokenFileIo(format!("{}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrations_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations")
    }

    #[test]
    fn migrate_and_verify_principal() {
        let dir = std::env::temp_dir().join(format!("am-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let token_file = dir.join("user_a.token");

        let mut store = Store::open(&db, &migrations_dir()).unwrap();
        store.principal_add("t1", "user-a", &token_file).unwrap();
        let token = fs::read_to_string(&token_file).unwrap().trim().to_string();
        let scope = store.verify_token(&token).unwrap().unwrap();
        assert_eq!(scope.tenant_id, "t1");
        assert_eq!(scope.user_id, "user-a");
        // 错误令牌 → None
        assert!(store.verify_token("bogus").unwrap().is_none());

        // rotate 后旧令牌失效
        let token_file2 = dir.join("user_a_rotated.token");
        store
            .principal_rotate_token("t1", "user-a", &token_file2)
            .unwrap();
        assert!(store.verify_token(&token).unwrap().is_none());
        let token2 = fs::read_to_string(&token_file2).unwrap().trim().to_string();
        assert_eq!(
            store.verify_token(&token2).unwrap().unwrap().user_id,
            "user-a"
        );

        // 重复 add 拒绝
        let dup = dir.join("dup.token");
        let err = store.principal_add("t1", "user-a", &dup).unwrap_err();
        assert!(matches!(err, StoreError::PrincipalExists { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_v1_db_to_v2_prompt_version() {
        // doc2/05 §3：旧库有序升级到 schema 2，已有 v1 作业与候选仍可读，
        // prompt_version 由 DEFAULT 回填 'extract_v1'。
        let dir = std::env::temp_dir().join(format!("am-store-v2-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v1.db");
        let token_file = dir.join("u.token");

        // 手工构造"仅 0001"的旧库（模拟 v1 现场）。
        let migrations = migrations_dir();
        let v1_sql = fs::read_to_string(migrations.join("0001_init.sql")).unwrap();
        let mut conn = Connection::open(&db).unwrap();
        conn.execute_batch(&v1_sql).unwrap();
        // 与迁移加载器同一算法，避免硬编码哈希随文件换行变化而脆断。
        let v1_sha = hex::encode(sha2::Sha256::digest(v1_sql.as_bytes()));
        conn.execute_batch(&format!(
            "INSERT INTO schema_migrations (version, name, sha256, applied_at)
             VALUES (1, '0001_init', '{v1_sha}', '2026-09-24T00:00:00Z');"
        ))
        .unwrap();
        // 旧 v1 数据：principal + 一条已 succeeded 的 v1 作业。
        conn.execute_batch(
            "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at)
             VALUES ('t1', 'u1', 'x', 'active', '2026-09-24T00:00:00Z');
             INSERT INTO extraction_jobs
               (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                status, attempts, run_after, created_at, updated_at)
             VALUES ('j1', 't1', 'u1', 'dsh', 's1', 'v1:5', 5, 'succeeded', 1,
                     '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z');",
        )
        .unwrap();
        drop(conn);

        // Store::open 依序应用 0002、0003、0004 升级（版本随迁移文件递增）。
        let mut store = Store::open(&db, &migrations).unwrap();
        let schema_v: u32 = store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(schema_v, memory_contract::SCHEMA_VERSION);
        let scope = ScopeKey {
            tenant_id: "t1".into(),
            user_id: "u1".into(),
        };
        let job = store.get_job(&scope, "j1").unwrap().unwrap();
        assert_eq!(job.status, "succeeded");
        assert_eq!(
            job.prompt_version, "extract_v1",
            "旧作业必须回填 extract_v1"
        );
        // 新 flush 写入当前版本（extract_v3/admit_v2）；0002 的列默认值保持 extract_v1、
        // 0004 默认保持 admit_v1（迁移已冻结，仅对不带该列插入的历史行生效，
        // flush 一律显式写当前常量）。
        assert_eq!(
            job.admission_version, "admit_v1",
            "历史作业必须回填 admit_v1"
        );
        // doc7/03：D5-3 的 extract_v3/admit_v2 切换保持冻结；当前默认版本在其后
        // 同提交切换为 extract_v4/admit_v4（Muse文档/13 rewrite 步骤）。
        assert_eq!(memory_contract::EXTRACT_PROMPT_VERSION, "extract_v4");
        assert_eq!(memory_contract::ADMISSION_VERSION, "admit_v4");
        let _ = token_file;
        let _ = fs::remove_dir_all(&dir);
    }

    /// 手工构造"仅到 0002"的旧库（模拟 v2 现场）：按迁移加载器同算法记录 checksum。
    fn build_v2_db(db: &Path) {
        let migrations = migrations_dir();
        let conn = Connection::open(db).unwrap();
        for (name, file) in [
            ("0001_init", "0001_init.sql"),
            ("0002_prompt_version", "0002_prompt_version.sql"),
        ] {
            let sql = fs::read_to_string(migrations.join(file)).unwrap();
            let sha = hex::encode(Sha256::digest(sql.as_bytes()));
            conn.execute_batch(&sql).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO schema_migrations (version, name, sha256, applied_at)
                 VALUES ({v}, '{name}', '{sha}', '2026-09-24T00:00:00Z');",
                v = if name == "0001_init" { 1 } else { 2 },
            ))
            .unwrap();
        }
        drop(conn);
    }

    #[test]
    fn migrate_v2_db_to_v3_job_recovery() {
        // doc4 卡 D4-1：schema 2→3 升级；旧作业保留 ID/status/attempts/prompt_version，
        // claim_generation 由 DEFAULT 回填 0；重复启动不重放迁移。
        let dir = std::env::temp_dir().join(format!("am-store-v3-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v2.db");
        build_v2_db(&db);
        {
            let mut conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at)
                 VALUES ('t1', 'u1', 'x', 'active', '2026-09-24T00:00:00Z');
                 INSERT INTO extraction_jobs
                   (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                    status, attempts, run_after, created_at, updated_at)
                 VALUES ('j1', 't1', 'u1', 'dsh', 's1', 'v1:5', 5, 'retryable_failed', 2,
                         '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z');",
            )
            .unwrap();
        }

        let store = Store::open(&db, &migrations_dir()).unwrap();
        let (schema_v, applied): (u32, i64) = store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version),0), count(*) FROM schema_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(schema_v, memory_contract::SCHEMA_VERSION);
        assert_eq!(
            applied,
            memory_contract::SCHEMA_VERSION as i64,
            "0001 起各迁移一条，无重放"
        );
        let scope = ScopeKey {
            tenant_id: "t1".into(),
            user_id: "u1".into(),
        };
        let job = store.get_job(&scope, "j1").unwrap().unwrap();
        assert_eq!(job.status, "retryable_failed");
        assert_eq!(job.attempts, 2);
        assert_eq!(job.prompt_version, "extract_v1", "旧作业回填 extract_v1");
        let gen: i32 = store
            .conn()
            .query_row(
                "SELECT claim_generation FROM extraction_jobs WHERE id='j1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gen, 0, "既有作业 claim_generation 回填 0");
        // skips 表存在且为空。
        let skips: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM extraction_job_skips", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(skips, 0);
        // 重复启动：迁移不重放。
        let store2 = Store::open(&db, &migrations_dir()).unwrap();
        let applied2: i64 = store2
            .conn()
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied2, memory_contract::SCHEMA_VERSION as i64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_v3_conflict_reports_job_ids_and_keeps_rows() {
        // doc4 卡 D4-1 / doc4/04 §4：唯一索引冲突 → 报精确 job ID、未丢行、不做部分 DDL；
        // 冲突排除后同一库可正常升级。
        let dir = std::env::temp_dir().join(format!("am-store-v3-conflict-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("conflict.db");
        build_v2_db(&db);
        {
            // window_key 不同绕开 0001 的既有唯一键，但 (host,session,through) 冲突。
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at)
                 VALUES ('t1', 'u1', 'x', 'active', '2026-09-24T00:00:00Z');
                 INSERT INTO extraction_jobs
                   (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                    status, attempts, run_after, created_at, updated_at)
                 VALUES ('jA', 't1', 'u1', 'dsh', 's1', 'v1:5', 5, 'succeeded', 1,
                         '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z'),
                        ('jB', 't1', 'u1', 'dsh', 's1', 'legacy:5', 5, 'queued', 0,
                         '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z');",
            )
            .unwrap();
        }
        let err = match Store::open(&db, &migrations_dir()) {
            Err(e) => e,
            Ok(_) => panic!("冲突库必须迁移失败"),
        };
        match err {
            StoreError::MigrationJobConflict { ids } => {
                assert!(
                    ids.contains("jA") && ids.contains("jB"),
                    "应列出冲突 job ID：{ids}"
                );
            }
            other => panic!("应为 MigrationJobConflict，实际 {other:?}"),
        }
        // 未丢行、未做部分 DDL（schema 仍为 2，新表/新列不存在）。
        {
            let conn = Connection::open(&db).unwrap();
            let jobs: i64 = conn
                .query_row("SELECT count(*) FROM extraction_jobs", [], |r| r.get(0))
                .unwrap();
            assert_eq!(jobs, 2, "冲突不得删行");
            let schema_v: u32 = conn
                .query_row(
                    "SELECT COALESCE(MAX(version),0) FROM schema_migrations",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(schema_v, 2);
            let has_col: i64 = conn.query_row(
                "SELECT count(*) FROM pragma_table_info('extraction_jobs') WHERE name='claim_generation'",
                [], |r| r.get(0),
            ).unwrap();
            assert_eq!(has_col, 0, "失败迁移不得留下新列");
        }
        // 排除冲突后同一库可升级。
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("DELETE FROM extraction_jobs WHERE id='jB';")
                .unwrap();
        }
        let store = Store::open(&db, &migrations_dir()).unwrap();
        let schema_v: u32 = store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version),0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(schema_v, memory_contract::SCHEMA_VERSION);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 手工构造"仅到 0003"的旧库（模拟 schema 3 现场）：按迁移加载器同算法记录 checksum。
    fn build_v3_db(db: &Path) {
        let migrations = migrations_dir();
        let conn = Connection::open(db).unwrap();
        for (name, file, v) in [
            ("0001_init", "0001_init.sql", 1),
            ("0002_prompt_version", "0002_prompt_version.sql", 2),
            ("0003_job_recovery", "0003_job_recovery.sql", 3),
        ] {
            let sql = fs::read_to_string(migrations.join(file)).unwrap();
            let sha = hex::encode(Sha256::digest(sql.as_bytes()));
            conn.execute_batch(&sql).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO schema_migrations (version, name, sha256, applied_at)
                 VALUES ({v}, '{name}', '{sha}', '2026-09-24T00:00:00Z');"
            ))
            .unwrap();
        }
        drop(conn);
    }

    #[test]
    fn migrate_v3_db_to_v4_admission_version() {
        // doc5 卡 D5-1（doc5/03 §1）：schema 3→4；历史作业所有旧列不变、admission_version
        // 回填 admit_v1；空库直装；重复打开不重跑；新作业仍写 extract_v2/admit_v1。
        let dir = std::env::temp_dir().join(format!("am-store-v4-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v3.db");
        build_v3_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at)
                 VALUES ('t1', 'u1', 'x', 'active', '2026-09-24T00:00:00Z');
                 INSERT INTO extraction_jobs
                   (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                    status, attempts, error_code, run_after, created_at, updated_at)
                 VALUES ('j1', 't1', 'u1', 'dsh', 's1', 'v1:5', 5, 'retryable_failed', 2,
                         'MODEL_TIMEOUT', '2026-09-24T00:00:00Z',
                         '2026-09-24T00:00:00Z', '2026-09-24T00:00:00Z');",
            )
            .unwrap();
        }

        let store = Store::open(&db, &migrations_dir()).unwrap();
        let (schema_v, applied): (u32, i64) = store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version),0), count(*) FROM schema_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 迁移器应用全部未执行迁移：当前 checkout 会一路升到 SCHEMA_VERSION（≥4）。
        assert_eq!(schema_v, memory_contract::SCHEMA_VERSION);
        assert_eq!(applied, memory_contract::SCHEMA_VERSION as i64);
        // 历史作业：旧列逐字段不变，新列回填 admit_v1。
        let row: (String, String, i32, Option<String>, String, String, i64) = store.conn().query_row(
            "SELECT status, prompt_version, attempts, error_code, window_key, admission_version,
                    claim_generation
             FROM extraction_jobs WHERE id='j1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        ).unwrap();
        assert_eq!(row.0, "retryable_failed");
        assert_eq!(row.1, "extract_v1", "prompt_version 不因 0004 改变");
        assert_eq!(row.2, 2);
        assert_eq!(row.3.as_deref(), Some("MODEL_TIMEOUT"));
        assert_eq!(row.4, "v1:5");
        assert_eq!(row.5, "admit_v1", "历史作业回填 admit_v1");
        assert_eq!(row.6, 0);
        // 重复打开：迁移不重放。
        let store2 = Store::open(&db, &migrations_dir()).unwrap();
        let applied2: i64 = store2
            .conn()
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied2, memory_contract::SCHEMA_VERSION as i64);

        // 新作业写当前默认版本：extract_v3/admit_v2（doc5/03 §3，D5-3 已切换）。
        let mut store3 = Store::open_in_memory(&migrations_dir()).unwrap();
        let tdir = dir.join("tok");
        fs::create_dir_all(&tdir).unwrap();
        store3
            .principal_add("t", "u", &tdir.join("u.token"))
            .unwrap();
        let scope = ScopeKey {
            tenant_id: "t".into(),
            user_id: "u".into(),
        };
        let t = chrono::Utc::now();
        store3
            .record_evidence(
                &scope,
                &memory_domain::Origin {
                    host_id: "dsh".into(),
                    agent_id: "a".into(),
                    session_id: "s1".into(),
                },
                1,
                "user",
                "user",
                &t,
                "以后回答我用中文",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        let _ = store3
            .flush_window(
                &scope,
                "dsh",
                "s1",
                1,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        let (pv, av): (String, String) = store3
            .conn()
            .query_row(
                "SELECT prompt_version, admission_version FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pv, "extract_v4");
        assert_eq!(av, "admit_v4", "doc7/03 切换后新作业写 extract_v4/admit_v4");
        // doc7/03：D5-3 的 extract_v3/admit_v2 切换保持冻结；当前默认版本在其后
        // 同提交切换为 extract_v4/admit_v4（Muse文档/13 rewrite 步骤）。
        assert_eq!(memory_contract::EXTRACT_PROMPT_VERSION, "extract_v4");
        assert_eq!(memory_contract::ADMISSION_VERSION, "admit_v4");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 手工构造"仅到 0004"的旧库（模拟 schema 4 现场）：按迁移加载器同算法记录 checksum。
    fn build_v4_db(db: &Path) {
        let migrations = migrations_dir();
        let conn = Connection::open(db).unwrap();
        for (name, file, v) in [
            ("0001_init", "0001_init.sql", 1),
            ("0002_prompt_version", "0002_prompt_version.sql", 2),
            ("0003_job_recovery", "0003_job_recovery.sql", 3),
            ("0004_admission_version", "0004_admission_version.sql", 4),
        ] {
            let sql = fs::read_to_string(migrations.join(file)).unwrap();
            let sha = hex::encode(Sha256::digest(sql.as_bytes()));
            conn.execute_batch(&sql).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO schema_migrations (version, name, sha256, applied_at)
                 VALUES ({v}, '{name}', '{sha}', '2026-09-26T00:00:00Z');"
            ))
            .unwrap();
        }
        drop(conn);
    }

    #[test]
    fn migrate_v4_db_to_v5_soul_resident_keeps_old_rows() {
        // doc6 卡 D6-1（doc6/02 §2/§9）：0005 只新增表，不改 0001—0004；
        // v4 文件副本旧行逐字段不变，新表存在且为空，重复打开不重放。
        let dir = std::env::temp_dir().join(format!("am-store-v5-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v4.db");
        build_v4_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO principals (tenant_id, user_id, token_sha256, status, created_at)
                 VALUES ('t1', 'u1', 'x', 'active', '2026-09-26T00:00:00Z');
                 INSERT INTO evidence_events
                   (id, tenant_id, user_id, host_id, agent_id, session_id, event_seq, role,
                    source_kind, occurred_at, received_at, content, content_sha256)
                 VALUES ('e1', 't1', 'u1', 'dsh', 'a', 's1', 1, 'user', 'user',
                         '2026-09-26T00:00:00Z', '2026-09-26T00:00:00Z', '原话', 'h');
                 INSERT INTO memories
                   (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256,
                    source_class, status, version, origin_host_id, origin_agent_id,
                    created_at, updated_at)
                 VALUES ('m1', 't1', 'u1', 'fact', '用户住在杭州', '用户住在杭州', 'h1',
                         'user_explicit', 'active', 1, 'dsh', 'a',
                         '2026-09-26T00:00:00Z', '2026-09-26T00:00:00Z');",
            )
            .unwrap();
        }

        let store = Store::open(&db, &migrations_dir()).unwrap();
        let (schema_v, applied): (u32, i64) = store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version),0), count(*) FROM schema_migrations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(schema_v, memory_contract::SCHEMA_VERSION);
        assert_eq!(
            applied,
            memory_contract::SCHEMA_VERSION as i64,
            "0001—0005 各一条，无重放"
        );
        // 旧行逐字段不变。
        let memory_row: (String, String, String, String, i64) = store
            .conn()
            .query_row(
                "SELECT kind, claim, normalized_claim, status, version FROM memories
             WHERE tenant_id='t1' AND user_id='u1' AND id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            memory_row,
            (
                "fact".into(),
                "用户住在杭州".into(),
                "用户住在杭州".into(),
                "active".into(),
                1
            )
        );
        let evidence_count: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM evidence_events WHERE tenant_id='t1' AND user_id='u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(evidence_count, 1);
        // 新表存在且为空。
        for table in [
            "memory_audit",
            "soul_profiles",
            "soul_revisions",
            "resident_pins",
            "mutation_receipts",
        ] {
            let count: i64 = store
                .conn()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table} 应存在且为空");
        }
        // 新表 scope 外键生效：无 principal 的 scope 无法写入 soul。
        let mut store_mut = store;
        let fk_blocked = store_mut.conn_mut().execute(
            "INSERT INTO soul_profiles
               (tenant_id, user_id, agent_id, body_md, version, body_sha256, created_at, updated_at)
             VALUES ('t1', 'ghost', 'a', 'x', 1, 'h', '2026-09-26T00:00:00Z', '2026-09-26T00:00:00Z')",
            [],
        );
        assert!(
            fk_blocked.is_err(),
            "soul_profiles 必须受 principals 外键约束"
        );
        // 重复打开：迁移不重放。
        let store2 = Store::open(&db, &migrations_dir()).unwrap();
        let applied2: i64 = store2
            .conn()
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied2, memory_contract::SCHEMA_VERSION as i64);
        let _ = fs::remove_dir_all(&dir);
    }
}
