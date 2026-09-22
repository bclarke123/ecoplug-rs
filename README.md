# ecopumpd

A small Rust daemon that keeps an ECO Plugs / WiOn / Dewenwils Wi-Fi outlet
(such as the HOWT01A pool-pump timer) on a schedule using the outlet's local
UDP protocol. No vendor cloud, no app, no Home Assistant required.

It reconciles rather than fires and forgets: every poll it computes the desired
state from the schedule, reads the plug's actual state, and corrects it only if
they differ. Missed packets, power cuts and manual button presses self-heal.

The protocol was reverse-engineered by
[Danimal4326/homebridge-ecoplug](https://github.com/Danimal4326/homebridge-ecoplug);
see `doc/plan.md` for the packet layout and design notes.

## Features

- Schedule made of on-windows in a local timezone; windows may cross midnight
  and can be limited to certain weekdays. DST just works.
- Temporary overrides (`on --for 2h`, `off --until 06:00`) that expire on
  their own and survive daemon restarts.
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
