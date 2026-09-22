//! Maps a local wall-clock time to the desired pump state via on-windows.
//!
//! The pump should be on whenever the current local time falls inside any
//! [`Window`]. A window whose `end` is earlier than its `start` crosses
//! midnight and belongs to the day it starts on. Start is inclusive, end
//! exclusive. Everything is evaluated in the configured timezone so daylight
//! saving changes need no special handling.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Weekday};
use chrono_tz::Tz;

/// Minutes in a day.
const DAY_MINUTES: u16 = 24 * 60;
/// How far ahead [`Schedule::next_transition`] looks before giving up.
const LOOKAHEAD_DAYS: i64 = 8;

/// A time of day with minute resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hm(u16);

impl Hm {
    /// Builds a time of day; `hour` must be < 24 and `minute` < 60.
    pub fn new(hour: u16, minute: u16) -> Result<Self> {
        if hour >= 24 || minute >= 60 {
            bail!("time {hour:02}:{minute:02} is out of range");
        }
        Ok(Self(hour * 60 + minute))
    }

    /// Minutes since midnight.
    pub fn minutes(self) -> u16 {
        self.0
    }
}

impl FromStr for Hm {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (h, m) = s.split_once(':').ok_or_else(|| anyhow!("expected HH:MM, got {s:?}"))?;
        let hour = h.parse().with_context(|| format!("bad hour in {s:?}"))?;
        let minute = m.parse().with_context(|| format!("bad minute in {s:?}"))?;
        Self::new(hour, minute)
    }
}

impl fmt::Display for Hm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// Parses a weekday name such as `mon` or `Monday`.
pub fn parse_weekday(s: &str) -> Result<Weekday> {
    let lower = s.to_ascii_lowercase();
    let day = match lower.get(..3) {
        Some("mon") => Weekday::Mon,
        Some("tue") => Weekday::Tue,
        Some("wed") => Weekday::Wed,
        Some("thu") => Weekday::Thu,
        Some("fri") => Weekday::Fri,
        Some("sat") => Weekday::Sat,
        Some("sun") => Weekday::Sun,
        _ => bail!("unknown weekday {s:?}"),
    };
    Ok(day)
}

/// One "pump on" interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    start: Hm,
    end: Hm,
    /// Indexed by [`Weekday::num_days_from_monday`].
    days: [bool; 7],
}

impl Window {
    /// Builds a window active on `days` (all days when empty).
    pub fn new(start: Hm, end: Hm, days: &[Weekday]) -> Result<Self> {
        if start == end {
            bail!("window {start}-{end} is empty; start and end must differ");
        }
        let mut mask = [days.is_empty(); 7];
        for d in days {
            mask[d.num_days_from_monday() as usize] = true;
        }
        Ok(Self { start, end, days: mask })
    }

    fn crosses_midnight(&self) -> bool {
        self.end < self.start
    }

    fn active_on(&self, day: Weekday) -> bool {
        self.days[day.num_days_from_monday() as usize]
    }

    /// Whether `minute` of `day` is inside this window.
    fn contains(&self, day: Weekday, minute: u16) -> bool {
        let today = self.active_on(day);
        if self.crosses_midnight() {
            (today && minute >= self.start.0) || (self.active_on(day.pred()) && minute < self.end.0)
        } else {
            today && minute >= self.start.0 && minute < self.end.0
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)?;
        if self.days.iter().any(|&d| !d) {
            let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
            let list: Vec<&str> = names
                .iter()
                .zip(self.days)
                .filter(|(_, on)| *on)
                .map(|(n, _)| *n)
                .collect();
            write!(f, " [{}]", list.join(","))?;
        }
        Ok(())
    }
}

/// The union of all configured windows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    windows: Vec<Window>,
}

impl Schedule {
    /// Builds a schedule from its windows.
    pub fn new(windows: Vec<Window>) -> Self {
        Self { windows }
    }

    /// The configured windows.
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// Whether the pump should be on at `now`.
    pub fn desired(&self, now: DateTime<Tz>) -> bool {
        let minute = u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(DAY_MINUTES);
        let day = now.weekday();
        self.windows.iter().any(|w| w.contains(day, minute))
    }

    /// The next instant, after `now`, at which the desired state changes.
    ///
    /// Returns `None` when nothing changes within the next week, e.g. an
    /// empty schedule. Scans minute by minute so DST days are handled by the
    /// timezone rather than by arithmetic on local times.
    pub fn next_transition(&self, now: DateTime<Tz>) -> Option<(DateTime<Tz>, bool)> {
        let tz = now.timezone();
        let current = self.desired(now);
        // Truncate in UTC: local times can be ambiguous on a DST fall-back day.
        let base = now.with_timezone(&chrono::Utc).with_second(0)?.with_nanosecond(0)?;
        (1..=LOOKAHEAD_DAYS * i64::from(DAY_MINUTES))
            .map(|k| tz.from_utc_datetime(&(base + Duration::minutes(k)).naive_utc()))
            .find(|t| self.desired(*t) != current)
            .map(|t| (t, !current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    const TZ: Tz = chrono_tz::America::Toronto;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        TZ.from_local_datetime(
            &NaiveDate::from_ymd_opt(y, mo, d)
                .unwrap()
                .and_hms_opt(h, mi, 0)
                .unwrap(),
        )
        .earliest()
        .unwrap()
    }

    fn win(start: &str, end: &str, days: &[Weekday]) -> Window {
        Window::new(start.parse().unwrap(), end.parse().unwrap(), days).unwrap()
    }

    #[test]
    fn parses_hm() {
        assert_eq!("09:05".parse::<Hm>().unwrap(), Hm::new(9, 5).unwrap());
        assert!("24:00".parse::<Hm>().is_err());
        assert!("9".parse::<Hm>().is_err());
        assert_eq!(Hm::new(17, 0).unwrap().to_string(), "17:00");
    }

    #[test]
    fn same_day_window_boundaries() {
        let s = Schedule::new(vec![win("09:00", "17:00", &[])]);
        // 2026-09-22 is a Tuesday.
        assert!(!s.desired(at(2026, 9, 22, 8, 59)));
        assert!(s.desired(at(2026, 9, 22, 9, 0)));
        assert!(s.desired(at(2026, 9, 22, 16, 59)));
        assert!(!s.desired(at(2026, 9, 22, 17, 0)));
    }

    #[test]
    fn midnight_crossing_window() {
        let s = Schedule::new(vec![win("22:00", "02:00", &[Weekday::Tue])]);
        assert!(!s.desired(at(2026, 9, 22, 21, 59)));
        assert!(s.desired(at(2026, 9, 22, 23, 30)));
        assert!(s.desired(at(2026, 9, 23, 1, 59))); // Wednesday morning, started Tuesday
        assert!(!s.desired(at(2026, 9, 23, 2, 0)));
        assert!(!s.desired(at(2026, 9, 23, 23, 0))); // Wednesday night not in days
        assert!(!s.desired(at(2026, 9, 22, 1, 0))); // Tuesday morning: Monday not in days
    }

    #[test]
    fn day_filter() {
        let s = Schedule::new(vec![win("09:00", "17:00", &[Weekday::Sat, Weekday::Sun])]);
        assert!(!s.desired(at(2026, 9, 22, 12, 0))); // Tue
        assert!(s.desired(at(2026, 9, 26, 12, 0))); // Sat
    }

    #[test]
    fn dst_transition_day() {
        // 2026-03-08 clocks go 02:00 -> 03:00 in Toronto; 2026-11-01 they go back.
        let s = Schedule::new(vec![win("01:00", "04:00", &[])]);
        assert!(s.desired(at(2026, 3, 8, 1, 30)));
        assert!(s.desired(at(2026, 3, 8, 3, 30)));
        assert!(!s.desired(at(2026, 3, 8, 4, 0)));
        // Fall back: the window is 4h of wall time but still ends at local 04:00.
        let before = at(2026, 11, 1, 0, 30);
        let (t, on) = s.next_transition(before).unwrap();
        assert!(on);
        assert_eq!(t, at(2026, 11, 1, 1, 0));
        let (t2, on2) = s.next_transition(t).unwrap();
        assert!(!on2);
        assert_eq!(t2.hour(), 4);
        assert_eq!(t2 - t, Duration::hours(4));
    }

    #[test]
    fn next_transition_and_empty_schedule() {
        let s = Schedule::new(vec![win("09:00", "17:00", &[])]);
        let (t, on) = s.next_transition(at(2026, 9, 22, 8, 0)).unwrap();
        assert!(on);
        assert_eq!(t, at(2026, 9, 22, 9, 0));
        let (t, on) = s.next_transition(at(2026, 9, 22, 9, 0)).unwrap();
        assert!(!on);
        assert_eq!(t, at(2026, 9, 22, 17, 0));
        assert!(Schedule::default().next_transition(at(2026, 9, 22, 9, 0)).is_none());
    }

    #[test]
    fn rejects_empty_window() {
        assert!(Window::new(Hm::new(9, 0).unwrap(), Hm::new(9, 0).unwrap(), &[]).is_err());
    }
}
