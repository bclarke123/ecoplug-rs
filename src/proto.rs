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
use tracing::{debug, trace};

/// Port the plug delivers every reply to.
pub const REPLY_PORT: u16 = 9000;
/// Port the plug accepts discovery broadcasts on.
pub const DISCOVERY_PORT: u16 = 25;
/// Port the plug accepts get/set commands on.
pub const COMMAND_PORT: u16 = 80;

/// Command word for "read state".
const CMD_GET: u32 = 0x1700_0500;
/// Command word for "write state".
const CMD_SET: u32 = 0x1600_0500;
/// Sub-command word for "read state" (bytes 8..10).
const SUB_GET: u16 = 0x0000;
/// Sub-command word for "write state" (bytes 8..10).
const SUB_SET: u16 = 0x0200;
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

    /// Sends `msg` to the plug and waits for a reply of `want_len` bytes that
    /// echoes `device_id`, retrying up to [`ATTEMPTS`] times.
    fn exchange(&self, target: SocketAddr, msg: &[u8], device_id: &str, want_len: usize) -> Result<Vec<u8>> {
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
                if from.ip() != target.ip() || n != want_len || id.as_deref() != Some(device_id) {
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
        let reply = self.exchange(SocketAddr::new(host, COMMAND_PORT), &msg, device_id, SET_LEN)?;
        parse_state(&reply).context("malformed state reply")
    }

    /// Sets the relay state and confirms it with a follow-up read.
    ///
    /// # Errors
    /// Fails if the plug does not acknowledge, or if the confirming read
    /// reports a different state than requested.
    pub fn set_state(&self, device_id: &str, host: IpAddr, power: Power) -> Result<()> {
        let msg = build_set(device_id, rand::random(), epoch_now(), power);
        self.exchange(SocketAddr::new(host, COMMAND_PORT), &msg, device_id, GET_LEN)?;
        let actual = self.get_state(device_id, host)?;
        if actual != power {
            bail!("plug reports {actual} after being set {power}");
        }
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
    fn reply_id_is_read_from_offset_16() {
        let mut reply = vec![0u8; 130];
        reply[16..28].copy_from_slice(b"ECO-780D4D7D");
        assert_eq!(reply_device_id(&reply).as_deref(), Some("ECO-780D4D7D"));
    }
}
