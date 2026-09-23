//! The daemon's main loop: keep the plug's actual state equal to the desired one.
//!
//! Every poll the loop computes the desired state (override first, then
//! schedule), reads the plug, and corrects it only when they differ. This
//! self-heals after missed packets, power cuts and manual button presses.
//! Observations are published into [`State`] for the HTTP API, and the API
//! wakes the loop whenever an override changes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use chrono_tz::OffsetComponents;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use tracing::{debug, error, info, warn};

use crate::api::{self, State};
use crate::config::Config;
use crate::proto::{Client, Power};

/// Upper bound on the poll interval while the plug is unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Minimum gap between "unreachable" warnings.
const WARN_EVERY: Duration = Duration::from_secs(300);
/// After this long unreachable, warn that the outage is prolonged.
const PROLONGED_OUTAGE: Duration = Duration::from_secs(30 * 60);
/// Granularity at which the sleep checks for signals.
const TICK: Duration = Duration::from_secs(1);
/// How often the plug's DST flag is checked against the configured timezone.
const DST_CHECK_EVERY: Duration = Duration::from_secs(3600);
/// Extra delay past a scheduled transition so the poll lands just after it.
const TRANSITION_MARGIN: Duration = Duration::from_millis(500);

/// Shortens `wait` so the loop wakes right after `next` if that comes first.
///
/// Without this the poll phase drifts by the UDP round-trip every cycle and
/// a transition can be acted on up to a full poll interval late.
fn clamp_to_transition(wait: Duration, now: chrono::DateTime<Utc>, next: Option<chrono::DateTime<Utc>>) -> Duration {
    match next.and_then(|t| (t - now).to_std().ok()) {
        Some(until) => wait.min(until + TRANSITION_MARGIN),
        None => wait,
    }
}

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

    fn reload_pending(&self) -> bool {
        self.reload.load(Ordering::Relaxed)
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
            warn!(
                error = format!("{err:#}"),
                unreachable_for_secs = elapsed.as_secs(),
                "plug unreachable"
            );
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

/// Sleeps up to `dur`, returning early on shutdown, reload, or an API wake.
fn sleep_interruptible(dur: Duration, signals: &Signals, state: &State) {
    let deadline = Instant::now() + dur;
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        let before = Instant::now();
        state.wait(left.min(TICK));
        if signals.shutting_down() || signals.reload_pending() {
            return;
        }
        // Returned before the tick elapsed: the API woke us.
        if before.elapsed() < left.min(TICK) {
            debug!("woken by api");
            return;
        }
    }
}

/// Whether `tz` is observing daylight saving at `now`.
fn dst_active(tz: chrono_tz::Tz, now: chrono::DateTime<Utc>) -> bool {
    !now.with_timezone(&tz).offset().dst_offset().is_zero()
}

/// Makes the plug's DST flag match whether the configured timezone is in DST now.
///
/// The plug keeps standard local time and applies its timers an hour later
/// when the flag is set, so a stale flag makes on-device timers fire an hour off.
fn sync_dst(state: &State) -> Result<()> {
    let tz = state.with(|s| s.config.timezone);
    let want = dst_active(tz, Utc::now());
    let clock = state.plug_clock()?;
    if clock.dst == want {
        debug!(dst = want, plug_clock = %clock, "plug dst flag in sync");
        return Ok(());
    }
    let after = state.set_plug_dst(want)?;
    info!(from = clock.dst, to = want, plug_clock = %after, "corrected plug dst flag");
    Ok(())
}

/// Runs the reconcile loop and API server until SIGTERM or SIGINT.
///
/// `config_path` is re-read on SIGHUP; a bad new config is logged and the
/// previous one kept.
pub fn run(config_path: &Path, config: Config) -> Result<()> {
    let signals = Signals::register()?;
    let client = Client::bind(config.bind)?;
    info!(
        device_id = %config.device_id, host = %config.host, timezone = %config.timezone,
        poll_secs = config.poll_interval.as_secs(), windows = config.schedule.windows().len(),
        "ecopumpd starting"
    );
    let state = State::new(config).with_plug(client);
    api::serve(state.clone())?;
    let mut outage = Outage::default();
    let mut last_desired: Option<Power> = None;
    let mut last_dst_check: Option<Instant> = None;

    while !signals.shutting_down() {
        if signals.take_reload() {
            match Config::load(config_path) {
                Ok(c) => {
                    info!(windows = c.schedule.windows().len(), "config reloaded");
                    state.with(|s| s.config = c);
                }
                Err(e) => error!(error = %e, "config reload failed, keeping previous config"),
            }
        }

        let st = state.status();
        let (device_id, host, poll) =
            state.with(|s| (s.config.device_id.clone(), s.config.host, s.config.poll_interval));
        if last_desired != Some(st.desired) {
            let source = if st.r#override.is_some() {
                "override"
            } else {
                "schedule"
            };
            info!(desired = %st.desired, source, "desired state changed");
            last_desired = Some(st.desired);
        }

        let wait = match state.with_plug_client(|c| c.get_state(&device_id, host)) {
            Err(e) => {
                state.with(|s| {
                    s.reachable = false;
                    s.last_poll = Some(Utc::now());
                });
                outage.record_failure(&e, poll)
            }
            Ok(actual) => {
                outage.record_success();
                let mut observed = actual;
                if actual == st.desired {
                    debug!(state = %actual, "in sync");
                } else {
                    match state.with_plug_client(|c| c.set_state(&device_id, host, st.desired)) {
                        Ok(()) => {
                            observed = st.desired;
                            info!(from = %actual, to = %st.desired, "corrected plug state");
                        }
                        Err(e) => warn!(error = %e, wanted = %st.desired, "failed to correct plug state"),
                    }
                }
                state.with(|s| {
                    s.reachable = true;
                    s.actual = Some(observed);
                    s.last_poll = Some(Utc::now());
                });
                poll
            }
        };
        let wait = clamp_to_transition(wait, Utc::now(), st.next.map(|n| n.at));
        if outage.since.is_none() && last_dst_check.is_none_or(|t| t.elapsed() >= DST_CHECK_EVERY) {
            last_dst_check = Some(Instant::now());
            if let Err(e) = sync_dst(&state) {
                warn!(error = format!("{e:#}"), "could not check plug dst flag");
            }
        }
        sleep_interruptible(wait, &signals, &state);
    }
    info!("ecopumpd stopping");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn dst_detection() {
        let toronto = chrono_tz::America::Toronto;
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap();
        let january = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert!(dst_active(toronto, july));
        assert!(!dst_active(toronto, january));
        assert!(!dst_active(chrono_tz::UTC, july));
    }

    #[test]
    fn wakes_for_transition_when_sooner() {
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let poll = Duration::from_secs(120);
        let soon = now + chrono::Duration::seconds(30);
        assert_eq!(
            clamp_to_transition(poll, now, Some(soon)),
            Duration::from_millis(30_500)
        );
        let later = now + chrono::Duration::seconds(600);
        assert_eq!(clamp_to_transition(poll, now, Some(later)), poll);
        assert_eq!(clamp_to_transition(poll, now, None), poll);
        // A transition already in the past never yields a negative wait.
        assert_eq!(
            clamp_to_transition(poll, now, Some(now - chrono::Duration::seconds(5))),
            poll
        );
    }
}
