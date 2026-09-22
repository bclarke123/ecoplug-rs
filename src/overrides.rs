//! Temporary manual overrides persisted in a small state file.
//!
//! The file holds one line, `on <unix-expiry>` or `off <unix-expiry>`. While
//! it exists and has not expired it wins over the schedule. The daemon deletes
//! it once it expires; the CLI writes and clears it. The module is named
//! `overrides` because `override` is a reserved word in Rust.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use tracing::warn;

use crate::proto::Power;

/// A manual override of the schedule until `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Override {
    pub power: Power,
    pub until: DateTime<Utc>,
}

impl Override {
    /// Whether the override has passed its expiry at `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.until
    }

    /// Parses the `on|off <unix-seconds>` file format.
    pub fn parse(text: &str) -> Result<Self> {
        let mut parts = text.split_whitespace();
        let power = match parts.next() {
            Some("on") => Power::On,
            Some("off") => Power::Off,
            other => bail!("expected on|off, got {other:?}"),
        };
        let secs: i64 = parts
            .next()
            .ok_or_else(|| anyhow!("missing expiry"))?
            .parse()
            .context("expiry is not an integer")?;
        let until = Utc
            .timestamp_opt(secs, 0)
            .single()
            .ok_or_else(|| anyhow!("expiry out of range"))?;
        Ok(Self { power, until })
    }

    /// Serialises to the file format.
    pub fn to_file_string(self) -> String {
        format!("{} {}\n", self.power, self.until.timestamp())
    }
}

impl fmt::Display for Override {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} until {}", self.power, self.until.to_rfc3339())
    }
}

/// Reads the override file, returning `None` if absent, malformed or expired.
///
/// A malformed file is logged at `warn` and treated as absent; an expired file
/// is removed. Errors are returned only for unexpected I/O failures.
pub fn read(path: &Path, now: DateTime<Utc>) -> Result<Option<Override>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let ov = match Override::parse(&text) {
        Ok(ov) => ov,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring malformed override file");
            return Ok(None);
        }
    };
    if ov.is_expired(now) {
        clear(path)?;
        return Ok(None);
    }
    Ok(Some(ov))
}

/// Writes an override, creating the parent directory if needed.
pub fn write(path: &Path, ov: Override) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(path, ov.to_file_string()).with_context(|| format!("write {}", path.display()))
}

/// Removes the override file if present.
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

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ecopumpd-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn round_trip_and_expiry() {
        let path = tmp("rt");
        let now = Utc.timestamp_opt(1_727_049_600, 0).unwrap();
        let ov = Override {
            power: Power::On,
            until: now + chrono::Duration::hours(2),
        };
        write(&path, ov).unwrap();
        assert_eq!(read(&path, now).unwrap(), Some(ov));
        assert_eq!(read(&path, now + chrono::Duration::hours(3)).unwrap(), None);
        assert!(!path.exists(), "expired override should be deleted");
    }

    #[test]
    fn malformed_is_ignored() {
        let path = tmp("bad");
        fs::write(&path, "maybe later\n").unwrap();
        assert_eq!(read(&path, Utc::now()).unwrap(), None);
        assert!(path.exists(), "malformed file is left in place for inspection");
        assert_eq!(read(&tmp("missing"), Utc::now()).unwrap(), None);
    }

    #[test]
    fn parse_format() {
        let ov = Override::parse("off 1727049600").unwrap();
        assert_eq!(ov.power, Power::Off);
        assert_eq!(ov.until.timestamp(), 1_727_049_600);
        assert_eq!(ov.to_file_string(), "off 1727049600\n");
        assert!(Override::parse("on").is_err());
        assert!(Override::parse("on soon").is_err());
    }
}
