//! ECO Plugs local UDP protocol: packet building, parsing and a blocking client.
//!
//! The plug listens on UDP port 80 for commands and port 25 for discovery
//! broadcasts, and always sends its replies to UDP port 9000 on the sender.
//! [`Client`] therefore binds port 9000 once and keeps it for its lifetime.
//!
//! All multi-byte fields are big-endian unless noted otherwise.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

/// Port the plug delivers every reply to.
pub const REPLY_PORT: u16 = 9000;
/// Port the plug accepts discovery broadcasts on.
pub const DISCOVERY_PORT: u16 = 25;
/// Port the plug accepts get/set commands on.
pub const COMMAND_PORT: u16 = 80;

/// Command word for "read state" (vendor `CMD_BASCI_GET_SWITCH_STATUS`, 327703 LE).
const CMD_GET: u32 = 0x1700_0500;
/// Command word for "write state" (vendor `CMD_BASCI_MODIFY_SWITCH`, 327702 LE).
const CMD_SET: u32 = 0x1600_0500;
/// Vendor command ids, little-endian at bytes 0..4 (from the ECO Plugs APK).
const VCMD_SCHEDULE_ADD: u32 = 327_936;
// 327_937 is SCHEDULE_EDIT (full table with one entry changed); not needed yet.
const VCMD_SCHEDULE_DELETE: u32 = 327_938;
const VCMD_SCHEDULE_GETALL: u32 = 327_939;
/// Reads the 364-byte settings block (`BoxSetting`).
const VCMD_GET_SETTING: u32 = 327_685;
/// Sets the DST flag via a 12-byte `TimeZone` with magic values (see [`Client::set_dst`]).
const VCMD_MODIFY_TIMEZONE: u32 = 327_701;
/// Offset of the `TimeZone` block inside `BoxSetting`.
const SETTING_TZ_OFFSET: usize = 104;
/// Bit set in the year field when daylight saving is on.
const DST_YEAR_FLAG: u16 = 0x1000;
/// Bytes 8..10 hold the little-endian length of the payload after the header.
const SUB_GET: u16 = 0x0000;
const SUB_SET: u16 = 0x0200;
/// Header bytes 4..6: zero in requests, result code in replies (0 ok, 3 no permission).
const MARK_OFFSET: usize = 4;
const MARK_NO_PERMISSION: u16 = 3;
/// Header length shared by every command and reply.
pub const HEADER_LEN: usize = 128;
/// Size of the on-device schedule table payload.
pub const SCHEDULE_LEN: usize = 388;
/// Entries the device can store.
pub const SCHEDULE_SLOTS: usize = 12;
/// Size of one schedule entry.
const ENTRY_LEN: usize = 32;
/// Trailer word at bytes 124..128 of every command; meaning unknown but required.
const TRAILER: u32 = 0xCDB8_422A;
/// Discovery magic written at offset 23.
const DISCOVERY_MAGIC_A: u32 = 0x00E0_070B;
/// Discovery magic written at offset 27.
const DISCOVERY_MAGIC_B: u32 = 0x11F7_9D00;
/// Payload for "relay on" at bytes 128..130 of a set command.
const STATE_ON: u16 = 0x0101;
/// Payload for "relay off" at bytes 128..130 of a set command.
const STATE_OFF: u16 = 0x0100;

/// Length of a get command and of a set acknowledgement.
pub const GET_LEN: usize = 128;
/// Length of a set command and of a get reply.
pub const SET_LEN: usize = 130;
/// Length of a discovery reply.
pub const DISCOVERY_REPLY_LEN: usize = 408;
/// Offset of the relay state byte in a get reply.
const STATE_OFFSET: usize = 129;

/// Number of send attempts before giving up.
const ATTEMPTS: u32 = 3;
/// How long to wait for a reply per attempt.
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long discovery listens after each broadcast.
const DISCOVERY_WINDOW: Duration = Duration::from_secs(1);

/// Whether the relay is on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Off,
    On,
}

impl Power {
    /// Converts a boolean where `true` means on.
    pub fn from_bool(on: bool) -> Self {
        if on { Self::On } else { Self::Off }
    }

    /// Whether the relay is on.
    pub fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

impl fmt::Display for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Off => "off",
        })
    }
}

/// A plug found by discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub ip: IpAddr,
    pub firmware: String,
    pub id: String,
    pub name: String,
    pub short_id: String,
    pub ssid: String,
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<16} {:<16} fw={:<8} name={:?} ssid={}",
            self.id, self.ip, self.firmware, self.name, self.ssid
        )
    }
}

/// One entry of the plug's on-device timer table (32 bytes on the wire).
///
/// Only programmable weekly timers are modelled: `task_type` 0, `mode` 2,
/// switch on at `start_secs` and off at `end_secs` on the days in `days`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub id: u8,
    pub task_type: u8,
    pub mode: u8,
    /// Bitmask: Sun=0x40, Mon=0x20, Tue=0x10, Wed=0x08, Thu=0x04, Fri=0x02, Sat=0x01.
    pub days: u8,
    pub start_secs: u32,
    pub end_secs: u32,
    pub start_status: u8,
    pub end_status: u8,
    /// (year, month, day) stamped by the app; informational only.
    pub start_date: (u16, u8, u8),
    pub end_date: (u16, u8, u8),
}

impl ScheduleEntry {
    /// Task type used by the app for programmable and countdown timers.
    pub const TYPE_TIMER: u8 = 0;
    /// Mode used by the app for a weekly programmable timer (0 is countdown).
    pub const MODE_WEEKLY: u8 = 2;

    /// A weekly on/off window; `days` uses the wire bitmask.
    pub fn weekly(id: u8, days: u8, start_secs: u32, end_secs: u32, today: (u16, u8, u8)) -> Self {
        Self {
            id,
            task_type: Self::TYPE_TIMER,
            mode: Self::MODE_WEEKLY,
            days,
            start_secs,
            end_secs,
            start_status: 1,
            end_status: 0,
            start_date: today,
            end_date: today,
        }
    }

    fn to_bytes(self) -> [u8; ENTRY_LEN] {
        let mut b = [0u8; ENTRY_LEN];
        b[0] = self.id;
        b[1] = self.task_type;
        b[2] = self.mode;
        b[3] = self.days;
        b[12..14].copy_from_slice(&self.start_date.0.to_le_bytes());
        b[14] = self.start_date.1;
        b[15] = self.start_date.2;
        b[16..20].copy_from_slice(&self.start_secs.to_le_bytes());
        b[20] = self.start_status;
        b[22..24].copy_from_slice(&self.end_date.0.to_le_bytes());
        b[24] = self.end_date.1;
        b[25] = self.end_date.2;
        b[26] = self.end_status;
        b[28..32].copy_from_slice(&self.end_secs.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Self {
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Self {
            id: b[0],
            task_type: b[1],
            mode: b[2],
            days: b[3],
            start_date: (u16_at(12), b[14], b[15]),
            start_secs: u32_at(16),
            start_status: b[20],
            end_date: (u16_at(22), b[24], b[25]),
            end_status: b[26],
            end_secs: u32_at(28),
        }
    }
}

impl fmt::Display for ScheduleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hm = |s: u32| format!("{:02}:{:02}", s / 3600, (s / 60) % 60);
        let names = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let days: Vec<&str> = (0..7)
            .filter(|i| self.days & (0x40 >> i) != 0)
            .map(|i| names[i])
            .collect();
        write!(
            f,
            "#{} type={} mode={} {}-{} [{}] on={} off={}",
            self.id,
            self.task_type,
            self.mode,
            hm(self.start_secs),
            hm(self.end_secs),
            days.join(","),
            self.start_status,
            self.end_status
        )
    }
}

/// The plug's notion of local time, from its settings block.
///
/// The app provisions the plug with *standard* local time plus a DST flag,
/// so `hour:minute` here is standard time even in summer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlugClock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// Seconds since midnight, standard time.
    pub secs: u32,
    pub dst: bool,
    /// Offset in seconds relative to the vendor's home zone (UTC+8).
    pub offset_secs: i32,
}

impl PlugClock {
    /// Parses the 12-byte `TimeZone` block.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 12 {
            return None;
        }
        let year_raw = u16::from_le_bytes([b[0], b[1]]);
        Some(Self {
            year: year_raw & !DST_YEAR_FLAG,
            month: b[2],
            day: b[3],
            secs: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            dst: year_raw & DST_YEAR_FLAG != 0,
            offset_secs: i32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        })
    }

    /// Decodes the packed clock the plug puts at header bytes 112..116 of
    /// every reply: 12-bit year, 4-bit month, 5-bit day, 5-bit hour, 6-bit minute.
    pub fn from_header_date(v: u32) -> Self {
        Self {
            year: (v >> 20) as u16,
            month: ((v >> 16) & 0xF) as u8,
            day: ((v >> 11) & 0x1F) as u8,
            secs: ((v >> 6) & 0x1F) * 3600 + (v & 0x3F) * 60,
            dst: false,
            offset_secs: 0,
        }
    }
}

impl fmt::Display for PlugClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local = self.secs + if self.dst { 3600 } else { 0 };
        let hms = |s: u32| format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
        write!(
            f,
            "{:04}-{:02}-{:02} {} local (standard {}, dst={}, offset={}s)",
            self.year,
            self.month,
            self.day,
            hms(local),
            hms(self.secs),
            self.dst,
            self.offset_secs
        )
    }
}

/// The plug's whole timer table (388 bytes on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScheduleTable {
    pub entries: Vec<ScheduleEntry>,
}

impl ScheduleTable {
    fn to_bytes(&self) -> [u8; SCHEDULE_LEN] {
        let mut b = [0u8; SCHEDULE_LEN];
        let n = self.entries.len().min(SCHEDULE_SLOTS);
        b[0..2].copy_from_slice(&(n as u16).to_le_bytes());
        for (i, e) in self.entries.iter().take(n).enumerate() {
            let at = 4 + i * ENTRY_LEN;
            b[at..at + ENTRY_LEN].copy_from_slice(&e.to_bytes());
        }
        b
    }

    /// Parses the payload of a `SCHEDULE_GETALL` reply.
    ///
    /// The plug claims 388 bytes but its reply is capped at 512 bytes total,
    /// so only 384 arrive; like the app, missing bytes are treated as zero.
    pub fn from_bytes(raw: &[u8]) -> Option<Self> {
        if raw.len() < 4 {
            return None;
        }
        let mut b = [0u8; SCHEDULE_LEN];
        let n = raw.len().min(SCHEDULE_LEN);
        b[..n].copy_from_slice(&raw[..n]);
        let count = usize::from(u16::from_le_bytes([b[0], b[1]])).min(SCHEDULE_SLOTS);
        let entries = (0..count)
            .map(|i| ScheduleEntry::from_bytes(&b[4 + i * ENTRY_LEN..4 + (i + 1) * ENTRY_LEN]))
            .collect();
        Some(Self { entries })
    }

    /// The lowest id not used by any entry, as the app allocates them.
    pub fn free_id(&self) -> Option<u8> {
        (0..SCHEDULE_SLOTS as u8).find(|id| !self.entries.iter().any(|e| e.id == *id))
    }
}

/// Builds a vendor command with a little-endian id and an optional payload.
fn build_vendor(cmd: u32, device_id: &str, seq: u16, epoch: u32, payload: &[u8]) -> Vec<u8> {
    let mut msg = vec![0u8; HEADER_LEN + payload.len()];
    // Same layout as `fill_header`, but bytes 4..6 (result mark) stay zero and
    // the random sequence sits in 6..8 as the app does.
    msg[0..4].copy_from_slice(&cmd.to_le_bytes());
    msg[6..8].copy_from_slice(&seq.to_le_bytes());
    msg[8..10].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    let id = device_id.as_bytes();
    let n = id.len().min(16);
    msg[16..16 + n].copy_from_slice(&id[..n]);
    msg[116..120].copy_from_slice(&epoch.to_le_bytes());
    msg[124..128].copy_from_slice(&TRAILER.to_be_bytes());
    msg[HEADER_LEN..].copy_from_slice(payload);
    msg
}

/// Builds the 128-byte discovery broadcast.
pub fn build_discover() -> [u8; GET_LEN] {
    let mut msg = [0u8; GET_LEN];
    msg[23..27].copy_from_slice(&DISCOVERY_MAGIC_A.to_be_bytes());
    msg[27..31].copy_from_slice(&DISCOVERY_MAGIC_B.to_be_bytes());
    msg
}

/// Fills the fields shared by get and set commands into `msg`.
fn fill_header(msg: &mut [u8], cmd: u32, sub: u16, device_id: &str, seq: u32, epoch: u32) {
    msg[0..4].copy_from_slice(&cmd.to_be_bytes());
    msg[4..8].copy_from_slice(&seq.to_be_bytes());
    msg[8..10].copy_from_slice(&sub.to_be_bytes());
    let id = device_id.as_bytes();
    let n = id.len().min(16);
    msg[16..16 + n].copy_from_slice(&id[..n]);
    msg[116..120].copy_from_slice(&epoch.to_le_bytes());
    msg[124..128].copy_from_slice(&TRAILER.to_be_bytes());
}

/// Builds a 128-byte "read state" command.
pub fn build_get(device_id: &str, seq: u32, epoch: u32) -> [u8; GET_LEN] {
    let mut msg = [0u8; GET_LEN];
    fill_header(&mut msg, CMD_GET, SUB_GET, device_id, seq, epoch);
    msg
}

/// Builds a 130-byte "write state" command.
pub fn build_set(device_id: &str, seq: u32, epoch: u32, power: Power) -> [u8; SET_LEN] {
    let mut msg = [0u8; SET_LEN];
    fill_header(&mut msg, CMD_SET, SUB_SET, device_id, seq, epoch);
    let state = if power.is_on() { STATE_ON } else { STATE_OFF };
    msg[128..130].copy_from_slice(&state.to_be_bytes());
    msg
}

/// Hex-encodes bytes for debug logs.
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Reads a NUL-padded ASCII field.
fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Parses a 408-byte discovery reply received from `ip`.
pub fn parse_discovery(reply: &[u8], ip: IpAddr) -> Option<Device> {
    if reply.len() != DISCOVERY_REPLY_LEN {
        return None;
    }
    Some(Device {
        ip,
        firmware: cstr(&reply[4..10]),
        id: cstr(&reply[10..42]),
        name: cstr(&reply[42..74]),
        short_id: cstr(&reply[74..106]),
        ssid: cstr(&reply[120..144]),
    })
}

/// Parses a 130-byte state reply.
pub fn parse_state(reply: &[u8]) -> Option<Power> {
    if reply.len() != SET_LEN {
        return None;
    }
    Some(Power::from_bool(reply[STATE_OFFSET] != 0))
}

/// Returns the device id echoed at bytes 16..48 of a command reply, if any.
fn reply_device_id(reply: &[u8]) -> Option<String> {
    reply.get(16..48).map(cstr)
}

/// Current Unix time in seconds, saturated to `u32` as the protocol requires.
fn epoch_now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Blocking UDP client bound to the plug's reply port.
#[derive(Debug)]
pub struct Client {
    sock: UdpSocket,
}

impl Client {
    /// Binds UDP port 9000 on `local` (normally `0.0.0.0`).
    ///
    /// # Errors
    /// Fails if the port is already in use, typically because another
    /// `ecopumpd` process (the daemon) is running on this host.
    pub fn bind(local: IpAddr) -> Result<Self> {
        let sock = UdpSocket::bind((local, REPLY_PORT))
            .with_context(|| format!("bind udp port {REPLY_PORT} (is the daemon running?)"))?;
        sock.set_broadcast(true).context("enable broadcast")?;
        Ok(Self { sock })
    }

    /// Discards any replies still queued on the socket so a stale one is never
    /// mistaken for the answer to the next command.
    fn drain(&self) -> Result<()> {
        self.sock.set_read_timeout(Some(Duration::from_millis(1)))?;
        let mut buf = [0u8; 512];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => trace!(len = n, %from, "discarded stale packet"),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Sends `msg` to the plug and waits for a reply that echoes `device_id`
    /// and satisfies `accept`, retrying up to [`ATTEMPTS`] times.
    fn exchange(
        &self,
        target: SocketAddr,
        msg: &[u8],
        device_id: &str,
        accept: impl Fn(&[u8]) -> bool,
    ) -> Result<Vec<u8>> {
        self.drain()?;
        self.sock.set_read_timeout(Some(ATTEMPT_TIMEOUT))?;
        let mut buf = [0u8; 512];
        for attempt in 1..=ATTEMPTS {
            self.sock.send_to(msg, target).context("send command")?;
            let deadline = Instant::now() + ATTEMPT_TIMEOUT;
            while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
                self.sock.set_read_timeout(Some(left))?;
                let (n, from) = match self.sock.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                    Err(e) => return Err(e).context("receive reply"),
                };
                let reply = &buf[..n];
                let id = reply_device_id(reply);
                if from.ip() != target.ip() || !accept(reply) || id.as_deref() != Some(device_id) {
                    debug!(len = n, %from, ?id, "ignoring unexpected packet");
                    continue;
                }
                return Ok(reply.to_vec());
            }
            debug!(attempt, %target, "no reply, retrying");
        }
        bail!("no reply from {target} after {ATTEMPTS} attempts")
    }

    /// Reads the relay state of the plug at `host`.
    pub fn get_state(&self, device_id: &str, host: IpAddr) -> Result<Power> {
        let msg = build_get(device_id, rand::random(), epoch_now());
        let reply = self.exchange(SocketAddr::new(host, COMMAND_PORT), &msg, device_id, |r| {
            r.len() == SET_LEN
        })?;
        parse_state(&reply).context("malformed state reply")
    }

    /// Sets the relay state and confirms it with a follow-up read.
    ///
    /// # Errors
    /// Fails if the plug does not acknowledge, or if the confirming read
    /// reports a different state than requested.
    pub fn set_state(&self, device_id: &str, host: IpAddr, power: Power) -> Result<()> {
        let msg = build_set(device_id, rand::random(), epoch_now(), power);
        self.exchange(SocketAddr::new(host, COMMAND_PORT), &msg, device_id, |r| {
            r.len() == GET_LEN
        })?;
        let actual = self.get_state(device_id, host)?;
        if actual != power {
            bail!("plug reports {actual} after being set {power}");
        }
        Ok(())
    }

    /// Sends a vendor command and checks the reply's result mark.
    fn vendor(&self, device_id: &str, host: IpAddr, cmd: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let seq: u16 = rand::random_range(1..32_727);
        let msg = build_vendor(cmd, device_id, seq, epoch_now(), payload);
        let reply = self.exchange(SocketAddr::new(host, COMMAND_PORT), &msg, device_id, |r| {
            r.len() >= HEADER_LEN && u32::from_le_bytes([r[0], r[1], r[2], r[3]]) == cmd
        })?;
        let mark = u16::from_le_bytes([reply[MARK_OFFSET], reply[MARK_OFFSET + 1]]);
        let ext_len = u16::from_le_bytes([reply[8], reply[9]]);
        let clock = PlugClock::from_header_date(u32::from_le_bytes([reply[112], reply[113], reply[114], reply[115]]));
        debug!(
            cmd,
            mark,
            ext_len,
            len = reply.len(),
            plug_clock = %clock,
            payload = hex(&reply[HEADER_LEN..]),
            "vendor reply"
        );
        match mark {
            0 => Ok(reply),
            MARK_NO_PERMISSION => bail!("plug refused command {cmd}: no permission (password set?)"),
            m => bail!("plug rejected command {cmd} with result {m}"),
        }
    }

    /// Reads the plug's clock and DST flag from its settings block.
    pub fn get_clock(&self, device_id: &str, host: IpAddr) -> Result<PlugClock> {
        let reply = self.vendor(device_id, host, VCMD_GET_SETTING, &[])?;
        let tz = reply.get(HEADER_LEN + SETTING_TZ_OFFSET..HEADER_LEN + SETTING_TZ_OFFSET + 12);
        tz.and_then(PlugClock::from_bytes).context("settings reply too short")
    }

    /// Sets the plug's daylight-saving flag.
    ///
    /// Mirrors the app's DST switch: a `TimeZone` block with year 6111 (2015
    /// with the DST bit) for on or 2015 for off, and fixed filler values.
    pub fn set_dst(&self, device_id: &str, host: IpAddr, on: bool) -> Result<()> {
        let year: u16 = if on { 2015 | DST_YEAR_FLAG } else { 2015 };
        let mut b = [0u8; 12];
        b[0..2].copy_from_slice(&year.to_le_bytes());
        b[2] = 4;
        b[3] = 18;
        b[4..8].copy_from_slice(&57_600u32.to_le_bytes());
        self.vendor(device_id, host, VCMD_MODIFY_TIMEZONE, &b)?;
        Ok(())
    }

    /// Reads the plug's on-device timer table.
    pub fn get_schedule(&self, device_id: &str, host: IpAddr) -> Result<ScheduleTable> {
        let reply = self.vendor(device_id, host, VCMD_SCHEDULE_GETALL, &[])?;
        ScheduleTable::from_bytes(&reply[HEADER_LEN..]).context("malformed schedule reply")
    }

    /// Adds `entry` the way the app does: sends the whole table with it appended.
    pub fn add_schedule_entry(
        &self,
        device_id: &str,
        host: IpAddr,
        table: &mut ScheduleTable,
        entry: ScheduleEntry,
    ) -> Result<()> {
        if table.entries.len() >= SCHEDULE_SLOTS {
            bail!("plug already holds {SCHEDULE_SLOTS} timers");
        }
        table.entries.push(entry);
        self.vendor(device_id, host, VCMD_SCHEDULE_ADD, &table.to_bytes())?;
        Ok(())
    }

    /// Deletes the entry with `id`, sending the table without it.
    pub fn delete_schedule_entry(
        &self,
        device_id: &str,
        host: IpAddr,
        table: &mut ScheduleTable,
        id: u8,
    ) -> Result<()> {
        let before = table.entries.len();
        table.entries.retain(|e| e.id != id);
        if table.entries.len() == before {
            bail!("no timer with id {id}");
        }
        self.vendor(device_id, host, VCMD_SCHEDULE_DELETE, &table.to_bytes())?;
        Ok(())
    }

    /// Broadcasts discovery to each of `broadcasts` and collects every plug that answers.
    pub fn discover(&self, broadcasts: &[IpAddr]) -> Result<Vec<Device>> {
        let msg = build_discover();
        let mut found: Vec<Device> = Vec::new();
        let mut buf = [0u8; 1024];
        for _ in 0..ATTEMPTS {
            for b in broadcasts {
                self.sock
                    .send_to(&msg, SocketAddr::new(*b, DISCOVERY_PORT))
                    .context("send discovery broadcast")?;
            }
            let deadline = Instant::now() + DISCOVERY_WINDOW;
            while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
                self.sock.set_read_timeout(Some(left))?;
                let (n, from) = match self.sock.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                    Err(e) => return Err(e).context("receive discovery reply"),
                };
                if let Some(dev) = parse_discovery(&buf[..n], from.ip())
                    && !found.iter().any(|d| d.id == dev.id)
                {
                    found.push(dev);
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes(b.try_into().unwrap())
    }

    #[test]
    fn get_packet_layout() {
        let msg = build_get("ECO-780D4D7D", 0xBEEF, 1_727_049_600);
        assert_eq!(msg.len(), 128);
        assert_eq!(be32(&msg[0..4]), 0x17000500);
        assert_eq!(be32(&msg[4..8]), 0xBEEF);
        assert_eq!(&msg[8..10], &[0, 0]);
        assert_eq!(&msg[16..28], b"ECO-780D4D7D");
        assert_eq!(&msg[28..32], &[0; 4]);
        assert_eq!(u32::from_le_bytes(msg[116..120].try_into().unwrap()), 1_727_049_600);
        assert_eq!(be32(&msg[124..128]), 0xCDB8422A);
    }

    #[test]
    fn set_packet_layout() {
        let on = build_set("ECO-780D4D7D", 1, 0, Power::On);
        assert_eq!(on.len(), 130);
        assert_eq!(be32(&on[0..4]), 0x16000500);
        assert_eq!(&on[8..10], &[0x02, 0x00]);
        assert_eq!(&on[128..130], &[0x01, 0x01]);
        let off = build_set("ECO-780D4D7D", 1, 0, Power::Off);
        assert_eq!(&off[128..130], &[0x01, 0x00]);
    }

    #[test]
    fn discovery_packet_layout() {
        let msg = build_discover();
        assert_eq!(be32(&msg[23..27]), 0x00E0070B);
        assert_eq!(be32(&msg[27..31]), 0x11F79D00);
        assert!(msg[..23].iter().all(|&b| b == 0));
    }

    #[test]
    fn parses_state_reply() {
        let mut reply = vec![0u8; 130];
        assert_eq!(parse_state(&reply), Some(Power::Off));
        reply[129] = 1;
        assert_eq!(parse_state(&reply), Some(Power::On));
        assert_eq!(parse_state(&reply[..128]), None);
    }

    #[test]
    fn parses_discovery_reply() {
        let mut reply = vec![0u8; 408];
        reply[4..9].copy_from_slice(b"1.0.2");
        reply[10..22].copy_from_slice(b"ECO-780D4D7D");
        reply[42..46].copy_from_slice(b"Pool");
        reply[74..82].copy_from_slice(b"780D4D7D");
        reply[120..126].copy_from_slice(b"MyWifi");
        let ip: IpAddr = "10.1.1.169".parse().unwrap();
        let dev = parse_discovery(&reply, ip).unwrap();
        assert_eq!(dev.firmware, "1.0.2");
        assert_eq!(dev.id, "ECO-780D4D7D");
        assert_eq!(dev.name, "Pool");
        assert_eq!(dev.short_id, "780D4D7D");
        assert_eq!(dev.ssid, "MyWifi");
        assert_eq!(dev.ip, ip);
        assert!(parse_discovery(&reply[..400], ip).is_none());
    }

    #[test]
    fn schedule_entry_round_trip_and_layout() {
        // Sun|Mon|Fri = 0x40|0x20|0x02
        let e = ScheduleEntry::weekly(3, 0x62, 9 * 3600, 17 * 3600 + 30 * 60, (2026, 9, 23));
        let b = e.to_bytes();
        assert_eq!(&b[0..4], &[3, 0, 2, 0x62]);
        assert_eq!(&b[12..16], &[0xEA, 0x07, 9, 23]);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 32_400);
        assert_eq!(b[20], 1);
        assert_eq!(b[26], 0);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 63_000);
        assert_eq!(ScheduleEntry::from_bytes(&b), e);
        assert_eq!(e.to_string(), "#3 type=0 mode=2 09:00-17:30 [sun,mon,fri] on=1 off=0");
    }

    #[test]
    fn schedule_table_round_trip_and_ids() {
        let mut t = ScheduleTable::default();
        t.entries.push(ScheduleEntry::weekly(0, 0x7F, 0, 60, (2026, 1, 1)));
        t.entries.push(ScheduleEntry::weekly(2, 0x7F, 0, 60, (2026, 1, 1)));
        let b = t.to_bytes();
        assert_eq!(b.len(), 388);
        assert_eq!(&b[0..4], &[2, 0, 0, 0]);
        assert_eq!(b[4 + 32], 2, "second entry id at 4 + 32");
        assert_eq!(ScheduleTable::from_bytes(&b).unwrap(), t);
        assert_eq!(t.free_id(), Some(1));
        assert_eq!(
            ScheduleTable::from_bytes(&b[..384]).unwrap(),
            t,
            "short tables are zero-padded"
        );
        assert!(ScheduleTable::from_bytes(&b[..3]).is_none());
    }

    #[test]
    fn vendor_packet_layout() {
        let payload = [0xAAu8; 388];
        let msg = build_vendor(VCMD_SCHEDULE_ADD, "ECO-780D4D7D", 0x1234, 1_727_049_600, &payload);
        assert_eq!(msg.len(), 516);
        assert_eq!(&msg[0..4], &[0x00, 0x01, 0x05, 0x00]);
        assert_eq!(&msg[4..6], &[0, 0], "result mark is zero in requests");
        assert_eq!(&msg[6..8], &[0x34, 0x12]);
        assert_eq!(&msg[8..10], &[0x84, 0x01], "payload length 388 LE");
        assert_eq!(&msg[16..28], b"ECO-780D4D7D");
        assert_eq!(&msg[128..], &payload[..]);
        // The GETALL request is a bare header.
        assert_eq!(build_vendor(VCMD_SCHEDULE_GETALL, "x", 1, 0, &[]).len(), 128);
        // Sanity: our existing get command is the same scheme with id 327703.
        assert_eq!(327_703u32.to_le_bytes(), CMD_GET.to_be_bytes());
    }

    #[test]
    fn decodes_plug_clock() {
        // Captured header date 0x7ea9bb6d while local time was 14:45 EDT.
        let c = PlugClock::from_header_date(0x7ea9_bb6d);
        assert_eq!((c.year, c.month, c.day, c.secs), (2026, 9, 23, 13 * 3600 + 45 * 60));
        let mut b = [0u8; 12];
        b[0..2].copy_from_slice(&(2026u16 | 0x1000).to_le_bytes());
        b[2] = 9;
        b[3] = 23;
        b[4..8].copy_from_slice(&49_500u32.to_le_bytes());
        b[8..12].copy_from_slice(&(-46_800i32).to_le_bytes());
        let c = PlugClock::from_bytes(&b).unwrap();
        assert!(c.dst);
        assert_eq!(c.year, 2026);
        assert_eq!(c.offset_secs, -46_800);
        assert_eq!(
            c.to_string(),
            "2026-09-23 14:45:00 local (standard 13:45:00, dst=true, offset=-46800s)"
        );
    }

    #[test]
    fn reply_id_is_read_from_offset_16() {
        let mut reply = vec![0u8; 130];
        reply[16..28].copy_from_slice(b"ECO-780D4D7D");
        assert_eq!(reply_device_id(&reply).as_deref(), Some("ECO-780D4D7D"));
    }
}
