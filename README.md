# ecopumpd

A small Rust daemon that keeps an ECO Plugs / WiOn / Dewenwils Wi-Fi outlet
(such as the HOWT01A pool-pump timer) on a schedule using the outlet's local
UDP protocol. No vendor cloud, no app, no Home Assistant required.

It reconciles rather than fires and forgets: every poll it computes the desired
state from the schedule, reads the plug's actual state, and corrects it only if
they differ. Missed packets, power cuts and manual button presses self-heal.

It also programs the plug's **own on-device timers**, so the schedule keeps
running even if the host goes down. As far as we know this is the first
third-party implementation of the ECO Plugs timer and clock commands; the
on/off protocol came from
[Danimal4326/homebridge-ecoplug](https://github.com/Danimal4326/homebridge-ecoplug)
and the rest was recovered from the vendor app. `doc/protocol.md` documents
all of it, verified against a real HOWT01A (firmware 1.7.1).

## Features

- Schedule made of on-windows in a local timezone; windows may cross midnight
  and can be limited to certain weekdays. DST just works.
- One command pushes the same windows into the plug's own 12-slot timer table,
  and the daemon keeps the plug's daylight-saving flag correct, which the
  vendor app leaves to a checkbox.
- Temporary overrides (`on --for 2h`, `off --until 06:00`) that expire on
  their own and survive daemon restarts.
- Winter mode: between two dates the daemon leaves the relay alone and clears
  the plug's timers, then puts everything back when the season ends.
- A small HTTP API on localhost that the CLI uses and that Home Assistant can
  poll, plus a Prometheus `/metrics` endpoint.
- Single static binary, one TOML config, `tracing` logs that read well under
  journald.

## Build

Requires a stable Rust toolchain.

```sh
cargo build --release
```

For a static Linux binary (e.g. for a Raspberry Pi), use
[`cross`](https://github.com/cross-rs/cross):

```sh
cross build --release --target aarch64-unknown-linux-musl
```

The GitHub Actions workflow builds `aarch64` and `x86_64` musl binaries on
every push and attaches them to a release on `v*` tags.

## Configure

Copy `deploy/ecopumpd.toml` to `/etc/ecopumpd.toml` and edit it:

```toml
device_id = "ECO-XXXXXXXX"      # from `ecopumpd discover`
host = "192.168.1.50"           # give the plug a DHCP reservation
timezone = "America/Toronto"
poll_interval_secs = 120
override_file = "/var/lib/ecopumpd/override"
listen = "127.0.0.1:8090"       # HTTP API; add `token = "..."` if exposed on the LAN

[[windows]]
start = "09:00"
end   = "17:00"
days  = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]   # optional
```

The pump is on whenever the current local time is inside any window. Start is
inclusive, end exclusive. A window whose end is earlier than its start crosses
midnight and belongs to the day it starts on.

Find your plug's id and address with:

```sh
ecopumpd discover
```

## Install as a service (systemd)

```sh
sudo useradd -r -s /usr/sbin/nologin ecopumpd
sudo install -m755 ecopumpd /usr/local/bin/ecopumpd
sudo install -m644 deploy/ecopumpd.toml /etc/ecopumpd.toml     # then edit
sudo install -m644 deploy/ecopumpd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ecopumpd
journalctl -fu ecopumpd
```

## Usage

```
ecopumpd run                       # the daemon
ecopumpd discover [--broadcast A]  # list plugs on the LAN
ecopumpd state                     # last state observed by the daemon
ecopumpd on | off                  # hold until the next scheduled transition
ecopumpd override on --for 2h
ecopumpd override off --until 06:00
ecopumpd override clear
ecopumpd status [--json]           # desired vs actual, override, next transition
ecopumpd schedule show             # timers stored on the plug itself
ecopumpd schedule sync             # replace them with the config windows
ecopumpd schedule clear            # delete them all
ecopumpd clock show                # the plug's clock and DST flag
ecopumpd clock dst on|off          # set the DST flag by hand
ecopumpd power                     # live W/V/A and month energy, on metering models
ecopumpd winter start 2026-10-15   # hands off from that date (today or earlier: now)
ecopumpd winter end 2027-04-20     # resume on that date; starts winter today if not set
ecopumpd winter cancel             # back to normal on the next poll
```

All commands accept `--config PATH` (default `/etc/ecopumpd.toml`). Set
`RUST_LOG=debug` to see every poll; `RUST_LOG=info` logs only transitions,
corrections and problems.

Only `run` and `discover` talk to the plug. Every other subcommand is a client
of the daemon's HTTP API and fails if the daemon is not running.

## HTTP API

Served on `listen` (default `127.0.0.1:8090`). If `token` is set, send
`Authorization: Bearer <token>`.

| Method | Path        | Body                                  | Effect |
|--------|-------------|---------------------------------------|--------|
| GET    | `/status`   |                                       | JSON: `schedule`, `override`, `desired`, `actual`, `last_poll`, `reachable`, `next` |
| POST   | `/set`      | `{"power":"on"}`                      | hold until the next scheduled transition |
| POST   | `/override` | `{"power":"off","until":"2026-09-23T04:00:00Z"}` | override until `until` (RFC 3339) |
| DELETE | `/override` |                                       | clear the override |
| GET    | `/metrics`  |                                       | Prometheus text format |
| POST   | `/winter`   | `{"start":"2026-10-15","end":"2027-04-20"}` | set winter mode; `end` optional |
| DELETE | `/winter`   |                                       | cancel winter mode |
| GET    | `/power`    |                                       | live watts, volts, amps and energy on metering models |
| GET    | `/clock`    |                                       | the plug's clock and DST flag |
| POST   | `/clock/dst` | `{"on":true}`                        | set the plug's DST flag |
| GET    | `/schedule` |                                       | the plug's on-device timer table |
| POST   | `/schedule/sync` |                                  | replace it with the config windows |
| DELETE | `/schedule` |                                       | remove every on-device timer |

### On-device timers

The plug can store up to 12 weekly timers of its own, which keep running if the
host running `ecopumpd` goes down. `ecopumpd schedule sync` programs the config
windows into the plug; the daemon keeps reconciling on top, so overrides still
work. The packet format comes from the vendor app (`com.kab.unlimit`); see
`doc/protocol.md`. Windows that cross midnight are sent as-is and it is not yet
confirmed how the plug treats them.

### Winter mode

For the closed season, or any time people are working on the pump, `winter`
sets a date range during which the daemon keeps its hands off. On the first
poll inside the range it clears the plug's on-device timers; from then on it
only observes, so the plug's button does what it says. Overrides are refused,
but `ecopumpd on` and `off` still switch the relay directly. On the end date
it syncs the timers back from the config and resumes reconciling. The dates
are local midnights, the state survives restarts, and the config windows are
never touched, so there is nothing to restore by hand in spring.

### Power metering

Some models in this family carry an energy meter. `ecopumpd power` reads it
using the same command and conversion the vendor app uses, and reports the raw
values plus the factory calibration divisors alongside watts, volts, amps and
the energy total for the current month. A plug without the hardware answers
with zeros, which prints as "no metering hardware". The HOWT01A is one of
those, so this is implemented but untested against a real meter; if you have
a metering model, a reading with a known load would confirm the scale factors.

The plug keeps *standard* local time and applies its timers an hour later when
its DST flag is set. The daemon checks that flag hourly against the configured
timezone and corrects it, so the spring and autumn changes need no attention.

### Home Assistant

With the daemon reachable from Home Assistant (set `listen` to a LAN address
and a `token`), the built-in REST platform gives you a switch you can expose
to Google Assistant or Alexa through HA:

```yaml
switch:
  - platform: rest
    name: Pool Pump
    resource: http://pump-host:8090/set
    state_resource: http://pump-host:8090/status
    headers:
      Authorization: Bearer change-me
    body_on: '{"power":"on"}'
    body_off: '{"power":"off"}'
    is_on_template: "{{ value_json.actual == 'on' }}"
    scan_interval: 60
```

## Notes and limitations

- The plug always sends replies to UDP port 9000 on the caller, so only one
  process per host can talk to it. The daemon owns that port; stop it before
  running `discover`.
- On hosts with several interfaces on the plug's subnet, set
  `bind = "<local ip>"` in the config so replies come back to the right one.
- macOS may return "No route to host" for LAN unicast UDP unless the terminal
  has Local Network permission. Linux works out of the box.
- The plug's own countdown timer may switch it off; the daemon re-asserts the
  desired state on the next poll.
- Consider blocking the plug's outbound access to the vendor cloud at your
  firewall once local control works.
