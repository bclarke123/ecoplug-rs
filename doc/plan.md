# ecopumpd — project plan

A small Rust daemon that keeps a Dewenwils HOWT01A (ECO Plugs firmware, ESP8266)
pool-pump timer on a schedule using its local UDP protocol. No vendor cloud.
Runs on `basil` (Raspberry Pi 5, aarch64) as a static musl binary built by
GitHub Actions.

Reference implementation of the protocol: `ecoplug.py` (already working against
the real device) and `lib/eco.js` in Danimal4326/homebridge-ecoplug.

---

## 1. Goals

- Reconcile, don't fire-and-forget: every N seconds compute the desired state
  from the schedule, read the plug's actual state, and correct it only if they
  differ. Missed packets, power outages and manual button presses self-heal.
- Temporary manual overrides ("run the pump for 2h now", "keep it off until
  tomorrow") that expire automatically.
- Proper logging (`tracing`), journald-friendly, with a `RUST_LOG` filter.
- Single static binary, no runtime deps, config in one TOML file.
- CLI subcommands for discovery and one-off control so the Python script can
  be retired.

Non-goals: Home Assistant / Google Home integration, MQTT, web UI (maybe later).

---

## 2. Protocol (ECO Plugs local UDP)

All multi-byte fields big-endian unless noted. Replies always arrive on
**UDP 9000** on the sender, so the daemon binds 0.0.0.0:9000 once and keeps it.

### Discovery
- Send 128 zero bytes with `u32 BE 0x00E0070B` at offset 23 and
  `u32 BE 0x11F79D00` at offset 27, to `<subnet broadcast>:25`
  (255.255.255.255 is unreliable; default to the interface's directed
  broadcast, allow override).
- Reply: 408 bytes. Offsets: `4..10` firmware version (ASCII, NUL-padded),
  `10..42` id (`ECO-XXXXXXXX`), `42..74` name, `74..106` short id,
  `120..144` SSID. Source IP of the reply is the plug's IP.

### Get state
- 128-byte packet to `<plug ip>:80`:
  - `0..4`   `u32 BE 0x17000500`
  - `4..8`   `u32 BE` random sequence
  - `8..10`  `u16 BE 0x0000`
  - `16..32` device id ASCII, NUL-padded
  - `116..120` `u32 LE` current epoch seconds
  - `124..128` `u32 BE 0xCDB8422A`
- Reply: 130 bytes; **byte 129** is the state (`0` off, non-zero on).

### Set state
- 130-byte packet, same layout, except:
  - `0..4`   `u32 BE 0x16000500`
  - `8..10`  `u16 BE 0x0200`
  - `128..130` `u16 BE 0x0101` for on, `0x0100` for off
- Reply: 128-byte ack. Always follow a set with a get to confirm.

Retries: 3 attempts, 1.5 s timeout each. Sequence numbers can be used to
discard stale replies.

---

## 3. Architecture

```
src/
  main.rs        clap CLI, subcommand dispatch
  config.rs      TOML config + validation
  proto.rs       packet build/parse, UdpSocket client (sync, std::net)
  schedule.rs    windows -> desired state at a given local time
  override.rs    override file read/write/expiry
  reconcile.rs   the main loop
```

Keep it synchronous. `std::net::UdpSocket` with `set_read_timeout` is plenty;
no tokio.

### Crates
`clap` (derive), `serde` + `toml`, `chrono` + `chrono-tz`, `tracing` +
`tracing-subscriber` (env-filter, optional `json` feature), `anyhow`,
`rand` (sequence numbers), `signal-hook` (SIGTERM/SIGHUP).

### Config (`/etc/ecopumpd.toml`)
```toml
device_id = "ECO-780D4D7D"
host = "10.1.1.169"
timezone = "America/Toronto"
poll_interval_secs = 120
override_file = "/var/lib/ecopumpd/override"

# Union of windows = "pump on". Windows may cross midnight.
[[windows]]
start = "09:00"
end   = "17:00"
days  = ["mon","tue","wed","thu","fri","sat","sun"]   # optional, default all
```

### Schedule semantics
- `desired(now_local) = any window contains now`.
- A window with `end < start` spans midnight and belongs to the day it starts.
- `days` matches the day the window *starts*.
- Evaluate in the configured timezone, not UTC, so DST just works.

### Overrides
Single small file, e.g. `on 1727049600` (state + unix expiry). Daemon reads
it every loop; if present and unexpired, it wins over the schedule; if expired,
delete it and log. Written by `ecopumpd override on --for 2h`,
`ecopumpd override off --until 06:00`, cleared by `ecopumpd override clear`.

### Reconcile loop
```
loop:
  desired = override.unwrap_or(schedule.desired(now))
  actual  = proto.get_state()            # with retries
  match actual:
    Err  -> warn (rate-limited), backoff up to 5 min, continue
    Ok(s) if s != desired ->
        proto.set_state(desired); confirm with get; info!("corrected ...")
    Ok(_) -> debug!("in sync")
  sleep(poll_interval), wake early on SIGHUP (config reload)
```
Log every transition and every correction at `info`, in-sync polls at
`debug`, unreachable plug at `warn` but not more than once per 5 minutes.
Emit a `warn` if the plug has been unreachable for > 30 min.

### CLI
```
ecopumpd run       [--config PATH]
ecopumpd discover  [--broadcast 10.1.1.255]
ecopumpd state
ecopumpd on | off
ecopumpd override on --for 2h | off --until 06:00 | clear
ecopumpd status     # desired vs actual, active override, next transition
```

---

## 4. Deployment

- `deploy/ecopumpd.service`: `Type=simple`, `Restart=always`,
  `Environment=RUST_LOG=info`, `DynamicUser=yes` won't work because of the
  fixed port 9000 bind + state dir; use a dedicated `ecopumpd` user with
  `StateDirectory=ecopumpd`.
- Give the plug a DHCP reservation in UniFi (10.1.1.169).
- Keep the firewall block on `topcms.dyndns.tv`.
- Remove the old crontab entries once the daemon has run clean for a day.

---

## 5. CI (GitHub Actions)

`.github/workflows/build.yml`:
- Trigger on push, PR, and tags `v*`.
- Matrix: `aarch64-unknown-linux-musl` (basil), `x86_64-unknown-linux-musl`.
- Use `cross` (`taiki-e/install-action@cross`) or `cargo-zigbuild`; both
  handle the aarch64 musl linker without fuss.
- `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on the host
  target first.
- Upload each binary as an artifact; on a tag, attach both to a GitHub Release
  (`softprops/action-gh-release`).
- Release profile: `lto = true`, `codegen-units = 1`, `strip = true`,
  `panic = "abort"`.

Install on basil: download the aarch64 asset, `install -m755` to
`/usr/local/bin`, copy the unit, `systemctl enable --now ecopumpd`.

---

## 6. Tests

- `proto`: build a get/set packet and assert byte offsets against the values
  in §2; parse a canned 408/130/128 reply.
- `schedule`: same-day window, midnight-crossing window, day filter, DST
  transition day in America/Toronto, boundary minutes (start inclusive, end
  exclusive).
- `override`: expiry, malformed file is ignored and logged.
- Integration (manual): `ecopumpd discover` finds the plug; `state`/`on`/
  `off` round-trip; `run` with a 1-minute window flips the relay and logs
  the correction.

---

## 7. Milestones

1. Cargo project, CLI skeleton, `proto` with `discover` / `state` / `on` /
   `off`. Parity with `ecoplug.py`. Verify against the real plug.
2. Config + schedule + reconcile loop + logging. Run under systemd.
3. Overrides + `status`.
4. CI cross-build + release workflow. Tag `v0.1.0`, install on basil, remove
   cron.

Later, if wanted: `--metrics` endpoint or MQTT publish so HA can see it.
