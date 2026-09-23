//! Winter mode: a date range during which the daemon keeps its hands off.
//!
//! While a season is active the daemon does not reconcile the relay and the
//! plug's on-device timers are cleared, so the pump can be worked on with
//! the button. When the end date arrives the timers are synced back from the
//! config and reconciling resumes. The season is persisted in a small file,
//! `winter <start> [<end>]`, with ISO dates in the configured timezone.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// A hands-off period. `end` is the first day normal operation resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Season {
    pub start: NaiveDate,
    pub end: Option<NaiveDate>,
}

impl Season {
    /// Builds a season, rejecting an end on or before the start.
    pub fn new(start: NaiveDate, end: Option<NaiveDate>) -> Result<Self> {
        if let Some(e) = end
            && e <= start
        {
            bail!("end date {e} must be after start date {start}");
        }
        Ok(Self { start, end })
    }

    /// Whether `today` falls inside the season.
    pub fn is_active(&self, today: NaiveDate) -> bool {
        today >= self.start && self.end.is_none_or(|e| today < e)
    }

    /// Whether the season has finished, i.e. `today` is on or after `end`.
    pub fn is_over(&self, today: NaiveDate) -> bool {
        self.end.is_some_and(|e| today >= e)
    }

    /// Parses the file format.
    pub fn parse(text: &str) -> Result<Self> {
        let mut parts = text.split_whitespace();
        if parts.next() != Some("winter") {
            bail!("expected 'winter <start> [end]'");
        }
        let start: NaiveDate = parts
            .next()
            .ok_or_else(|| anyhow!("missing start date"))?
            .parse()
            .context("bad start date")?;
        let end = parts
            .next()
            .map(|e| e.parse::<NaiveDate>().context("bad end date"))
            .transpose()?;
        Self::new(start, end)
    }

    /// Serialises to the file format.
    pub fn to_file_string(self) -> String {
        match self.end {
            Some(e) => format!("winter {} {}\n", self.start, e),
            None => format!("winter {}\n", self.start),
        }
    }
}

impl fmt::Display for Season {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.end {
            Some(e) => write!(f, "from {} until {}", self.start, e),
            None => write!(f, "from {} until cancelled", self.start),
        }
    }
}

/// Reads the season file; malformed files are logged and treated as absent.
pub fn read(path: &Path) -> Result<Option<Season>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    match Season::parse(&text) {
        Ok(s) => Ok(Some(s)),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring malformed season file");
            Ok(None)
        }
    }
}

/// Writes the season file, creating the parent directory if needed.
pub fn write(path: &Path, season: Season) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(path, season.to_file_string()).with_context(|| format!("write {}", path.display()))
}

/// Removes the season file if present.
pub fn clear(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn activity_window() {
        let s = Season::new(d("2026-10-15"), Some(d("2027-04-20"))).unwrap();
        assert!(!s.is_active(d("2026-10-14")));
        assert!(s.is_active(d("2026-10-15")));
        assert!(s.is_active(d("2027-04-19")));
        assert!(!s.is_active(d("2027-04-20")));
        assert!(s.is_over(d("2027-04-20")));
        assert!(!s.is_over(d("2027-04-19")));
        let open = Season::new(d("2026-10-15"), None).unwrap();
        assert!(open.is_active(d("2030-01-01")));
        assert!(!open.is_over(d("2030-01-01")));
        assert!(Season::new(d("2026-10-15"), Some(d("2026-10-15"))).is_err());
    }

    #[test]
    fn file_format() {
        let s = Season::parse("winter 2026-10-15 2027-04-20\n").unwrap();
        assert_eq!(s.to_file_string(), "winter 2026-10-15 2027-04-20\n");
        assert_eq!(s.to_string(), "from 2026-10-15 until 2027-04-20");
        let open = Season::parse("winter 2026-10-15").unwrap();
        assert_eq!(open.end, None);
        assert!(Season::parse("summer 2026-10-15").is_err());
        assert!(Season::parse("winter soon").is_err());
        let path = std::env::temp_dir().join(format!("ecopumpd-season-{}", std::process::id()));
        write(&path, s).unwrap();
        assert_eq!(read(&path).unwrap(), Some(s));
        clear(&path).unwrap();
        assert_eq!(read(&path).unwrap(), None);
    }
}
