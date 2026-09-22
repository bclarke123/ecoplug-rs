# ecopumpd

Keeps a Dewenwils HOWT01A / ECO Plugs pool-pump timer on a schedule using its
local UDP protocol. No vendor cloud. See `doc/plan.md` for the design.

## Build

```sh
cargo build --release          # host
cross build --release --target aarch64-unknown-linux-musl   # for basil
```

GitHub Actions builds static musl binaries for aarch64 and x86_64 on every push
and attaches them to a release on `v*` tags.

## Install on basil

```sh
sudo useradd -r -s /usr/sbin/nologin ecopumpd
sudo install -m755 ecopumpd /usr/local/bin/ecopumpd
sudo install -m644 deploy/ecopumpd.toml /etc/ecopumpd.toml     # then edit
sudo install -m644 deploy/ecopumpd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ecopumpd
journalctl -fu ecopumpd
```

`override` subcommands write to `/var/lib/ecopumpd/override`, so run them as
the `ecopumpd` user (`sudo -u ecopumpd ecopumpd override on --for 2h`) or make
that directory group-writable.

## Usage

```
ecopumpd run                       # the daemon
ecopumpd discover [--broadcast A]  # list plugs on the LAN
ecopumpd state | on | off          # one-shot control
ecopumpd override on --for 2h
ecopumpd override off --until 06:00
ecopumpd override clear
ecopumpd status                    # desired vs actual, override, next transition
```

All commands take `--config PATH` (default `/etc/ecopumpd.toml`). Set
`RUST_LOG=debug` to see every poll.

The plug replies to UDP port 9000, so only one `ecopumpd` process can talk to
it at a time. While the daemon runs, `state`/`on`/`off` will fail to bind;
use `override` instead and the daemon applies it within a second or two.

## Notes

- On multi-homed hosts (e.g. a Mac with Ethernet and Wi-Fi on the same
  subnet) set `bind = "<local ip>"` in the config. macOS may still return
  "No route to host" for LAN unicast unless the terminal has Local Network
  permission; Linux works out of the box.
- The plug's own countdown timer may switch it off; the daemon re-asserts the
  desired state on the next poll.
