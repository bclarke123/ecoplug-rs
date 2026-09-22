//! The daemon's main loop: keep the plug's actual state equal to the desired one.
//!
//! Every poll the loop computes the desired state (override first, then
//! schedule), reads the plug, and corrects it only when they differ. This
//! self-heals after missed packets, power cuts and manual button presses.
//! The loop sleeps in short slices so it can wake early on SIGTERM, SIGHUP
//! (config reload) or a change to the override file.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::Utc;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::overrides;
use crate::proto::{Client, Power};

/// Upper bound on the poll interval while the plug is unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Minimum gap between "unreachable" warnings.
const WARN_EVERY: Duration = Duration::from_secs(300);
/// After this long unreachable, warn that the outage is prolonged.
const PROLONGED_OUTAGE: Duration = Duration::from_secs(30 * 60);
/// Granularity of the interruptible sleep.
const TICK: Duration = Duration::from_secs(1);

/// Process-wide signal flags.
#[derive(Debug, Clone)]
struct Signals {
    shutdown: Arc<AtomicBool>,
    reload: Arc<AtomicBool>,
}

impl Signals {
    fn register() -> Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));
        for sig in [SIGTERM, SIGINT] {
            signal_hook::flag::register(sig, Arc::clone(&shutdown)).context("register signal handler")?;
        }
        signal_hook::flag::register(SIGHUP, Arc::clone(&reload)).context("register SIGHUP handler")?;
        Ok(Self { shutdown, reload })
    }

    fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn take_reload(&self) -> bool {
        self.reload.swap(false, Ordering::Relaxed)
    }
}

/// Tracks how long the plug has been unreachable and rate-limits warnings.
#[derive(Debug, Default)]
struct Outage {
    since: Option<Instant>,
    last_warned: Option<Instant>,
    prolonged_warned: bool,
    backoff: Option<Duration>,
}

impl Outage {
    fn record_failure(&mut self, err: &anyhow::Error, poll: Duration) -> Duration {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if self.last_warned.is_none_or(|t| now.duration_since(t) >= WARN_EVERY) {
            warn!(error = %err, unreachable_for_secs = elapsed.as_secs(), "plug unreachable");
            self.last_warned = Some(now);
        }
        if elapsed >= PROLONGED_OUTAGE && !self.prolonged_warned {
            warn!(
                unreachable_for_secs = elapsed.as_secs(),
                "plug has been unreachable for over 30 minutes"
            );
            self.prolonged_warned = true;
        }
        let next = self
            .backoff
            .map_or(poll, |b| (b * 2).min(MAX_BACKOFF))
            .max(poll)
            .min(MAX_BACKOFF);
        self.backoff = Some(next);
        next
    }

    fn record_success(&mut self) {
        if let Some(since) = self.since.take() {
            info!(outage_secs = since.elapsed().as_secs(), "plug reachable again");
        }
        self.last_warned = None;
        self.prolonged_warned = false;
        self.backoff = None;
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Sleeps up to `dur`, returning early on shutdown, reload, or override change.
fn sleep_interruptible(dur: Duration, signals: &Signals, override_file: &Path) {
    let start_mtime = mtime(override_file);
    let deadline = Instant::now() + dur;
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        thread::sleep(left.min(TICK));
        if signals.shutting_down() || signals.reload.load(Ordering::Relaxed) {
            return;
        }
        if mtime(override_file) != start_mtime {
            info!("override file changed, waking early");
            return;
        }
    }
}

/// Runs the reconcile loop until SIGTERM or SIGINT.
///
/// `config_path` is re-read on SIGHUP; a bad new config is logged and the
/// previous one kept.
pub fn run(config_path: &Path, mut config: Config) -> Result<()> {
    let signals = Signals::register()?;
    let client = Client::bind(config.bind)?;
    let mut outage = Outage::default();
    let mut last_desired: Option<Power> = None;
    info!(
        device_id = %config.device_id, host = %config.host, timezone = %config.timezone,
        poll_secs = config.poll_interval.as_secs(), windows = config.schedule.windows().len(),
        "ecopumpd starting"
    );

    while !signals.shutting_down() {
        if signals.take_reload() {
            match Config::load(config_path) {
                Ok(c) => {
                    info!(windows = c.schedule.windows().len(), "config reloaded");
                    config = c;
                }
                Err(e) => error!(error = %e, "config reload failed, keeping previous config"),
            }
        }

        let now = Utc::now();
        let ov = match overrides::read(&config.override_file, now) {
            Ok(ov) => ov,
            Err(e) => {
                error!(error = %e, "cannot read override file, using schedule");
                None
            }
        };
        let scheduled = Power::from_bool(config.schedule.desired(now.with_timezone(&config.timezone)));
        let desired = ov.map_or(scheduled, |o| o.power);
        if last_desired != Some(desired) {
            let source = if ov.is_some() { "override" } else { "schedule" };
            info!(%desired, source, "desired state changed");
            last_desired = Some(desired);
        }

        let wait = match client.get_state(&config.device_id, config.host) {
            Err(e) => outage.record_failure(&e, config.poll_interval),
            Ok(actual) => {
                outage.record_success();
                if actual == desired {
                    debug!(state = %actual, "in sync");
                } else {
                    match client.set_state(&config.device_id, config.host, desired) {
                        Ok(()) => info!(from = %actual, to = %desired, "corrected plug state"),
                        Err(e) => warn!(error = %e, wanted = %desired, "failed to correct plug state"),
                    }
                }
                config.poll_interval
            }
        };
        sleep_interruptible(wait, &signals, &config.override_file);
    }
    info!("ecopumpd stopping");
    Ok(())
}
