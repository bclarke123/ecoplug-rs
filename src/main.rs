//! `ecopumpd`: schedule and control an ECO Plugs pool-pump timer over local UDP.
//!
//! Run `ecopumpd --help` for the subcommands. The `run` subcommand is the
//! long-lived daemon; the others are one-shot tools that replace the old
//! Python script.

mod config;
mod overrides;
mod proto;
mod reconcile;
mod schedule;

use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration as ChronoDuration, Utc};
use clap::{Args, Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::Config;
use overrides::Override;
use proto::{Client, Power};
use schedule::Hm;

#[derive(Debug, Parser)]
#[command(name = "ecopumpd", version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the reconcile loop as a daemon.
    Run,
    /// Broadcast discovery and list every plug that answers.
    Discover {
        /// Broadcast address to use. By default both 255.255.255.255 and the
        /// /24 directed broadcast of the config host are tried.
        #[arg(long)]
        broadcast: Option<Ipv4Addr>,
    },
    /// Print the plug's current relay state.
    State,
    /// Switch the plug on.
    On,
    /// Switch the plug off.
    Off,
    /// Manage a temporary override of the schedule.
    #[command(subcommand)]
    Override(OverrideCmd),
    /// Show desired vs actual state, the active override and the next transition.
    Status,
}

#[derive(Debug, Subcommand)]
enum OverrideCmd {
    /// Force the pump on.
    On(Until),
    /// Force the pump off.
    Off(Until),
    /// Remove any override.
    Clear,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct Until {
    /// Duration such as 2h, 90m, 1h30m.
    #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
    for_: Option<Duration>,
    /// Local time of day (HH:MM); the next occurrence is used.
    #[arg(long, value_name = "HH:MM")]
    until: Option<Hm>,
}

/// Parses `2h`, `90m`, `1h30m`, `45s` style durations.
fn parse_duration(s: &str) -> Result<Duration> {
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num
            .parse()
            .map_err(|_| anyhow!("expected a number before {c:?} in {s:?}"))?;
        num.clear();
        total += match c {
            'd' => n * 86_400,
            'h' => n * 3600,
            'm' => n * 60,
            's' => n,
            _ => bail!("unknown unit {c:?} in {s:?}; use d, h, m or s"),
        };
    }
    if !num.is_empty() {
        bail!("missing unit after {num} in {s:?}");
    }
    if total == 0 {
        bail!("duration must be positive");
    }
    Ok(Duration::from_secs(total))
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(false)
        .init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            reconcile::run(&cli.config, cfg)
        }
        Cmd::Discover { broadcast } => discover(&cli.config, broadcast),
        Cmd::State => {
            let cfg = Config::load(&cli.config)?;
            let state = Client::bind(cfg.bind)?.get_state(&cfg.device_id, cfg.host)?;
            println!("{state}");
            Ok(())
        }
        Cmd::On => set(&cli.config, Power::On),
        Cmd::Off => set(&cli.config, Power::Off),
        Cmd::Override(cmd) => override_cmd(&cli.config, cmd),
        Cmd::Status => status(&cli.config),
    }
}

fn set(config: &Path, power: Power) -> Result<()> {
    let cfg = Config::load(config)?;
    Client::bind(cfg.bind)?.set_state(&cfg.device_id, cfg.host, power)?;
    println!("{power}");
    Ok(())
}

fn discover(config: &Path, broadcast: Option<Ipv4Addr>) -> Result<()> {
    let cfg = Config::load(config).ok();
    let local = cfg.as_ref().map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |c| c.bind);
    let mut targets = vec![IpAddr::V4(Ipv4Addr::BROADCAST)];
    match (broadcast, cfg.as_ref().map(|c| c.host)) {
        (Some(b), _) => targets = vec![IpAddr::V4(b)],
        (None, Some(IpAddr::V4(h))) => {
            let [a, b, c, _] = h.octets();
            targets.insert(0, IpAddr::V4(Ipv4Addr::new(a, b, c, 255)));
        }
        _ => {}
    }
    info!(?targets, "broadcasting discovery");
    let devices = Client::bind(local)?.discover(&targets)?;
    if devices.is_empty() {
        bail!("no devices answered discovery on {targets:?}");
    }
    for d in devices {
        println!("{d}");
    }
    Ok(())
}

fn override_cmd(config: &Path, cmd: OverrideCmd) -> Result<()> {
    let cfg = Config::load(config)?;
    let path = &cfg.override_file;
    let (power, until) = match cmd {
        OverrideCmd::Clear => {
            overrides::clear(path)?;
            println!("override cleared");
            return Ok(());
        }
        OverrideCmd::On(u) => (Power::On, u),
        OverrideCmd::Off(u) => (Power::Off, u),
    };
    let now = Utc::now();
    let until = match (until.for_, until.until) {
        (Some(d), _) => now + ChronoDuration::from_std(d).context("duration too large")?,
        (None, Some(hm)) => next_occurrence(&cfg, hm),
        (None, None) => bail!("specify --for or --until"),
    };
    let ov = Override { power, until };
    overrides::write(path, ov)?;
    println!(
        "override {} (local {})",
        ov,
        until.with_timezone(&cfg.timezone).format("%Y-%m-%d %H:%M %Z")
    );
    println!("the daemon applies it within its poll interval");
    Ok(())
}

/// The next time `hm` occurs in the configured timezone, strictly after now.
fn next_occurrence(cfg: &Config, hm: Hm) -> chrono::DateTime<Utc> {
    use chrono::{NaiveTime, TimeZone};
    let now = Utc::now().with_timezone(&cfg.timezone);
    let time = NaiveTime::from_hms_opt(u32::from(hm.minutes() / 60), u32::from(hm.minutes() % 60), 0)
        .expect("Hm is validated");
    for offset in 0..3 {
        let date = now.date_naive() + ChronoDuration::days(offset);
        // `earliest` skips a wall time that does not exist on a DST day.
        if let Some(t) = cfg.timezone.from_local_datetime(&date.and_time(time)).earliest()
            && t > now
        {
            return t.with_timezone(&Utc);
        }
    }
    unreachable!("a valid HH:MM occurs within three days")
}

fn status(config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let now = Utc::now();
    let local = now.with_timezone(&cfg.timezone);
    let scheduled = Power::from_bool(cfg.schedule.desired(local));
    let ov = overrides::read(&cfg.override_file, now)?;
    let desired = ov.map_or(scheduled, |o| o.power);
    println!("now:        {}", local.format("%Y-%m-%d %H:%M:%S %Z"));
    println!("schedule:   {scheduled}");
    match ov {
        Some(o) => println!(
            "override:   {} until {}",
            o.power,
            o.until.with_timezone(&cfg.timezone).format("%Y-%m-%d %H:%M %Z")
        ),
        None => println!("override:   none"),
    }
    println!("desired:    {desired}");
    match Client::bind(cfg.bind).and_then(|c| c.get_state(&cfg.device_id, cfg.host)) {
        Ok(actual) => println!(
            "actual:     {actual}{}",
            if actual == desired { "" } else { "  (out of sync)" }
        ),
        Err(e) => println!("actual:     unknown ({e:#})"),
    }
    match cfg.schedule.next_transition(local) {
        Some((t, on)) => println!(
            "next:       {} at {}",
            Power::from_bool(on),
            t.format("%Y-%m-%d %H:%M %Z")
        ),
        None => println!("next:       none within a week"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("2x").is_err());
        assert!(parse_duration("0m").is_err());
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["ecopumpd", "override", "on", "--for", "2h"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Override(OverrideCmd::On(Until {
                for_: Some(_),
                until: None
            }))
        ));
        assert!(Cli::try_parse_from(["ecopumpd", "override", "on"]).is_err());
    }
}
