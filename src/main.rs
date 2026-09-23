//! `ecopumpd`: schedule and control an ECO Plugs pool-pump timer over local UDP.
//!
//! Run `ecopumpd --help` for the subcommands. `run` is the long-lived daemon
//! and the only thing that talks to the plug; every other subcommand except
//! `discover` is a client of the daemon's HTTP API and fails if it is down.

mod api;
mod client;
mod config;
mod overrides;
mod proto;
mod reconcile;
mod schedule;
mod season;

use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use clap::{Args, Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use api::{OverrideRequest, SetRequest, Status, WinterRequest};
use chrono::NaiveDate;
use config::Config;
use proto::Power;
use schedule::Hm;

#[derive(Debug, Parser)]
#[command(name = "ecopumpd", version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    /// Print raw JSON instead of a human-readable summary.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the reconcile loop and HTTP API as a daemon.
    Run,
    /// Broadcast discovery and list every plug that answers (talks to the
    /// plug directly; stop the daemon first).
    Discover {
        /// Broadcast address to use. By default both 255.255.255.255 and the
        /// /24 directed broadcast of the config host are tried.
        #[arg(long)]
        broadcast: Option<Ipv4Addr>,
    },
    /// Print the plug's last observed relay state.
    State,
    /// Hold the pump on until the next scheduled transition.
    On,
    /// Hold the pump off until the next scheduled transition.
    Off,
    /// Manage a temporary override of the schedule.
    #[command(subcommand)]
    Override(OverrideCmd),
    /// Show desired vs actual state, the active override and the next transition.
    Status,
    /// Inspect or program the plug's own on-device timers.
    #[command(subcommand)]
    Schedule(ScheduleCmd),
    /// Inspect or adjust the plug's clock.
    #[command(subcommand)]
    Clock(ClockCmd),
    /// Read live power draw and this month's energy, if the plug has a meter.
    Power,
    /// Winter mode: hands off the relay and no on-device timers between two dates.
    #[command(subcommand)]
    Winter(WinterCmd),
}

#[derive(Debug, Subcommand)]
enum WinterCmd {
    /// Start winter mode on DATE (YYYY-MM-DD, today or earlier starts now). Keeps any end date.
    Start { date: NaiveDate },
    /// Resume normal operation on DATE. Starts winter today if none is set.
    End { date: NaiveDate },
    /// Cancel winter mode now; on-device timers are restored on the next poll.
    Cancel,
}

#[derive(Debug, Subcommand)]
enum ClockCmd {
    /// Show the plug's clock and DST flag.
    Show,
    /// Turn the plug's daylight-saving flag on or off.
    Dst {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Debug, Subcommand)]
enum ScheduleCmd {
    /// List the timers stored on the plug.
    Show,
    /// Replace the plug's timers with the windows from the config file.
    Sync,
    /// Delete every timer stored on the plug.
    Clear,
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

fn api_client(cfg: &Config) -> client::Client {
    client::Client::new(cfg.listen, cfg.token.clone())
}

fn dispatch(cli: Cli) -> Result<()> {
    if let Cmd::Discover { broadcast } = cli.cmd {
        return discover(&cli.config, broadcast);
    }
    let cfg = Config::load(&cli.config)?;
    if let Cmd::Run = cli.cmd {
        return reconcile::run(&cli.config, cfg);
    }
    let api = api_client(&cfg);
    if let Cmd::Winter(wc) = cli.cmd {
        let status = match wc {
            WinterCmd::Start { date } => {
                let end = api.status()?.winter.and_then(|w| w.end);
                api.set_winter(WinterRequest { start: date, end })?
            }
            WinterCmd::End { date } => {
                let start = api
                    .status()?
                    .winter
                    .map_or_else(|| Utc::now().with_timezone(&cfg.timezone).date_naive(), |w| w.start);
                api.set_winter(WinterRequest { start, end: Some(date) })?
            }
            WinterCmd::Cancel => api.cancel_winter()?,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            print_status(&status, cfg.timezone);
        }
        return Ok(());
    }
    if let Cmd::Power = cli.cmd {
        let p = api.power()?;
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&p)?);
        } else {
            println!("{p}");
        }
        return Ok(());
    }
    if let Cmd::Clock(cc) = cli.cmd {
        let clock = match cc {
            ClockCmd::Show => api.clock()?,
            ClockCmd::Dst { state } => api.set_dst(state == "on")?,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&clock)?);
        } else {
            println!("{clock}");
        }
        return Ok(());
    }
    if let Cmd::Schedule(sc) = cli.cmd {
        let table = match sc {
            ScheduleCmd::Show => api.schedule()?,
            ScheduleCmd::Sync => api.sync_schedule()?,
            ScheduleCmd::Clear => api.clear_schedule()?,
        };
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&table)?);
        } else if table.entries.is_empty() {
            println!("no timers stored on the plug");
        } else {
            for e in &table.entries {
                println!("{e}");
            }
        }
        return Ok(());
    }
    let status = match cli.cmd {
        Cmd::State => {
            let st = api.status()?;
            match st.actual {
                Some(p) => println!("{p}"),
                None => bail!("plug has not answered yet"),
            }
            return Ok(());
        }
        Cmd::On => api.set(SetRequest { power: Power::On })?,
        Cmd::Off => api.set(SetRequest { power: Power::Off })?,
        Cmd::Override(OverrideCmd::Clear) => api.clear_override()?,
        Cmd::Override(OverrideCmd::On(u)) => api.set_override(OverrideRequest {
            power: Power::On,
            until: until_time(&cfg, u)?,
        })?,
        Cmd::Override(OverrideCmd::Off(u)) => api.set_override(OverrideRequest {
            power: Power::Off,
            until: until_time(&cfg, u)?,
        })?,
        Cmd::Status => api.status()?,
        Cmd::Run | Cmd::Discover { .. } | Cmd::Schedule(_) | Cmd::Clock(_) | Cmd::Power | Cmd::Winter(_) => {
            unreachable!("handled above")
        }
    };
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status, cfg.timezone);
    }
    Ok(())
}

fn until_time(cfg: &Config, u: Until) -> Result<DateTime<Utc>> {
    match (u.for_, u.until) {
        (Some(d), _) => Ok(Utc::now() + ChronoDuration::from_std(d).context("duration too large")?),
        (None, Some(hm)) => Ok(next_occurrence(cfg.timezone, hm)),
        (None, None) => bail!("specify --for or --until"),
    }
}

/// The next time `hm` occurs in `tz`, strictly after now.
fn next_occurrence(tz: Tz, hm: Hm) -> DateTime<Utc> {
    use chrono::{NaiveTime, TimeZone};
    let now = Utc::now().with_timezone(&tz);
    let time = NaiveTime::from_hms_opt(u32::from(hm.minutes() / 60), u32::from(hm.minutes() % 60), 0)
        .expect("Hm is validated");
    for offset in 0..3 {
        let date = now.date_naive() + ChronoDuration::days(offset);
        // `earliest` skips a wall time that does not exist on a DST day.
        if let Some(t) = tz.from_local_datetime(&date.and_time(time)).earliest()
            && t > now
        {
            return t.with_timezone(&Utc);
        }
    }
    unreachable!("a valid HH:MM occurs within three days")
}

fn print_status(st: &Status, tz: Tz) {
    let fmt = |t: DateTime<Utc>| t.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S %Z").to_string();
    println!("now:        {}", fmt(st.now));
    println!("schedule:   {}", st.schedule);
    match st.r#override {
        Some(o) => println!("override:   {} until {}", o.power, fmt(o.until)),
        None => println!("override:   none"),
    }
    match st.winter {
        Some(w) => {
            let end = w.end.map_or("cancelled".to_owned(), |e| e.to_string());
            let phase = if w.active {
                if w.timers_cleared {
                    "active, hands off"
                } else {
                    "active, timers not yet cleared"
                }
            } else {
                "scheduled"
            };
            println!("winter:     {} until {} ({phase})", w.start, end);
        }
        None => println!("winter:     off"),
    }
    if st.winter.is_some_and(|w| w.active) {
        println!("desired:    hands off (winter)");
    } else {
        println!("desired:    {}", st.desired);
    }
    match (st.actual, st.last_poll) {
        (Some(a), Some(t)) => {
            let sync = if a == st.desired { "" } else { "  (out of sync)" };
            let stale = if st.reachable { "" } else { ", plug unreachable" };
            println!("actual:     {a}{sync}  (as of {}{stale})", fmt(t));
        }
        _ => println!("actual:     unknown (plug has not answered yet)"),
    }
    match st.next {
        Some(n) => println!("next:       {} at {}", n.power, fmt(n.at)),
        None => println!("next:       none within a week"),
    }
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
    let devices = proto::Client::bind(local)?.discover(&targets)?;
    if devices.is_empty() {
        bail!("no devices answered discovery on {targets:?}");
    }
    for d in devices {
        println!("{d}");
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
