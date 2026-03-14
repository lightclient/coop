use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;

use crate::config::{CronConfig, UserConfig};

pub(crate) fn parse_timezone(name: &str) -> Result<Tz> {
    name.parse::<Tz>()
        .with_context(|| format!("invalid timezone '{name}'"))
}

fn system_timezone() -> Result<Tz> {
    let name = iana_time_zone::get_timezone().context("system timezone unavailable")?;
    parse_timezone(&name).with_context(|| format!("system timezone '{name}' is invalid"))
}

fn default_timezone_or_utc<F>(default_timezone: F) -> Tz
where
    F: FnOnce() -> Result<Tz>,
{
    default_timezone().unwrap_or(chrono_tz::UTC)
}

fn resolve_user_timezone_with_default<F>(user: &UserConfig, default_timezone: F) -> Result<Tz>
where
    F: FnOnce() -> Result<Tz>,
{
    if let Some(timezone) = user.timezone.as_deref() {
        return parse_timezone(timezone)
            .with_context(|| format!("user '{}' has invalid timezone", user.name));
    }

    Ok(default_timezone_or_utc(default_timezone))
}

pub(crate) fn resolve_user_timezone(user: &UserConfig) -> Result<Tz> {
    resolve_user_timezone_with_default(user, system_timezone)
}

fn resolve_cron_timezone_with_default<F>(
    entry: &CronConfig,
    users: &[UserConfig],
    default_timezone: F,
) -> Result<Tz>
where
    F: Copy + Fn() -> Result<Tz>,
{
    if let Some(timezone) = entry.timezone.as_deref() {
        return parse_timezone(timezone)
            .with_context(|| format!("cron '{}' has invalid timezone", entry.name));
    }

    if let Some(user_name) = entry.user.as_deref()
        && let Some(user) = users.iter().find(|user| user.name == user_name)
    {
        return resolve_user_timezone_with_default(user, default_timezone).with_context(|| {
            format!(
                "cron '{}' inherits timezone from user '{}'",
                entry.name, user_name
            )
        });
    }

    Ok(default_timezone_or_utc(default_timezone))
}

pub(crate) fn resolve_cron_timezone(entry: &CronConfig, users: &[UserConfig]) -> Result<Tz> {
    resolve_cron_timezone_with_default(entry, users, system_timezone)
}

pub(crate) fn next_cron_fire_after(
    schedule: &Schedule,
    timezone: Tz,
    after_utc: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let after_local = after_utc.with_timezone(&timezone);
    schedule
        .after(&after_local)
        .next()
        .map(|time| time.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::scheduler::parse_cron;
    use chrono::TimeZone;
    use coop_core::TrustLevel;

    fn utc_datetime(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
    ) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .single()
            .expect("valid UTC datetime")
    }

    #[test]
    fn resolve_cron_timezone_prefers_explicit_timezone() {
        let cron = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            timezone: Some("Europe/Berlin".to_owned()),
            message: "Morning briefing".to_owned(),
            user: Some("alice".to_owned()),
            delivery: None,
            deliver: None,
            review_prompt: None,
            sandbox: None,
        };
        let users = vec![UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
            timezone: Some("America/Chicago".to_owned()),
            sandbox: None,
        }];

        let timezone = resolve_cron_timezone(&cron, &users).expect("should parse timezone");
        assert_eq!(timezone, chrono_tz::Europe::Berlin);
    }

    #[test]
    fn resolve_cron_timezone_falls_back_to_user_timezone() {
        let cron = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            timezone: None,
            message: "Morning briefing".to_owned(),
            user: Some("alice".to_owned()),
            delivery: None,
            deliver: None,
            review_prompt: None,
            sandbox: None,
        };
        let users = vec![UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
            timezone: Some("America/Chicago".to_owned()),
            sandbox: None,
        }];

        let timezone = resolve_cron_timezone(&cron, &users).expect("should parse timezone");
        assert_eq!(timezone, chrono_tz::America::Chicago);
    }

    #[test]
    fn resolve_user_timezone_defaults_to_system_timezone() {
        let user = UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
            timezone: None,
            sandbox: None,
        };

        let timezone =
            resolve_user_timezone_with_default(&user, || Ok(chrono_tz::America::Los_Angeles))
                .expect("should resolve timezone");
        assert_eq!(timezone, chrono_tz::America::Los_Angeles);
    }

    #[test]
    fn resolve_cron_timezone_uses_users_default_timezone_when_unset() {
        let cron = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            timezone: None,
            message: "Morning briefing".to_owned(),
            user: Some("alice".to_owned()),
            delivery: None,
            deliver: None,
            review_prompt: None,
            sandbox: None,
        };
        let users = vec![UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
            timezone: None,
            sandbox: None,
        }];

        let timezone =
            resolve_cron_timezone_with_default(&cron, &users, || Ok(chrono_tz::Europe::Berlin))
                .expect("should resolve timezone");
        assert_eq!(timezone, chrono_tz::Europe::Berlin);
    }

    #[test]
    fn resolve_cron_timezone_defaults_to_system_timezone() {
        let cron = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            timezone: None,
            message: "Run cleanup".to_owned(),
            user: None,
            delivery: None,
            deliver: None,
            review_prompt: None,
            sandbox: None,
        };

        let timezone =
            resolve_cron_timezone_with_default(&cron, &[], || Ok(chrono_tz::America::Los_Angeles))
                .expect("should resolve timezone");
        assert_eq!(timezone, chrono_tz::America::Los_Angeles);
    }

    #[test]
    fn resolve_cron_timezone_falls_back_to_utc_when_system_timezone_is_unavailable() {
        let cron = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            timezone: None,
            message: "Run cleanup".to_owned(),
            user: None,
            delivery: None,
            deliver: None,
            review_prompt: None,
            sandbox: None,
        };

        let timezone = resolve_cron_timezone_with_default(&cron, &[], || {
            anyhow::bail!("system timezone unavailable")
        })
        .expect("should fall back to UTC");
        assert_eq!(timezone, chrono_tz::UTC);
    }

    #[test]
    fn next_cron_fire_after_uses_timezone_local_wall_clock() {
        let schedule = parse_cron("0 8 * * *").expect("should parse cron");
        let after = utc_datetime(2026, 3, 11, 12, 0, 0);

        let next = next_cron_fire_after(&schedule, chrono_tz::America::Chicago, after)
            .expect("should compute next cron fire");

        assert_eq!(next, utc_datetime(2026, 3, 11, 13, 0, 0));
    }

    #[test]
    fn next_cron_fire_after_tracks_dst_transitions() {
        let schedule = parse_cron("0 8 * * *").expect("should parse cron");
        let after = utc_datetime(2026, 3, 7, 14, 1, 0);

        let next = next_cron_fire_after(&schedule, chrono_tz::America::Chicago, after)
            .expect("should compute next cron fire");

        assert_eq!(next, utc_datetime(2026, 3, 8, 13, 0, 0));
    }
}
