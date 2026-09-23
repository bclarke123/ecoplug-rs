# ECO Plugs local UDP protocol

Reconstructed from `lib/eco.js` in homebridge-ecoplug and from the vendor app
`com.kab.unlimit` (classes `com.further.net.FtSmartDefine`, `FtNetManager`).
Little-endian unless stated. The plug listens on UDP 80 (commands) and 25
(discovery) and always replies to UDP 9000 on the sender.

## Header (128 bytes, `SmartCtrlHead`)

| Offset | Size | Field | Notes |
|-------:|-----:|-------|-------|
| 0 | 4 | command id | LE u32, see below |
| 4 | 2 | result mark | 0 in requests; reply: 0 ok, 3 no permission |
| 6 | 2 | sequence | random 1..32727 |
| 8 | 2 | payload length | bytes following the header |
| 10 | 6 | firmware version | ASCII, app echoes it; may be empty |
| 16 | 32 | device id | `ECO-XXXXXXXX` |
| 48 | 32 | alias | device name; may be empty |
| 80 | 32 | password | empty unless set in the app |
| 112 | 4 | date | set by the device in replies |
| 116 | 4 | send time | Unix seconds |
| 120 | 4 | led version / reserved / data type | |
| 124 | 4 | client id | random per app install; any value works |

## Command ids

| Id | Name | Payload |
|---:|------|---------|
| 327702 | MODIFY_SWITCH | 2 bytes: `01 01` on, `01 00` off |
| 327703 | GET_SWITCH_STATUS | none; reply carries 2 bytes, state in the last |
| 327936 | SCHEDULE_ADD | full 388-byte table with the new entry appended |
| 327937 | SCHEDULE_EDIT | full table with one entry changed |
| 327938 | SCHEDULE_DELETE | full table without the removed entry |
| 327939 | SCHEDULE_GETALL | none; reply carries the 388-byte table |
| 327940 | GET_TODAY_TASKTAB | reply: 1 count byte + 20 × 7-byte windows |
| 327730 | READ_PWR_OFFSET | 56-byte `BoxPowerDetect` with a date range; reply carries the same block filled in |
| 327685 | GET_SETTING | reply: 364-byte `BoxSetting`, `TimeZone` at offset 104 |
| 327701 | MODIFY_TIMEZONE | 12-byte `TimeZone`; the app uses it only as a DST toggle |

Replies are capped at 512 bytes, so a 388-byte table arrives 4 bytes short;
treat missing bytes as zero. The plug sends every reply twice.

## Clock (`TimeZone`, 12 bytes)

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 2 | year; bit 12 (0x1000) set means DST on |
| 2 | 1 | month |
| 3 | 1 | day |
| 4 | 4 | seconds since midnight, standard time |
| 8 | 4 | offset in seconds from UTC+8 (the vendor's zone), standard time |

The header's `date` field (offset 112) packs the same clock: 12-bit year,
4-bit month, 5-bit day, 5-bit hour, 6-bit minute.

The app's DST switch sends `MODIFY_TIMEZONE` with year 6111 (2015 | 0x1000)
for on or 2015 for off, month 4, day 18, time 57600, offset 0. Only the flag
matters. With the flag off, on-device timers fire an hour late in summer.

## Schedule table (388 bytes, `ScheduleTask`)

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 2 | entry count (0..12) |
| 4 + 32·i | 32 | entry i |

### Entry (32 bytes, `TaskTable`)

| Offset | Size | Field | Programmable timer value |
|-------:|-----:|-------|--------------------------|
| 0 | 1 | task id | 0..11, lowest free |
| 1 | 1 | task type | 0 timer, 2 astronomic, 3 random |
| 2 | 1 | mode | 2 weekly, 0 countdown |
| 3 | 1 | days | Sun 0x40, Mon 0x20 … Sat 0x01 |
| 4 | 1 | device id | 0 |
| 5 | 1 | start-after flag | countdown only |
| 6 | 2 | start-after seconds | countdown only |
| 8 | 1 | end-after flag | countdown only |
| 10 | 2 | end-after seconds | countdown only |
| 12 | 2 | start year | today |
| 14 | 1 | start month | today |
| 15 | 1 | start day | today |
| 16 | 4 | start time | seconds since midnight |
| 20 | 1 | start status | 1 = switch on |
| 22 | 2 | end year | today |
| 24 | 1 | end month | today |
| 25 | 1 | end day | today |
| 26 | 1 | end status | 0 = switch off |
| 28 | 4 | end time | seconds since midnight |

## Metering block (`BoxPowerDetect`, 56 bytes)

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 1 | flag (high bits of the energy total) |
| 4 | 4 | start year (request) |
| 8 | 1 | start month |
| 9 | 1 | start day |
| 12 | 4 | end year (request) |
| 16 | 1 | end month |
| 17 | 1 | end day |
| 20 | 4 | energy total, raw |
| 24 | 4 | current, raw pulse period in µs |
| 28 | 4 | power, raw |
| 32 | 4 | voltage, raw |
| 36 | 4 | energy calibration divisor |
| 40 | 4 | current calibration divisor |
| 44 | 4 | power calibration divisor |
| 48 | 4 | voltage calibration divisor |
| 52 | 4 | energy now, raw |

The app converts `x = 1 / (raw × 1e-6) / divisor` and scales by 1e3 for amps,
1e5 for volts and 1e6 for watts; energy is `(flag × 2^32 + raw) × 100 /
divisor` kWh. All-zero current, voltage and power means the model has no meter
(the HOWT01A does not; it still answers the command).

## Discovery

128 zero bytes with `00 E0 07 0B` at offset 23 and `11 F7 9D 00` at offset 27,
sent to the broadcast address on port 25. The 408-byte reply is the app's
`DvsSearchRespond`: firmware at 4..10, id at 10..42, name at 42..74, short id
at 74..106, SSID at 120..144. The reply also carries the Wi-Fi password in
clear text, so keep discovery on a trusted network.
