//! # pearl-scheduler
//!
//! Scheduling engine that determines WHEN tasks become eligible for execution.
//!
//! This crate implements the scheduling layer required by Constitution Article 3:
//! scheduling is infrastructure that belongs in code, never in prompts. A task's
//! eligibility is a mechanical function of its schedule, the current time, and the
//! system profile - no LLM judgement is involved.
//!
//! Supported schedule types:
//! - **Cron**: 5-field cron expressions (minute hour day-of-month month day-of-week)
//! - **Interval**: fixed time between triggers
//! - **OneShot**: fires once at a specific time (or immediately if `at` is None)
//! - **Manual**: only triggered by explicit external action
//! - **Disabled**: never fires
//!
//! Misfire policy handles what happens when a scheduled trigger is missed (system was
//! down). Profile-aware throttling limits how many tasks can be triggered per poll
//! cycle based on the current [`RuntimeProfile`].

use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use pearl_core::{Clock, RuntimeProfile, TaskId};
use serde::{Deserialize, Serialize};

/// Schedule type for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Schedule {
    /// 5-field cron expression: minute hour day-of-month month day-of-week.
    Cron {
        expression: String,
        timezone: String,
    },
    /// Fixed interval between triggers.
    Interval {
        #[serde(with = "timedelta_serde")]
        every: TimeDelta,
    },
    /// Fires once at a specified time, or immediately if `at` is None.
    OneShot { at: Option<DateTime<Utc>> },
    /// Only triggered by explicit external action; never auto-fires.
    Manual,
    /// Never fires.
    Disabled,
}

/// What happens when a scheduled trigger is missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MisfirePolicy {
    /// If missed, skip that occurrence entirely.
    Skip,
    /// If missed, fire exactly once on the next poll.
    RunOnce,
    /// If missed, fire all missed occurrences.
    RunAll,
}

/// A task with its schedule configuration.
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub task_id: TaskId,
    pub schedule: Schedule,
    pub misfire_policy: MisfirePolicy,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub enabled: bool,
}

impl ScheduledTask {
    pub fn new(task_id: TaskId, schedule: Schedule, misfire_policy: MisfirePolicy) -> Self {
        Self {
            task_id,
            schedule,
            misfire_policy,
            last_triggered_at: None,
            enabled: true,
        }
    }
}

// ─── Cron expression parser ─────────────────────────────────────────────────

/// A parsed 5-field cron expression.
///
/// Supports:
/// - `*` (any value)
/// - Single numeric values (e.g. `0`, `15`, `3`)
/// - Comma-separated lists (e.g. `1,15,30`)
/// - Ranges (e.g. `1-5`)
/// - Step values with `*` (e.g. `*/5`, `*/15`)
///
/// Fields: minute (0-59), hour (0-23), day-of-month (1-31), month (1-12), day-of-week (0-6, 0=Sunday).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub minutes: CronField,
    pub hours: CronField,
    pub days_of_month: CronField,
    pub months: CronField,
    pub days_of_week: CronField,
}

/// A single cron field, representing the set of values that match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronField {
    /// Matches any value.
    Any,
    /// Matches specific values.
    Values(Vec<u32>),
}

impl CronField {
    /// Whether the given value matches this field.
    pub fn matches(&self, value: u32) -> bool {
        match self {
            CronField::Any => true,
            CronField::Values(vs) => vs.contains(&value),
        }
    }
}

impl CronExpr {
    /// Parses a 5-field cron expression string.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::InvalidCron`] if the expression cannot be parsed.
    pub fn parse(expr: &str) -> Result<Self, SchedulerError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(SchedulerError::InvalidCron {
                expression: expr.to_string(),
                detail: format!("expected 5 fields, got {}", fields.len()),
            });
        }

        Ok(CronExpr {
            minutes: Self::parse_field(fields[0], 0, 59, expr)?,
            hours: Self::parse_field(fields[1], 0, 23, expr)?,
            days_of_month: Self::parse_field(fields[2], 1, 31, expr)?,
            months: Self::parse_field(fields[3], 1, 12, expr)?,
            days_of_week: Self::parse_field(fields[4], 0, 6, expr)?,
        })
    }

    fn parse_field(
        field: &str,
        min: u32,
        max: u32,
        full_expr: &str,
    ) -> Result<CronField, SchedulerError> {
        if field == "*" {
            return Ok(CronField::Any);
        }

        // Step: */N
        if let Some(step_str) = field.strip_prefix("*/") {
            let step: u32 = step_str.parse().map_err(|_| SchedulerError::InvalidCron {
                expression: full_expr.to_string(),
                detail: format!("invalid step value: {field}"),
            })?;
            if step == 0 {
                return Err(SchedulerError::InvalidCron {
                    expression: full_expr.to_string(),
                    detail: "step value cannot be zero".to_string(),
                });
            }
            let values: Vec<u32> = (min..=max).filter(|v| (v - min) % step == 0).collect();
            return Ok(CronField::Values(values));
        }

        // Comma-separated list (may include ranges)
        let mut values = Vec::new();
        for part in field.split(',') {
            if part.contains('-') {
                // Range: N-M
                let bounds: Vec<&str> = part.splitn(2, '-').collect();
                let start: u32 = bounds[0].parse().map_err(|_| SchedulerError::InvalidCron {
                    expression: full_expr.to_string(),
                    detail: format!("invalid range start in: {part}"),
                })?;
                let end: u32 = bounds[1].parse().map_err(|_| SchedulerError::InvalidCron {
                    expression: full_expr.to_string(),
                    detail: format!("invalid range end in: {part}"),
                })?;
                if start > end || start < min || end > max {
                    return Err(SchedulerError::InvalidCron {
                        expression: full_expr.to_string(),
                        detail: format!("range {start}-{end} out of bounds ({min}-{max})"),
                    });
                }
                for v in start..=end {
                    values.push(v);
                }
            } else {
                // Single value
                let v: u32 = part.parse().map_err(|_| SchedulerError::InvalidCron {
                    expression: full_expr.to_string(),
                    detail: format!("invalid numeric value: {part}"),
                })?;
                if v < min || v > max {
                    return Err(SchedulerError::InvalidCron {
                        expression: full_expr.to_string(),
                        detail: format!("value {v} out of bounds ({min}-{max})"),
                    });
                }
                values.push(v);
            }
        }

        values.sort_unstable();
        values.dedup();
        Ok(CronField::Values(values))
    }

    /// Whether the given datetime matches this cron expression.
    pub fn matches(&self, dt: &DateTime<Utc>) -> bool {
        let minute = dt.minute();
        let hour = dt.hour();
        let day = dt.day();
        let month = dt.month();
        // chrono: Monday=1 .. Sunday=7; cron: Sunday=0 .. Saturday=6
        let weekday = dt.weekday().num_days_from_sunday();

        self.minutes.matches(minute)
            && self.hours.matches(hour)
            && self.days_of_month.matches(day)
            && self.months.matches(month)
            && self.days_of_week.matches(weekday)
    }
}

// ─── Scheduler Engine ───────────────────────────────────────────────────────

/// The scheduling engine. Holds scheduled tasks and determines which are due.
pub struct SchedulerEngine<C: Clock> {
    tasks: Vec<ScheduledTask>,
    clock: C,
    throttle: ProfileThrottle,
}

impl<C: Clock> SchedulerEngine<C> {
    pub fn new(clock: C, profile: RuntimeProfile) -> Self {
        Self {
            tasks: Vec::new(),
            clock,
            throttle: ProfileThrottle::new(profile),
        }
    }

    /// Adds a task to the scheduler.
    pub fn add_task(&mut self, task: ScheduledTask) {
        self.tasks.push(task);
    }

    /// Removes a task by ID. Returns true if the task existed.
    pub fn remove_task(&mut self, task_id: &TaskId) -> bool {
        let len_before = self.tasks.len();
        self.tasks.retain(|t| t.task_id != *task_id);
        self.tasks.len() < len_before
    }

    /// Returns a reference to the scheduled tasks.
    pub fn tasks(&self) -> &[ScheduledTask] {
        &self.tasks
    }

    /// Returns a mutable reference to a task by ID.
    pub fn get_task_mut(&mut self, task_id: &TaskId) -> Option<&mut ScheduledTask> {
        self.tasks.iter_mut().find(|t| t.task_id == *task_id)
    }

    /// Updates the runtime profile (e.g., on health degradation).
    pub fn set_profile(&mut self, profile: RuntimeProfile) {
        self.throttle = ProfileThrottle::new(profile);
    }

    /// Polls all tasks and returns which ones should be triggered now.
    ///
    /// Applies profile-aware throttling to limit the number of triggers per poll.
    /// Updates `last_triggered_at` for tasks that fire.
    pub fn poll(&mut self) -> Vec<TaskId> {
        let now = self.clock.now();
        let max_triggers = self.throttle.max_triggers_per_poll();

        let mut triggered = Vec::new();

        for task in self.tasks.iter_mut() {
            if triggered.len() >= max_triggers {
                break;
            }

            if !task.enabled {
                continue;
            }

            if Self::is_due_inner(task, now) {
                task.last_triggered_at = Some(now);

                // One-shot fires once then disables itself
                if matches!(task.schedule, Schedule::OneShot { .. }) {
                    task.enabled = false;
                }

                triggered.push(task.task_id.clone());
            }
        }

        triggered
    }

    /// Whether a task is due at the given time (without mutating state).
    pub fn is_due(task: &ScheduledTask, now: DateTime<Utc>) -> bool {
        if !task.enabled {
            return false;
        }
        Self::is_due_inner(task, now)
    }

    fn is_due_inner(task: &ScheduledTask, now: DateTime<Utc>) -> bool {
        match &task.schedule {
            Schedule::Disabled => false,
            Schedule::Manual => false,
            Schedule::OneShot { at } => match at {
                Some(target) => now >= *target,
                None => task.last_triggered_at.is_none(),
            },
            Schedule::Interval { every } => match task.last_triggered_at {
                Some(last) => now - last >= *every,
                None => true, // Never triggered, fire immediately
            },
            Schedule::Cron { expression, .. } => {
                let Ok(cron) = CronExpr::parse(expression) else {
                    return false;
                };
                if !cron.matches(&now) {
                    return false;
                }
                // Avoid double-firing within the same minute
                match task.last_triggered_at {
                    Some(last) => {
                        // Fired already this minute?
                        let same_minute = last.minute() == now.minute()
                            && last.hour() == now.hour()
                            && last.day() == now.day()
                            && last.month() == now.month()
                            && last.year() == now.year();
                        !same_minute
                    }
                    None => true,
                }
            }
        }
    }

    /// Applies the misfire policy for a task that missed triggers while the system was down.
    ///
    /// `missed_since` is the time since which triggers were missed.
    /// Returns the task IDs that should be triggered as a result.
    pub fn apply_misfire_policy(
        task: &ScheduledTask,
        now: DateTime<Utc>,
        missed_since: DateTime<Utc>,
    ) -> MisfireResult {
        match task.misfire_policy {
            MisfirePolicy::Skip => MisfireResult {
                triggers: 0,
                should_fire: false,
            },
            MisfirePolicy::RunOnce => {
                // Check if any trigger was missed
                let missed = Self::count_missed_triggers(task, now, missed_since);
                if missed > 0 {
                    MisfireResult {
                        triggers: 1,
                        should_fire: true,
                    }
                } else {
                    MisfireResult {
                        triggers: 0,
                        should_fire: false,
                    }
                }
            }
            MisfirePolicy::RunAll => {
                let missed = Self::count_missed_triggers(task, now, missed_since);
                MisfireResult {
                    triggers: missed,
                    should_fire: missed > 0,
                }
            }
        }
    }

    /// Counts how many triggers were missed between `since` and `now`.
    fn count_missed_triggers(
        task: &ScheduledTask,
        now: DateTime<Utc>,
        since: DateTime<Utc>,
    ) -> u32 {
        match &task.schedule {
            Schedule::Interval { every } => {
                if every.num_seconds() <= 0 {
                    return 0;
                }
                let elapsed = now - since;
                let count = elapsed.num_seconds() / every.num_seconds();
                count.max(0) as u32
            }
            Schedule::Cron { expression, .. } => {
                let Ok(cron) = CronExpr::parse(expression) else {
                    return 0;
                };
                // Count matching minutes between since and now
                let mut count = 0u32;
                let mut check = since;
                // Step by minutes, capped at a reasonable limit to avoid infinite loops
                let max_checks = 1440u32; // 24 hours of minutes
                let mut checks_done = 0u32;
                while check < now && checks_done < max_checks {
                    if cron.matches(&check) {
                        count += 1;
                    }
                    check += TimeDelta::try_minutes(1).expect("valid");
                    checks_done += 1;
                }
                count
            }
            Schedule::OneShot { at } => {
                if let Some(target) = at {
                    if *target >= since && *target < now {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            Schedule::Manual | Schedule::Disabled => 0,
        }
    }
}

/// Result of applying a misfire policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MisfireResult {
    /// How many triggers should fire.
    pub triggers: u32,
    /// Whether any triggers should fire at all.
    pub should_fire: bool,
}

// ─── Profile Throttle ───────────────────────────────────────────────────────

/// Profile-aware throttling for the scheduler.
///
/// Limits how many tasks can be triggered per poll cycle based on the current
/// runtime profile. This implements the effective_parallel() pattern from
/// daily_rust: a degraded system narrows what it will attempt.
#[derive(Debug, Clone)]
pub struct ProfileThrottle {
    profile: RuntimeProfile,
}

impl ProfileThrottle {
    pub fn new(profile: RuntimeProfile) -> Self {
        Self { profile }
    }

    pub fn profile(&self) -> RuntimeProfile {
        self.profile
    }

    /// Maximum number of tasks that can be triggered in a single poll cycle.
    ///
    /// Under Normal, effectively unlimited. Under Degraded, capped at 2.
    /// Under Recovery or Emergency, capped at 1.
    pub fn max_triggers_per_poll(&self) -> usize {
        match self.profile {
            RuntimeProfile::Normal => usize::MAX,
            RuntimeProfile::Degraded => 2,
            RuntimeProfile::Recovery => 1,
            RuntimeProfile::Emergency => 1,
        }
    }
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Scheduler errors.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("invalid cron expression '{expression}': {detail}")]
    InvalidCron { expression: String, detail: String },
}

// ─── Serde helper for TimeDelta ─────────────────────────────────────────────

mod timedelta_serde {
    use chrono::TimeDelta;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(td: &TimeDelta, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(td.num_seconds())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<TimeDelta, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = i64::deserialize(deserializer)?;
        TimeDelta::try_seconds(secs).ok_or_else(|| serde::de::Error::custom("invalid TimeDelta"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::TestClock;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    /// Generates a unique TaskId for each test invocation.
    fn test_task_id() -> TaskId {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        TaskId::parse(format!("test-task-{n}")).unwrap()
    }

    // ─── CronExpr parsing tests ─────────────────────────────────────────

    #[test]
    fn cron_every_minute() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(expr.minutes, CronField::Any);
        assert_eq!(expr.hours, CronField::Any);
        assert_eq!(expr.days_of_month, CronField::Any);
        assert_eq!(expr.months, CronField::Any);
        assert_eq!(expr.days_of_week, CronField::Any);

        // Matches any time
        let dt = DateTime::parse_from_rfc3339("2026-08-15T14:30:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&dt));
    }

    #[test]
    fn cron_every_hour() {
        let expr = CronExpr::parse("0 * * * *").unwrap();
        // Matches at minute 0
        let on = DateTime::parse_from_rfc3339("2026-08-15T14:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&on));
        // Does not match at minute 30
        let off = DateTime::parse_from_rfc3339("2026-08-15T14:30:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&off));
    }

    #[test]
    fn cron_daily_midnight() {
        let expr = CronExpr::parse("0 0 * * *").unwrap();
        let midnight = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&midnight));
        let noon = DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&noon));
    }

    #[test]
    fn cron_weekly_monday() {
        // Monday = 1 in cron (Sunday=0)
        let expr = CronExpr::parse("0 0 * * 1").unwrap();
        // 2026-08-17 is a Monday
        let monday = DateTime::parse_from_rfc3339("2026-08-17T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&monday));
        // 2026-08-15 is a Saturday
        let saturday = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&saturday));
    }

    #[test]
    fn cron_step_values() {
        let expr = CronExpr::parse("*/15 * * * *").unwrap();
        // Should match at 0, 15, 30, 45
        let at_0 = DateTime::parse_from_rfc3339("2026-08-15T10:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&at_0));
        let at_15 = DateTime::parse_from_rfc3339("2026-08-15T10:15:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&at_15));
        let at_7 = DateTime::parse_from_rfc3339("2026-08-15T10:07:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&at_7));
    }

    #[test]
    fn cron_comma_separated() {
        let expr = CronExpr::parse("0,30 * * * *").unwrap();
        let at_0 = DateTime::parse_from_rfc3339("2026-08-15T10:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&at_0));
        let at_30 = DateTime::parse_from_rfc3339("2026-08-15T10:30:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&at_30));
        let at_15 = DateTime::parse_from_rfc3339("2026-08-15T10:15:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&at_15));
    }

    #[test]
    fn cron_range() {
        let expr = CronExpr::parse("* 9-17 * * *").unwrap();
        let at_10 = DateTime::parse_from_rfc3339("2026-08-15T10:00:00Z")
            .unwrap()
            .to_utc();
        assert!(expr.matches(&at_10));
        let at_3 = DateTime::parse_from_rfc3339("2026-08-15T03:00:00Z")
            .unwrap()
            .to_utc();
        assert!(!expr.matches(&at_3));
    }

    #[test]
    fn cron_invalid_field_count() {
        let err = CronExpr::parse("* * *").unwrap_err();
        assert!(err.to_string().contains("expected 5 fields"));
    }

    #[test]
    fn cron_invalid_value() {
        let err = CronExpr::parse("60 * * * *").unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn cron_invalid_step_zero() {
        let err = CronExpr::parse("*/0 * * * *").unwrap_err();
        assert!(err.to_string().contains("cannot be zero"));
    }

    // ─── Interval scheduling tests ──────────────────────────────────────

    #[test]
    fn interval_triggers_immediately_when_never_fired() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Normal);
        engine.add_task(task);

        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id]);
    }

    #[test]
    fn interval_does_not_fire_before_period_elapses() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // First poll fires
        let triggered = engine.poll();
        assert_eq!(triggered.len(), 1);

        // Advance only 30s - should not fire again
        clock.advance_secs(30);
        let triggered = engine.poll();
        assert!(triggered.is_empty());

        // Advance another 31s (total 61s) - should fire
        clock.advance_secs(31);
        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id]);
    }

    #[test]
    fn interval_fires_repeatedly() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::Interval {
                every: TimeDelta::try_seconds(10).unwrap(),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // First fire
        assert_eq!(engine.poll().len(), 1);

        // Fire 3 more times
        for _ in 0..3 {
            clock.advance_secs(10);
            let triggered = engine.poll();
            assert_eq!(triggered, vec![task_id.clone()]);
        }
    }

    // ─── Cron scheduling tests ──────────────────────────────────────────

    #[test]
    fn cron_triggers_at_matching_time() {
        // TestClock starts at 2026-08-15T00:00:00Z
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::Cron {
                expression: "0 0 * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // It's midnight, should fire
        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id.clone()]);

        // Same minute, should not fire again
        let triggered = engine.poll();
        assert!(triggered.is_empty());

        // Advance to next day midnight
        clock.advance_secs(86400);
        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id]);
    }

    #[test]
    fn cron_does_not_trigger_at_non_matching_time() {
        // TestClock starts at 2026-08-15T00:00:00Z, advance to 00:01
        let clock = TestClock::new();
        clock.advance_secs(60);
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Cron {
                expression: "0 12 * * *".to_string(), // noon only
                timezone: "UTC".to_string(),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Normal);
        engine.add_task(task);

        let triggered = engine.poll();
        assert!(triggered.is_empty());
    }

    // ─── OneShot scheduling tests ───────────────────────────────────────

    #[test]
    fn oneshot_fires_once_then_disables() {
        let clock = TestClock::new();
        let now = clock.now();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::OneShot {
                at: Some(now + TimeDelta::try_seconds(10).unwrap()),
            },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // Not yet due
        let triggered = engine.poll();
        assert!(triggered.is_empty());

        // Advance past target
        clock.advance_secs(11);
        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id.clone()]);

        // Should not fire again
        clock.advance_secs(100);
        let triggered = engine.poll();
        assert!(triggered.is_empty());

        // Verify task is disabled
        let t = engine.get_task_mut(&task_id).unwrap();
        assert!(!t.enabled);
    }

    #[test]
    fn oneshot_without_target_fires_immediately() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::OneShot { at: None },
            MisfirePolicy::Skip,
        );

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // Fires immediately
        let triggered = engine.poll();
        assert_eq!(triggered, vec![task_id.clone()]);

        // Never again
        clock.advance_secs(1000);
        let triggered = engine.poll();
        assert!(triggered.is_empty());
    }

    // ─── Manual scheduling tests ────────────────────────────────────────

    #[test]
    fn manual_never_auto_fires() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(task_id, Schedule::Manual, MisfirePolicy::Skip);

        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);
        engine.add_task(task);

        // Never fires
        let triggered = engine.poll();
        assert!(triggered.is_empty());

        clock.advance_secs(86400);
        let triggered = engine.poll();
        assert!(triggered.is_empty());
    }

    // ─── Disabled scheduling tests ──────────────────────────────────────

    #[test]
    fn disabled_never_fires() {
        let clock = TestClock::new();
        let task_id = test_task_id();
        let task = ScheduledTask::new(task_id, Schedule::Disabled, MisfirePolicy::Skip);

        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Normal);
        engine.add_task(task);

        let triggered = engine.poll();
        assert!(triggered.is_empty());
    }

    // ─── Misfire policy tests ───────────────────────────────────────────

    #[test]
    fn misfire_skip_produces_no_triggers() {
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::Skip,
        );

        let now = DateTime::parse_from_rfc3339("2026-08-15T01:00:00Z")
            .unwrap()
            .to_utc();
        let missed_since = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        let result = SchedulerEngine::<TestClock>::apply_misfire_policy(&task, now, missed_since);
        assert!(!result.should_fire);
        assert_eq!(result.triggers, 0);
    }

    #[test]
    fn misfire_run_once_fires_exactly_once() {
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::RunOnce,
        );

        let now = DateTime::parse_from_rfc3339("2026-08-15T01:00:00Z")
            .unwrap()
            .to_utc();
        let missed_since = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        let result = SchedulerEngine::<TestClock>::apply_misfire_policy(&task, now, missed_since);
        assert!(result.should_fire);
        assert_eq!(result.triggers, 1);
    }

    #[test]
    fn misfire_run_all_fires_all_missed() {
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::RunAll,
        );

        // 1 hour gap with 60s interval = 60 missed triggers
        let now = DateTime::parse_from_rfc3339("2026-08-15T01:00:00Z")
            .unwrap()
            .to_utc();
        let missed_since = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        let result = SchedulerEngine::<TestClock>::apply_misfire_policy(&task, now, missed_since);
        assert!(result.should_fire);
        assert_eq!(result.triggers, 60);
    }

    #[test]
    fn misfire_run_all_with_cron() {
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Cron {
                expression: "0 * * * *".to_string(), // every hour at :00
                timezone: "UTC".to_string(),
            },
            MisfirePolicy::RunAll,
        );

        // 3 hour gap, should have 3 missed triggers (at :00 each hour)
        let now = DateTime::parse_from_rfc3339("2026-08-15T03:00:00Z")
            .unwrap()
            .to_utc();
        let missed_since = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        let result = SchedulerEngine::<TestClock>::apply_misfire_policy(&task, now, missed_since);
        assert!(result.should_fire);
        assert_eq!(result.triggers, 3);
    }

    #[test]
    fn misfire_no_missed_produces_no_triggers() {
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id,
            Schedule::Interval {
                every: TimeDelta::try_seconds(3600).unwrap(),
            },
            MisfirePolicy::RunAll,
        );

        // Only 30 minutes gap, interval is 1 hour - nothing missed
        let now = DateTime::parse_from_rfc3339("2026-08-15T00:30:00Z")
            .unwrap()
            .to_utc();
        let missed_since = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        let result = SchedulerEngine::<TestClock>::apply_misfire_policy(&task, now, missed_since);
        assert!(!result.should_fire);
        assert_eq!(result.triggers, 0);
    }

    // ─── Profile throttle tests ─────────────────────────────────────────

    #[test]
    fn throttle_normal_allows_many() {
        let throttle = ProfileThrottle::new(RuntimeProfile::Normal);
        assert_eq!(throttle.max_triggers_per_poll(), usize::MAX);
    }

    #[test]
    fn throttle_degraded_limits_to_two() {
        let throttle = ProfileThrottle::new(RuntimeProfile::Degraded);
        assert_eq!(throttle.max_triggers_per_poll(), 2);
    }

    #[test]
    fn throttle_recovery_limits_to_one() {
        let throttle = ProfileThrottle::new(RuntimeProfile::Recovery);
        assert_eq!(throttle.max_triggers_per_poll(), 1);
    }

    #[test]
    fn throttle_emergency_limits_to_one() {
        let throttle = ProfileThrottle::new(RuntimeProfile::Emergency);
        assert_eq!(throttle.max_triggers_per_poll(), 1);
    }

    #[test]
    fn profile_throttle_limits_poll_output() {
        let clock = TestClock::new();
        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Degraded);

        // Add 5 tasks that should all fire immediately (interval, never triggered)
        for _ in 0..5 {
            let task = ScheduledTask::new(
                test_task_id(),
                Schedule::Interval {
                    every: TimeDelta::try_seconds(1).unwrap(),
                },
                MisfirePolicy::Skip,
            );
            engine.add_task(task);
        }

        // Degraded allows only 2
        let triggered = engine.poll();
        assert_eq!(triggered.len(), 2);
    }

    #[test]
    fn profile_throttle_recovery_limits_single() {
        let clock = TestClock::new();
        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Recovery);

        for _ in 0..3 {
            let task = ScheduledTask::new(
                test_task_id(),
                Schedule::Interval {
                    every: TimeDelta::try_seconds(1).unwrap(),
                },
                MisfirePolicy::Skip,
            );
            engine.add_task(task);
        }

        let triggered = engine.poll();
        assert_eq!(triggered.len(), 1);
    }

    #[test]
    fn set_profile_changes_throttle() {
        let clock = TestClock::new();
        let mut engine = SchedulerEngine::new(clock.clone(), RuntimeProfile::Normal);

        for _ in 0..5 {
            let task = ScheduledTask::new(
                test_task_id(),
                Schedule::Interval {
                    every: TimeDelta::try_seconds(1).unwrap(),
                },
                MisfirePolicy::Skip,
            );
            engine.add_task(task);
        }

        // Normal: all fire
        let triggered = engine.poll();
        assert_eq!(triggered.len(), 5);

        // Advance so they are due again
        clock.advance_secs(2);

        // Switch to degraded
        engine.set_profile(RuntimeProfile::Degraded);
        let triggered = engine.poll();
        assert_eq!(triggered.len(), 2);
    }

    // ─── is_due static method tests ─────────────────────────────────────

    #[test]
    fn is_due_respects_enabled_flag() {
        let task_id = test_task_id();
        let mut task = ScheduledTask::new(
            task_id,
            Schedule::Interval {
                every: TimeDelta::try_seconds(1).unwrap(),
            },
            MisfirePolicy::Skip,
        );
        let now = DateTime::parse_from_rfc3339("2026-08-15T00:00:00Z")
            .unwrap()
            .to_utc();

        assert!(SchedulerEngine::<TestClock>::is_due(&task, now));

        task.enabled = false;
        assert!(!SchedulerEngine::<TestClock>::is_due(&task, now));
    }

    // ─── Task management tests ──────────────────────────────────────────

    #[test]
    fn add_and_remove_task() {
        let clock = TestClock::new();
        let mut engine = SchedulerEngine::new(clock, RuntimeProfile::Normal);
        let task_id = test_task_id();
        let task = ScheduledTask::new(
            task_id.clone(),
            Schedule::Interval {
                every: TimeDelta::try_seconds(60).unwrap(),
            },
            MisfirePolicy::Skip,
        );

        engine.add_task(task);
        assert_eq!(engine.tasks().len(), 1);

        let removed = engine.remove_task(&task_id);
        assert!(removed);
        assert_eq!(engine.tasks().len(), 0);

        // Removing nonexistent returns false
        let removed = engine.remove_task(&task_id);
        assert!(!removed);
    }

    // ─── Serde tests ────────────────────────────────────────────────────

    #[test]
    fn schedule_serializes_correctly() {
        let schedule = Schedule::Cron {
            expression: "0 0 * * *".to_string(),
            timezone: "UTC".to_string(),
        };
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains("\"type\":\"cron\""));
        assert!(json.contains("\"expression\":\"0 0 * * *\""));

        let interval = Schedule::Interval {
            every: TimeDelta::try_seconds(300).unwrap(),
        };
        let json = serde_json::to_string(&interval).unwrap();
        assert!(json.contains("\"type\":\"interval\""));
        assert!(json.contains("\"every\":300"));
    }

    #[test]
    fn schedule_roundtrips() {
        let schedules = vec![
            Schedule::Cron {
                expression: "*/5 * * * *".to_string(),
                timezone: "UTC".to_string(),
            },
            Schedule::Interval {
                every: TimeDelta::try_seconds(120).unwrap(),
            },
            Schedule::OneShot {
                at: Some(
                    DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
                        .unwrap()
                        .to_utc(),
                ),
            },
            Schedule::Manual,
            Schedule::Disabled,
        ];

        for schedule in schedules {
            let json = serde_json::to_string(&schedule).unwrap();
            let deserialized: Schedule = serde_json::from_str(&json).unwrap();
            assert_eq!(schedule, deserialized);
        }
    }

    #[test]
    fn misfire_policy_roundtrips() {
        let policies = vec![
            MisfirePolicy::Skip,
            MisfirePolicy::RunOnce,
            MisfirePolicy::RunAll,
        ];
        for policy in policies {
            let json = serde_json::to_string(&policy).unwrap();
            let deserialized: MisfirePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, deserialized);
        }
    }
}
