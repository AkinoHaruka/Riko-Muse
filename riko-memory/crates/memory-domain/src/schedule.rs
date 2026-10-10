//! V2-B1 调度判定（doc7/08 §2）。纯函数：离线就能穷举验证
//! 「无新信号零模型调用」与「各任务开关互不影响」。

/// 后台任务的调度参数。默认值来自 doc7/08 §1（本项目初始参数，非 Muse 实测常量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleConfig {
    pub upkeep_enabled: bool,
    pub relationships_enabled: bool,
    pub quiet_enabled: bool,
    pub nightly_enabled: bool,
    /// upkeep 最小间隔（默认 3600 秒）。
    pub upkeep_interval_secs: i64,
    /// relationships 最小间隔（默认 3600 秒）。
    pub relationships_interval_secs: i64,
    /// quiet pass 需要的沉寂时长（默认 1200 秒 = 20 分钟）。
    pub quiet_idle_secs: i64,
    /// quiet pass 每日上限（默认 3 次）。
    pub quiet_max_per_day: i64,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        ScheduleConfig {
            upkeep_enabled: true,
            relationships_enabled: true,
            quiet_enabled: true,
            nightly_enabled: true,
            upkeep_interval_secs: 3600,
            relationships_interval_secs: 3600,
            quiet_idle_secs: 1200,
            quiet_max_per_day: 3,
        }
    }
}

fn elapsed_at_least(now_epoch: i64, then_epoch: i64, secs: i64) -> bool {
    now_epoch.saturating_sub(then_epoch) >= secs
}

/// upkeep：有新信号**且**距上次运行达到间隔。无新信号恒 false。
pub fn upkeep_due(
    cfg: &ScheduleConfig,
    now_epoch: i64,
    last_run_epoch: Option<i64>,
    unprocessed: i64,
) -> bool {
    if !cfg.upkeep_enabled || unprocessed <= 0 {
        return false;
    }
    match last_run_epoch {
        None => true,
        Some(last) => elapsed_at_least(now_epoch, last, cfg.upkeep_interval_secs),
    }
}

/// relationships：实体/关系来源有变化**且**距上次运行达到间隔。
pub fn relationships_due(
    cfg: &ScheduleConfig,
    now_epoch: i64,
    last_run_epoch: Option<i64>,
    changed_entities: i64,
) -> bool {
    if !cfg.relationships_enabled || changed_entities <= 0 {
        return false;
    }
    match last_run_epoch {
        None => true,
        Some(last) => elapsed_at_least(now_epoch, last, cfg.relationships_interval_secs),
    }
}

/// quiet pass：三重条件缺一不可——沉寂够久、有未消化重要信号、当日未超上限。
pub fn quiet_due(
    cfg: &ScheduleConfig,
    now_epoch: i64,
    last_activity_epoch: i64,
    unprocessed: i64,
    runs_today: i64,
) -> bool {
    if !cfg.quiet_enabled {
        return false;
    }
    if unprocessed <= 0 || runs_today >= cfg.quiet_max_per_day {
        return false;
    }
    elapsed_at_least(now_epoch, last_activity_epoch, cfg.quiet_idle_secs)
}

/// nightly：每个自然日一次；日期字符串按 \`YYYY-MM-DD\` 比较，跨日才算 due。
pub fn nightly_due(cfg: &ScheduleConfig, today: &str, last_nightly_date: Option<&str>) -> bool {
    if !cfg.nightly_enabled {
        return false;
    }
    match last_nightly_date {
        None => true,
        Some(last) => last < today,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_new_signal_never_runs() {
        let cfg = ScheduleConfig::default();
        assert!(!upkeep_due(&cfg, 10_000, None, 0));
        assert!(!relationships_due(&cfg, 10_000, None, 0));
        assert!(!quiet_due(&cfg, 10_000, 0, 0, 0));
        // 有信号但开关关闭也不跑。
        let off = ScheduleConfig {
            upkeep_enabled: false,
            ..ScheduleConfig::default()
        };
        assert!(!upkeep_due(&off, 10_000, None, 5));
    }

    #[test]
    fn intervals_and_day_boundaries() {
        let cfg = ScheduleConfig::default();
        assert!(!upkeep_due(&cfg, 3_599, Some(0), 1));
        assert!(upkeep_due(&cfg, 3_600, Some(0), 1));
        assert!(upkeep_due(&cfg, 1, None, 1));
        assert!(nightly_due(&cfg, "2026-10-07", Some("2026-10-06")));
        assert!(!nightly_due(&cfg, "2026-10-06", Some("2026-10-06")));
        assert!(nightly_due(&cfg, "2026-10-06", None));
    }

    #[test]
    fn quiet_requires_all_three_conditions() {
        let cfg = ScheduleConfig::default();
        // 沉寂不足
        assert!(!quiet_due(&cfg, 1_199, 0, 1, 0));
        // 无重要信号
        assert!(!quiet_due(&cfg, 5_000, 0, 0, 0));
        // 当日已 3 次
        assert!(!quiet_due(&cfg, 5_000, 0, 1, 3));
        // 三者都满足
        assert!(quiet_due(&cfg, 1_200, 0, 1, 2));
    }

    #[test]
    fn switches_are_independent() {
        let base = ScheduleConfig::default();
        let cases = [
            ScheduleConfig {
                upkeep_enabled: false,
                ..base
            },
            ScheduleConfig {
                relationships_enabled: false,
                ..base
            },
            ScheduleConfig {
                quiet_enabled: false,
                ..base
            },
            ScheduleConfig {
                nightly_enabled: false,
                ..base
            },
        ];
        for cfg in cases {
            // 关掉一个开关不得改变其它任务的判定。
            let upkeep_off = !cfg.upkeep_enabled;
            let rel_off = !cfg.relationships_enabled;
            let quiet_off = !cfg.quiet_enabled;
            let night_off = !cfg.nightly_enabled;
            assert_eq!(upkeep_due(&cfg, 10_000, Some(0), 1), !upkeep_off);
            assert_eq!(relationships_due(&cfg, 10_000, Some(0), 1), !rel_off);
            assert_eq!(quiet_due(&cfg, 10_000, 0, 1, 0), !quiet_off);
            assert_eq!(
                nightly_due(&cfg, "2026-10-07", Some("2026-10-06")),
                !night_off
            );
        }
    }
}
