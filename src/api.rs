//! HTTP API served by the daemon and the JSON types shared with the CLI client.
//!
//! The daemon is the only process that can talk to the plug (it owns UDP port
//! 9000), so everything else goes through this API:
//!
//! | Method | Path        | Body                                   | Effect |
//! |--------|-------------|----------------------------------------|--------|
//! | GET    | `/status`   |                                        | [`Status`] |
//! | POST   | `/override` | `{"power":"on","until":<unix secs>}`   | set override |
//! | DELETE | `/override` |                                        | clear override |
//! | POST   | `/set`      | `{"power":"off"}`                      | override until the next scheduled transition |
//! | GET    | `/metrics`  |                                        | Prometheus text |
//! | GET    | `/clock`    |                                        | the plug's clock and DST flag |
//! | POST   | `/clock/dst`| `{"on":true}`                          | set the plug's DST flag |
//! | GET    | `/schedule` |                                        | the plug's on-device timer table |
//! | POST   | `/schedule/sync` |                                   | replace it with the config windows |
//! | DELETE | `/schedule` |                                        | remove every on-device timer |
//!
//! Errors are `{"error": "..."}` with a 4xx/5xx status. When a `token` is
//! configured, requests must carry `Authorization: Bearer <token>`.

use std::fmt::Write as _;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, Utc};
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::overrides::{self, Override};
use crate::proto::{self, PlugClock, Power, ScheduleEntry, ScheduleTable};

/// Fallback override length used by `/set` when the schedule never changes.
const SET_FALLBACK: Duration = Duration::hours(24);

/// Relay state as it appears on the wire (`"on"` / `"off"`).
impl Serialize for Power {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(if self.is_on() { "on" } else { "off" })
    }
}

impl<'de> Deserialize<'de> for Power {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match String::deserialize(d)?.as_str() {
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            other => Err(serde::de::Error::custom(format!(
                "expected \"on\" or \"off\", got {other:?}"
            ))),
        }
    }
}

/// An active override as reported by the API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverrideInfo {
    pub power: Power,
    pub until: DateTime<Utc>,
}

/// The next scheduled change.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transition {
    pub power: Power,
    pub at: DateTime<Utc>,
}

/// Everything the daemon knows, as returned by `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub now: DateTime<Utc>,
    pub timezone: String,
    /// What the schedule alone wants right now.
    pub schedule: Power,
    pub r#override: Option<OverrideInfo>,
    /// What the daemon is trying to hold the plug at.
    pub desired: Power,
    /// Last state read from the plug, if it has ever answered.
    pub actual: Option<Power>,
    pub last_poll: Option<DateTime<Utc>>,
    /// Whether the last poll succeeded.
    pub reachable: bool,
    pub next: Option<Transition>,
}

/// Body of `POST /override`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct OverrideRequest {
    pub power: Power,
    pub until: DateTime<Utc>,
}

/// Body of `POST /set`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SetRequest {
    pub power: Power,
}

/// Body of `POST /clock/dst`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DstRequest {
    pub on: bool,
}

/// Error body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// State shared between the reconcile loop and the API thread.
#[derive(Debug)]
pub struct Shared {
    pub config: Config,
    pub r#override: Option<Override>,
    pub actual: Option<Power>,
    pub last_poll: Option<DateTime<Utc>>,
    pub reachable: bool,
    /// Set by the API to make the loop re-evaluate immediately.
    pub wake: bool,
}

/// Handle to [`Shared`] plus the condvar used to wake the loop and the
/// UDP client shared with the API thread.
#[derive(Debug, Clone)]
pub struct State {
    inner: Arc<(Mutex<Shared>, Condvar)>,
    plug: Option<Arc<Mutex<proto::Client>>>,
}

impl State {
    /// Creates state for `config` with the override loaded from disk.
    pub fn new(config: Config) -> Self {
        let r#override = overrides::read(&config.override_file, Utc::now()).unwrap_or_else(|e| {
            warn!(error = %e, "cannot read override file");
            None
        });
        let shared = Shared {
            config,
            r#override,
            actual: None,
            last_poll: None,
            reachable: false,
            wake: false,
        };
        Self {
            inner: Arc::new((Mutex::new(shared), Condvar::new())),
            plug: None,
        }
    }

    /// Attaches the UDP client so both the loop and the API can use it.
    pub fn with_plug(mut self, client: proto::Client) -> Self {
        self.plug = Some(Arc::new(Mutex::new(client)));
        self
    }

    /// Runs `f` with exclusive use of the UDP client.
    pub fn with_plug_client<R>(&self, f: impl FnOnce(&proto::Client) -> Result<R>) -> Result<R> {
        let plug = self.plug.as_ref().ok_or_else(|| anyhow!("no plug client attached"))?;
        let guard = plug.lock().unwrap_or_else(|p| p.into_inner());
        f(&guard)
    }

    fn device(&self) -> (String, std::net::IpAddr) {
        self.with(|s| (s.config.device_id.clone(), s.config.host))
    }

    /// Reads the plug's clock and DST flag.
    pub fn plug_clock(&self) -> Result<PlugClock> {
        let (id, host) = self.device();
        self.with_plug_client(|c| c.get_clock(&id, host))
    }

    /// Sets the plug's DST flag and reads the clock back.
    pub fn set_plug_dst(&self, on: bool) -> Result<PlugClock> {
        let (id, host) = self.device();
        self.with_plug_client(|c| {
            c.set_dst(&id, host, on)?;
            info!(on, "set plug dst flag");
            c.get_clock(&id, host)
        })
    }

    /// Reads the on-device timer table.
    pub fn plug_schedule(&self) -> Result<ScheduleTable> {
        let (id, host) = self.device();
        self.with_plug_client(|c| c.get_schedule(&id, host))
    }

    /// Deletes every on-device timer, one at a time as the app does.
    pub fn clear_plug_schedule(&self) -> Result<ScheduleTable> {
        let (id, host) = self.device();
        self.with_plug_client(|c| {
            let mut table = c.get_schedule(&id, host)?;
            while let Some(e) = table.entries.first().copied() {
                c.delete_schedule_entry(&id, host, &mut table, e.id)?;
                info!(entry = %e, "removed on-device timer");
            }
            Ok(table)
        })
    }

    /// Replaces the on-device timers with the configured windows.
    pub fn sync_plug_schedule(&self) -> Result<ScheduleTable> {
        let (windows, tz) = self.with(|s| (s.config.schedule.windows().to_vec(), s.config.timezone));
        if windows.len() > proto::SCHEDULE_SLOTS {
            bail!(
                "config has {} windows but the plug stores at most {}",
                windows.len(),
                proto::SCHEDULE_SLOTS
            );
        }
        let mut table = self.clear_plug_schedule()?;
        let (id, host) = self.device();
        let today = Utc::now().with_timezone(&tz);
        let today = (today.year() as u16, today.month() as u8, today.day() as u8);
        self.with_plug_client(|c| {
            for w in &windows {
                let slot = table.free_id().ok_or_else(|| anyhow!("no free timer slot"))?;
                let entry = ScheduleEntry::weekly(
                    slot,
                    w.days_mask(),
                    u32::from(w.start().minutes()) * 60,
                    u32::from(w.end().minutes()) * 60,
                    today,
                );
                c.add_schedule_entry(&id, host, &mut table, entry)?;
                info!(entry = %entry, "added on-device timer");
            }
            Ok(table)
        })
    }

    /// Runs `f` with the lock held.
    pub fn with<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        let mut guard = self.inner.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    /// Blocks up to `timeout` or until [`State::wake`] is called.
    pub fn wait(&self, timeout: std::time::Duration) {
        let (lock, cv) = &*self.inner;
        let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let (mut guard, _) = cv
            .wait_timeout_while(guard, timeout, |s| !s.wake)
            .unwrap_or_else(|p| p.into_inner());
        guard.wake = false;
    }

    /// Wakes the reconcile loop.
    pub fn wake(&self) {
        self.with(|s| s.wake = true);
        self.inner.1.notify_all();
    }

    /// Drops any expired override and returns the active one.
    pub fn active_override(&self, now: DateTime<Utc>) -> Option<Override> {
        self.with(|s| {
            if let Some(ov) = s.r#override
                && ov.is_expired(now)
            {
                info!(r#override = %ov, "override expired");
                s.r#override = None;
                if let Err(e) = overrides::clear(&s.config.override_file) {
                    warn!(error = %e, "cannot remove expired override file");
                }
            }
            s.r#override
        })
    }

    /// Installs (or clears) an override, persists it, and wakes the loop.
    pub fn set_override(&self, ov: Option<Override>) -> Result<()> {
        self.with(|s| {
            let path = &s.config.override_file;
            match ov {
                Some(o) => overrides::write(path, o)?,
                None => overrides::clear(path)?,
            }
            s.r#override = ov;
            Ok::<(), anyhow::Error>(())
        })?;
        match ov {
            Some(o) => info!(r#override = %o, "override set"),
            None => info!("override cleared"),
        }
        self.wake();
        Ok(())
    }

    /// Builds the status snapshot.
    pub fn status(&self) -> Status {
        let now = Utc::now();
        let r#override = self.active_override(now);
        self.with(|s| {
            let local = now.with_timezone(&s.config.timezone);
            let schedule = Power::from_bool(s.config.schedule.desired(local));
            Status {
                now,
                timezone: s.config.timezone.name().to_owned(),
                schedule,
                r#override: r#override.map(|o| OverrideInfo {
                    power: o.power,
                    until: o.until,
                }),
                desired: r#override.map_or(schedule, |o| o.power),
                actual: s.actual,
                last_poll: s.last_poll,
                reachable: s.reachable,
                next: s.config.schedule.next_transition(local).map(|(t, on)| Transition {
                    power: Power::from_bool(on),
                    at: t.with_timezone(&Utc),
                }),
            }
        })
    }
}

fn json<T: Serialize>(code: u16, body: &T) -> Response<std::io::Cursor<Vec<u8>>> {
    let text = serde_json::to_vec(body).unwrap_or_default();
    Response::from_data(text)
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("static header"))
}

fn err(code: u16, msg: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    json(code, &ErrorBody { error: msg.into() })
}

fn metrics(st: &Status) -> String {
    let mut out = String::new();
    let b = |p: Option<Power>| u8::from(p.is_some_and(Power::is_on));
    let _ = writeln!(
        out,
        "# TYPE ecopumpd_desired gauge\necopumpd_desired {}",
        b(Some(st.desired))
    );
    let _ = writeln!(out, "# TYPE ecopumpd_actual gauge\necopumpd_actual {}", b(st.actual));
    let _ = writeln!(
        out,
        "# TYPE ecopumpd_reachable gauge\necopumpd_reachable {}",
        u8::from(st.reachable)
    );
    let _ = writeln!(
        out,
        "# TYPE ecopumpd_override_active gauge\necopumpd_override_active {}",
        u8::from(st.r#override.is_some())
    );
    if let Some(t) = st.last_poll {
        let _ = writeln!(
            out,
            "# TYPE ecopumpd_last_poll_seconds gauge\necopumpd_last_poll_seconds {}",
            t.timestamp()
        );
    }
    out
}

fn read_body<T: for<'de> Deserialize<'de>>(req: &mut Request) -> Result<T> {
    let mut buf = String::new();
    req.as_reader()
        .take(64 * 1024)
        .read_to_string(&mut buf)
        .context("read body")?;
    serde_json::from_str(&buf).map_err(|e| anyhow!("invalid JSON body: {e}"))
}

fn authorized(req: &Request, token: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .is_some_and(|h| h.value.as_str().strip_prefix("Bearer ") == Some(token))
}

fn handle(state: &State, mut req: Request) {
    let token = state.with(|s| s.config.token.clone());
    let response = if !authorized(&req, token.as_deref()) {
        err(401, "missing or invalid bearer token")
    } else {
        match (req.method(), req.url()) {
            (Method::Get, "/status") => json(200, &state.status()),
            (Method::Get, "/metrics") => Response::from_string(metrics(&state.status()))
                .with_header(Header::from_bytes("Content-Type", "text/plain; version=0.0.4").expect("static header")),
            (Method::Post, "/override") => match read_body::<OverrideRequest>(&mut req) {
                Err(e) => err(400, e.to_string()),
                Ok(r) if r.until <= Utc::now() => err(400, "until is in the past"),
                Ok(r) => match state.set_override(Some(Override {
                    power: r.power,
                    until: r.until,
                })) {
                    Ok(()) => json(200, &state.status()),
                    Err(e) => err(500, format!("{e:#}")),
                },
            },
            (Method::Delete, "/override") => match state.set_override(None) {
                Ok(()) => json(200, &state.status()),
                Err(e) => err(500, format!("{e:#}")),
            },
            (Method::Post, "/set") => match read_body::<SetRequest>(&mut req) {
                Err(e) => err(400, e.to_string()),
                Ok(r) => {
                    let now = Utc::now();
                    let until = state
                        .status()
                        .next
                        .map_or(now + SET_FALLBACK, |t| t.at.max(now + Duration::minutes(1)));
                    match state.set_override(Some(Override { power: r.power, until })) {
                        Ok(()) => json(200, &state.status()),
                        Err(e) => err(500, format!("{e:#}")),
                    }
                }
            },
            (Method::Get, "/clock") => match state.plug_clock() {
                Ok(c) => json(200, &c),
                Err(e) => err(502, format!("{e:#}")),
            },
            (Method::Post, "/clock/dst") => match read_body::<DstRequest>(&mut req) {
                Err(e) => err(400, e.to_string()),
                Ok(r) => match state.set_plug_dst(r.on) {
                    Ok(c) => json(200, &c),
                    Err(e) => err(502, format!("{e:#}")),
                },
            },
            (Method::Get, "/schedule") => match state.plug_schedule() {
                Ok(t) => json(200, &t),
                Err(e) => err(502, format!("{e:#}")),
            },
            (Method::Post, "/schedule/sync") => match state.sync_plug_schedule() {
                Ok(t) => json(200, &t),
                Err(e) => err(502, format!("{e:#}")),
            },
            (Method::Delete, "/schedule") => match state.clear_plug_schedule() {
                Ok(t) => json(200, &t),
                Err(e) => err(502, format!("{e:#}")),
            },
            _ => err(404, "no such endpoint"),
        }
    };
    debug!(method = %req.method(), url = req.url(), code = response.status_code().0, "api request");
    if let Err(e) = req.respond(response) {
        warn!(error = %e, "failed to send api response");
    }
}

/// Starts the API server on a background thread.
pub fn serve(state: State) -> Result<()> {
    let listen = state.with(|s| s.config.listen);
    let server = Server::http(listen).map_err(|e| anyhow!("listen on {listen}: {e}"))?;
    info!(%listen, "api listening");
    thread::Builder::new()
        .name("api".into())
        .spawn(move || {
            for req in server.incoming_requests() {
                handle(&state, req);
            }
            error!("api server stopped accepting requests");
        })
        .context("spawn api thread")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_json_round_trip() {
        assert_eq!(serde_json::to_string(&Power::On).unwrap(), "\"on\"");
        assert_eq!(serde_json::from_str::<Power>("\"off\"").unwrap(), Power::Off);
        assert!(serde_json::from_str::<Power>("\"maybe\"").is_err());
    }

    #[test]
    fn status_reflects_override_and_schedule() {
        let mut cfg = Config::parse(include_str!("../deploy/ecopumpd.toml")).unwrap();
        let dir = std::env::temp_dir().join(format!("ecopumpd-api-{}", std::process::id()));
        cfg.override_file = dir.join("override");
        let state = State::new(cfg);
        let st = state.status();
        assert!(st.r#override.is_none());
        assert_eq!(st.desired, st.schedule);
        assert!(st.actual.is_none());
        let until = Utc::now() + Duration::hours(1);
        state
            .set_override(Some(Override {
                power: Power::On,
                until,
            }))
            .unwrap();
        let st = state.status();
        assert_eq!(st.desired, Power::On);
        assert_eq!(st.r#override.map(|o| o.until.timestamp()), Some(until.timestamp()));
        state.set_override(None).unwrap();
        assert!(state.status().r#override.is_none());
    }

    #[test]
    fn metrics_format() {
        let cfg = Config::parse(include_str!("../deploy/ecopumpd.toml")).unwrap();
        let text = metrics(&State::new(cfg).status());
        assert!(text.contains("ecopumpd_reachable 0"));
        assert!(text.contains("ecopumpd_override_active 0"));
    }
}
