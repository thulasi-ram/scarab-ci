//! Cron schedules for `on: cron:` (git-bug 5d5fc8a).
//!
//! A schedule is a five-field cron expression, `minute hour day-of-month
//! month day-of-week`, read in UTC, or one of the macros `@yearly`
//! (`@annually`), `@monthly`, `@weekly`, `@daily` (`@midnight`) and
//! `@hourly`. A field is `*`, a number, a range `a-b`, any of those with a
//! step `/n`, or a comma-separated list of them. Months may be written
//! `jan`-`dec` and days of the week `sun`-`sat`; day-of-week `7` is Sunday,
//! as `0` is.
//!
//! Day-of-month and day-of-week follow the usual cron rule: when both are
//! restricted (neither starts with `*`), a day matches if either matches;
//! otherwise both must.
//!
//! Two fires of one schedule are at least [`MIN_INTERVAL_MINUTES`] apart:
//! [`Schedule::parse`] refuses `* * * * *` and `*/2 * * * *`, as GitHub
//! Actions does. Each fire costs forge API calls, and the floor bounds them.
//!
//! Pure: [`Schedule::next_after`] takes the time as an input and never reads
//! a clock.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};

/// A parsed schedule: the set of values each field allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    /// Day-of-month was written starting with `*`.
    dom_any: bool,
    /// Day-of-week was written starting with `*`.
    dow_any: bool,
}

/// One field's bounds and the names it accepts.
struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
};
const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
};
const DAY_OF_MONTH: Field = Field {
    name: "day of month",
    min: 1,
    max: 31,
    names: &[],
};
const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    names: &[
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ],
};
/// `7` is accepted and folded onto `0` (Sunday) after parsing.
const DAY_OF_WEEK: Field = Field {
    name: "day of week",
    min: 0,
    max: 7,
    names: &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
};

/// How far ahead [`Schedule::next_after`] looks before it reports that a
/// schedule never fires (`0 0 30 2 *`). Five years covers every leap-day
/// schedule.
const SEARCH_YEARS: i32 = 5;

/// The shortest time, in minutes, a schedule may leave between two fires.
pub const MIN_INTERVAL_MINUTES: u32 = 5;

impl Schedule {
    /// Parse a schedule. The error says which field is wrong and why, or
    /// that the schedule fires more often than [`MIN_INTERVAL_MINUTES`].
    pub fn parse(expr: &str) -> Result<Schedule, String> {
        let schedule = Self::parse_fields(expr)?;
        let gap = schedule.min_gap_minutes();
        if gap < MIN_INTERVAL_MINUTES {
            return Err(format!(
                "`{}` can fire {gap} minute{} apart; schedules fire at most every \
                 {MIN_INTERVAL_MINUTES} minutes",
                expr.trim(),
                if gap == 1 { "" } else { "s" }
            ));
        }
        Ok(schedule)
    }

    /// The parse without the interval floor.
    fn parse_fields(expr: &str) -> Result<Schedule, String> {
        let expr = expr.trim();
        let expanded = match expr.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *".to_string(),
            "@monthly" => "0 0 1 * *".to_string(),
            "@weekly" => "0 0 * * 0".to_string(),
            "@daily" | "@midnight" => "0 0 * * *".to_string(),
            "@hourly" => "0 * * * *".to_string(),
            other if other.starts_with('@') => {
                return Err(format!(
                    "`{expr}` is not a schedule this server knows; use five fields or one of \
                     @yearly, @monthly, @weekly, @daily, @hourly"
                ))
            }
            _ => expr.to_string(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "`{expr}` has {} fields; a schedule has five: minute hour day-of-month month \
                 day-of-week (UTC)",
                fields.len()
            ));
        }
        let minutes = parse_field(fields[0], &MINUTE)?;
        let hours = parse_field(fields[1], &HOUR)?;
        let days_of_month = parse_field(fields[2], &DAY_OF_MONTH)?;
        let months = parse_field(fields[3], &MONTH)?;
        let mut days_of_week = parse_field(fields[4], &DAY_OF_WEEK)?;
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week & !(1 << 7)) | 1;
        }
        let schedule = Schedule {
            minutes,
            hours: hours as u32,
            days_of_month: days_of_month as u32,
            months: months as u16,
            days_of_week: days_of_week as u8,
            dom_any: fields[2].starts_with('*'),
            dow_any: fields[4].starts_with('*'),
        };
        // A schedule that names no real day (`0 0 31 2 *`) is refused rather
        // than kept as one that never fires.
        if schedule.next_after(0).is_none() {
            return Err(format!(
                "`{expr}` names no date that exists, so it never fires"
            ));
        }
        Ok(schedule)
    }

    /// The first time, in Unix milliseconds, strictly after `after_ms` at
    /// which this schedule fires: always a whole minute. `None` when it does
    /// not fire within [`SEARCH_YEARS`] years, or the time is out of range.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let after = DateTime::from_timestamp_millis(after_ms)?.naive_utc();
        // The next whole minute after `after`.
        let mut t = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let limit_year = t.year() + SEARCH_YEARS;
        while t.year() <= limit_year {
            if !self.month_matches(t.month()) {
                t = first_of_next_month(t)?;
                continue;
            }
            if !self.day_matches(t.date()) {
                t = t.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if self.minutes & (1 << t.minute()) == 0 {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t.and_utc().timestamp_millis());
        }
        None
    }

    /// The shortest gap, in minutes, between two fires. Taken over the
    /// minutes of one day, and across midnight as if the next day also
    /// matched, so a schedule is never let through on its day fields.
    fn min_gap_minutes(&self) -> u32 {
        let fires: Vec<u32> = (0..24)
            .filter(|h| self.hours & (1 << h) != 0)
            .flat_map(|h| {
                (0..60)
                    .filter(move |m| self.minutes & (1 << m) != 0)
                    .map(move |m| h * 60 + m)
            })
            .collect();
        let (Some(first), Some(last)) = (fires.first(), fires.last()) else {
            return u32::MAX;
        };
        fires
            .windows(2)
            .map(|w| w[1] - w[0])
            .chain(std::iter::once(first + 24 * 60 - last))
            .min()
            .unwrap_or(u32::MAX)
    }

    fn month_matches(&self, month: u32) -> bool {
        self.months & (1 << month) != 0
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom = self.days_of_month & (1 << date.day()) != 0;
        let dow = self.days_of_week & (1 << date.weekday().num_days_from_sunday()) != 0;
        if self.dom_any || self.dow_any {
            dom && dow
        } else {
            dom || dow
        }
    }
}

fn first_of_next_month(t: NaiveDateTime) -> Option<NaiveDateTime> {
    let (y, m) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)
}

/// Parse one field into a bit set of the values it allows.
fn parse_field(text: &str, field: &Field) -> Result<u64, String> {
    let mut bits = 0u64;
    for part in text.split(',') {
        if part.is_empty() {
            return Err(format!("{}: `{text}` has an empty list entry", field.name));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("{}: step `{s}` is not a number", field.name))?;
                if step == 0 {
                    return Err(format!("{}: a step must be at least 1", field.name));
                }
                if step > field.max {
                    return Err(format!(
                        "{}: step `{step}` is larger than {}",
                        field.name, field.max
                    ));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (field.min, field.max)
        } else if let Some((a, b)) = range.split_once('-') {
            let lo = value(a, field)?;
            let hi = value(b, field)?;
            if lo > hi {
                return Err(format!("{}: range `{range}` runs backwards", field.name));
            }
            (lo, hi)
        } else {
            let v = value(range, field)?;
            // `5/15` means from 5 to the end, every 15.
            if part.contains('/') {
                (v, field.max)
            } else {
                (v, v)
            }
        };
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            // A step past the field's end stops the walk; it never wraps.
            match v.checked_add(step) {
                Some(next) => v = next,
                None => break,
            }
        }
    }
    Ok(bits)
}

/// One value of a field: a number in range, or one of the field's names.
fn value(text: &str, field: &Field) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();
    if let Some(i) = field.names.iter().position(|n| *n == lower) {
        // Month names start at 1 (`jan`), day names at 0 (`sun`).
        return Ok(i as u32 + u32::from(field.min == 1));
    }
    let v: u32 = text.parse().map_err(|_| {
        format!(
            "{}: `{text}` is not a number{}",
            field.name,
            if field.names.is_empty() {
                ""
            } else {
                " or a name"
            }
        )
    })?;
    if v < field.min || v > field.max {
        return Err(format!(
            "{}: `{v}` is outside {}-{}",
            field.name, field.min, field.max
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix milliseconds of a UTC time.
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
    }

    /// The maths, without the interval floor, so `* * * * *` can be used.
    fn next(expr: &str, after: i64) -> i64 {
        Schedule::parse_fields(expr)
            .unwrap()
            .next_after(after)
            .unwrap()
    }

    #[test]
    fn the_next_fire_is_the_first_matching_minute_strictly_after() {
        let t = at(2026, 11, 3, 10, 7);
        assert_eq!(next("* * * * *", t), at(2026, 11, 3, 10, 8));
        assert_eq!(next("0 * * * *", t), at(2026, 11, 3, 11, 0));
        assert_eq!(next("*/15 * * * *", t), at(2026, 11, 3, 10, 15));
        assert_eq!(next("0 2 * * *", t), at(2026, 11, 4, 2, 0));
        // Strictly after: a time on a slot moves to the next slot.
        assert_eq!(
            next("0 * * * *", at(2026, 11, 3, 11, 0)),
            at(2026, 11, 3, 12, 0)
        );
        // Seconds inside the minute do not count as the minute itself.
        assert_eq!(
            next("0 * * * *", at(2026, 11, 3, 10, 59) + 30_000),
            at(2026, 11, 3, 11, 0)
        );
    }

    #[test]
    fn fields_take_lists_ranges_steps_and_names() {
        let t = at(2026, 11, 3, 10, 7); // a Tuesday
        assert_eq!(next("5,10 9-17 * * *", t), at(2026, 11, 3, 10, 10));
        assert_eq!(next("5/20 * * * *", t), at(2026, 11, 3, 10, 25));
        assert_eq!(next("0 0 * * fri", t), at(2026, 11, 6, 0, 0));
        assert_eq!(next("0 0 1 jan *", t), at(2027, 1, 1, 0, 0));
        assert_eq!(next("0 0 * * 7", t), at(2026, 11, 8, 0, 0)); // Sunday
        assert_eq!(
            next("0 0 * * mon-fri", at(2026, 11, 6, 12, 0)),
            at(2026, 11, 9, 0, 0)
        );
    }

    #[test]
    fn day_of_month_and_day_of_week_match_either_when_both_are_set() {
        // The 15th, or any Friday: Friday the 6th comes first.
        let t = at(2026, 11, 3, 10, 7);
        assert_eq!(next("0 0 15 * fri", t), at(2026, 11, 6, 0, 0));
        // `*` in day-of-week: only the 15th.
        assert_eq!(next("0 0 15 * *", t), at(2026, 11, 15, 0, 0));
    }

    #[test]
    fn macros_expand_to_their_fields() {
        let t = at(2026, 11, 3, 10, 7);
        assert_eq!(next("@hourly", t), at(2026, 11, 3, 11, 0));
        assert_eq!(next("@daily", t), at(2026, 11, 4, 0, 0));
        assert_eq!(next("@weekly", t), at(2026, 11, 8, 0, 0));
        assert_eq!(next("@monthly", t), at(2026, 12, 1, 0, 0));
        assert_eq!(next("@yearly", t), at(2027, 1, 1, 0, 0));
    }

    #[test]
    fn schedules_five_minutes_apart_or_more_are_accepted() {
        for expr in [
            "*/5 * * * *",
            "0,5 * * * *",
            "@hourly",
            "@daily",
            "58 3 * * *",
            "1,58 3 * * *",
        ] {
            assert!(Schedule::parse(expr).is_ok(), "{expr}");
        }
    }

    #[test]
    fn a_leap_day_schedule_fires_on_the_next_leap_year() {
        assert_eq!(
            next("0 0 29 2 *", at(2026, 11, 3, 0, 0)),
            at(2028, 2, 29, 0, 0)
        );
    }

    #[test]
    fn a_bad_schedule_says_which_field_is_wrong() {
        for (expr, says) in [
            ("* * * *", "has 4 fields"),
            ("* * * * * *", "has 6 fields"),
            ("60 * * * *", "minute: `60` is outside 0-59"),
            ("* 24 * * *", "hour: `24` is outside 0-23"),
            ("* * 0 * *", "day of month: `0` is outside 1-31"),
            ("* * * 13 *", "month: `13` is outside 1-12"),
            ("* * * * 8", "day of week: `8` is outside 0-7"),
            ("*/0 * * * *", "a step must be at least 1"),
            ("10-5 * * * *", "runs backwards"),
            ("x * * * *", "minute: `x` is not a number"),
            ("* * * foo *", "month: `foo` is not a number or a name"),
            ("1,,2 * * * *", "empty list entry"),
            ("@reboot", "not a schedule this server knows"),
            (
                "0 1/4294967295 * * *",
                "hour: step `4294967295` is larger than 23",
            ),
            ("0 0 30 2 *", "never fires"),
            ("* * * * *", "can fire 1 minute apart"),
            ("*/2 * * * *", "can fire 2 minutes apart"),
            ("0,4 * * * *", "can fire 4 minutes apart"),
            // Across midnight: 23:58 and 00:02.
            ("2,58 0,23 * * *", "can fire 4 minutes apart"),
        ] {
            let e = Schedule::parse(expr).unwrap_err();
            assert!(e.contains(says), "{expr}: {e}");
        }
    }
}
