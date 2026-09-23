//! TOML configuration file loading and validation.
//!
//! See `deploy/ecopumpd.toml` for an annotated example.

use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::schedule::{Schedule, Window, parse_weekday};

/// Default location of the config file.
pub const DEFAULT_PATH: &str = "/etc/ecopumpd.toml";

/// Raw on-disk representation, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    device_id: String,
    host: String,
    timezone: String,
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default = "default_poll")]
    poll_interval_secs: u64,
    #[serde(default = "default_override_file")]
    override_file: PathBuf,
    #[serde(default = "default_season_file")]
    season_file: PathBuf,
    #[serde(default)]
    windows: Vec<RawWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    start: String,
    end: String,
    #[serde(default)]
    days: Vec<String>,
}

fn default_bind() -> String {
    "0.0.0.0".to_owned()
}

fn default_listen() -> String {
    "127.0.0.1:8090".to_owned()
}

fn default_poll() -> u64 {
    120
}

fn default_override_file() -> PathBuf {
    PathBuf::from("/var/lib/ecopumpd/override")
}

fn default_season_file() -> PathBuf {
    PathBuf::from("/var/lib/ecopumpd/season")
}

/// Validated daemon configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub device_id: String,
    pub host: IpAddr,
    pub timezone: Tz,
    /// Local address to bind the reply socket to; only matters on multi-homed hosts.
    pub bind: IpAddr,
    /// Address the HTTP API listens on.
    pub listen: SocketAddr,
    /// Optional bearer token required by the HTTP API.
    pub token: Option<String>,
    pub poll_interval: Duration,
    pub override_file: PathBuf,
    /// Where winter mode is persisted.
    pub season_file: PathBuf,
    pub schedule: Schedule,
}

impl Config {
    /// Reads and validates the config file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parses config from TOML text.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;
        if raw.device_id.is_empty() || raw.device_id.len() > 16 {
            bail!("device_id must be 1-16 characters, e.g. ECO-780D4D7D");
        }
        let host = raw
            .host
            .parse()
            .with_context(|| format!("host {:?} is not an IP address", raw.host))?;
        let timezone: Tz = raw
            .timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("unknown timezone {:?}: {e}", raw.timezone))?;
        let bind = raw
            .bind
            .parse()
            .with_context(|| format!("bind {:?} is not an IP address", raw.bind))?;
        let listen = raw
            .listen
            .parse()
            .with_context(|| format!("listen {:?} is not host:port", raw.listen))?;
        if raw.token.as_deref() == Some("") {
            bail!("token must not be empty; omit it to disable auth");
        }
        if raw.poll_interval_secs == 0 {
            bail!("poll_interval_secs must be > 0");
        }
        let mut windows = Vec::with_capacity(raw.windows.len());
        for (i, w) in raw.windows.iter().enumerate() {
            let ctx = || format!("windows[{i}]");
            let start = w.start.parse().with_context(ctx)?;
            let end = w.end.parse().with_context(ctx)?;
            let days = w
                .days
                .iter()
                .map(|d| parse_weekday(d))
                .collect::<Result<Vec<_>>>()
                .with_context(ctx)?;
            windows.push(Window::new(start, end, &days).with_context(ctx)?);
        }
        Ok(Self {
            device_id: raw.device_id,
            host,
            timezone,
            bind,
            listen,
            token: raw.token,
            poll_interval: Duration::from_secs(raw.poll_interval_secs),
            override_file: raw.override_file,
            season_file: raw.season_file,
            schedule: Schedule::new(windows),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../deploy/ecopumpd.toml");

    #[test]
    fn parses_example() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.device_id, "ECO-780D4D7D");
        assert_eq!(c.host, "10.1.1.169".parse::<IpAddr>().unwrap());
        assert_eq!(c.timezone, chrono_tz::America::Toronto);
        assert_eq!(c.poll_interval, Duration::from_secs(120));
        assert_eq!(c.schedule.windows().len(), 1);
    }

    #[test]
    fn defaults_and_errors() {
        let c = Config::parse("device_id='ECO-1'\nhost='10.0.0.1'\ntimezone='UTC'").unwrap();
        assert_eq!(c.poll_interval, Duration::from_secs(120));
        assert!(c.schedule.windows().is_empty());
        assert!(Config::parse("device_id='ECO-1'\nhost='nope'\ntimezone='UTC'").is_err());
        assert!(Config::parse("device_id='ECO-1'\nhost='10.0.0.1'\ntimezone='Mars/Olympus'").is_err());
        assert!(Config::parse("device_id='ECO-1'\nhost='10.0.0.1'\ntimezone='UTC'\nbogus=1").is_err());
        assert!(
            Config::parse("device_id='ECO-1'\nhost='10.0.0.1'\ntimezone='UTC'\n[[windows]]\nstart='9:00'\nend='9:00'")
                .is_err()
        );
    }
}
