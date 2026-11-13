//! An entry's modification time, and the two places a ZIP keeps it.
//!
//! **The DOS date-time** in every ZIP header has no zone and a 2-second step. Archivers write and
//! read it as **local** wall time (`tests/zip_time_7z.rs` checks this against 7-Zip), so Cram does
//! the same: writing UTC there showed every Cram-made entry shifted by the zone offset in other
//! tools.
//!
//! **The Info-ZIP extended timestamp**, extra field `0x5455`, carries the modification time as UTC
//! seconds since 1970. Cram writes it on every entry whose time fits, in the local and the central
//! header, so a reader that understands it gets the exact second with no zone guesswork. One kind
//! of entry has it in the local header only: an encrypted file written through the parallel path
//! (see `DoneBody::Archive` in `zip_write.rs`).
//!
//! Reading prefers `0x5455`, then the NTFS field `0x000A` (100 ns ticks since 1601, written by
//! 7-Zip and WinZip), and only then the DOS fields, read as local time.
//!
//! When the platform cannot give a local offset ([`crate::localtime`] returns `None`), the DOS
//! fields are written and read as UTC, which is what Cram did before; `0x5455` is written either
//! way.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::localtime;

/// Header ID of the Info-ZIP extended timestamp ("UT").
pub(crate) const EXT_TIMESTAMP_ID: u16 = 0x5455;

/// Flags byte of an extended timestamp that carries the modification time only.
const EXT_FLAG_MTIME: u8 = 0x01;

/// About the year 3000, far inside `time`'s range. An mtime beyond it is not stored: the value may
/// have come from an untrusted archive through `convert`, and `time` panics past ±9999 years.
const MAX_SANE_UNIX: u64 = 32_503_680_000;

/// Largest extended-timestamp value written or trusted: 2038-01-19T03:14:07Z.
///
/// The field is four bytes, and tools disagree on its sign. Info-ZIP stored a C `time_t`, so a
/// value with the top bit set reads as before 1970 in some readers and after 2038 in others. Cram
/// writes no `0x5455` for a time past this point (the DOS fields still carry it, to 2107), and on
/// reading treats such a value as unreliable and falls back to the next source.
const EXT_MAX: u32 = i32::MAX as u32;

/// Seconds between 1601-01-01 and 1970-01-01, the NTFS and Unix epochs.
const NTFS_TO_UNIX_SECS: u64 = 11_644_473_600;

/// NTFS timestamps count 100 ns ticks.
const NTFS_TICKS_PER_SEC: u64 = 10_000_000;

/// Largest offset honoured, in seconds, whatever its source.
const MAX_OFFSET_SECS: u32 = 86_400;

/// The DOS date-time to write for `modified`: its **local** calendar fields, or its UTC ones when
/// the platform gives no offset. `None` when there is no time, or it is before 1970 or beyond
/// [`MAX_SANE_UNIX`], or the local date falls outside DOS's 1980-2107; the zip crate then writes
/// its default.
pub(crate) fn dos_for_write(modified: Option<SystemTime>) -> Option<zip::DateTime> {
    dos_for_write_with(modified, localtime::utc_offset_at)
}

fn dos_for_write_with(
    modified: Option<SystemTime>,
    offset_at: impl Fn(i64) -> Option<i32>,
) -> Option<zip::DateTime> {
    let secs = modified?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    if secs > MAX_SANE_UNIX {
        return None;
    }
    let utc = i64::try_from(secs).ok()?;
    let local = utc.checked_add(offset(&offset_at, utc))?;
    let odt = time::OffsetDateTime::from_unix_timestamp(local).ok()?;
    zip::DateTime::from_date_and_time(
        u16::try_from(odt.year()).ok()?,
        u8::from(odt.month()),
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
    .ok()
}

/// The `0x5455` payload for `modified`: the flags byte, then the UTC seconds as a little-endian
/// `u32`. `None` when there is no time or it falls outside 1970 to [`EXT_MAX`].
pub(crate) fn ext_timestamp_payload(modified: Option<SystemTime>) -> Option<[u8; 5]> {
    let secs = modified?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let secs = u32::try_from(secs).ok().filter(|s| *s <= EXT_MAX)?;
    let mut out = [0u8; 5];
    out[0] = EXT_FLAG_MTIME;
    out[1..].copy_from_slice(&secs.to_le_bytes());
    Some(out)
}

/// An entry's modification time from what its central header holds: the `0x5455` mtime if present
/// and at most [`EXT_MAX`], else the NTFS mtime if it lands between 1970 and [`MAX_SANE_UNIX`],
/// else the DOS fields as local time. `None` when none of them gives a valid time; extraction then
/// leaves the file's own time alone.
pub(crate) fn mtime_for_read(
    dos: Option<zip::DateTime>,
    ext: Option<u32>,
    ntfs: Option<u64>,
) -> Option<SystemTime> {
    mtime_for_read_with(dos, ext, ntfs, localtime::utc_offset_at)
}

fn mtime_for_read_with(
    dos: Option<zip::DateTime>,
    ext: Option<u32>,
    ntfs: Option<u64>,
    offset_at: impl Fn(i64) -> Option<i32>,
) -> Option<SystemTime> {
    if let Some(secs) = ext.filter(|s| *s <= EXT_MAX) {
        return UNIX_EPOCH.checked_add(Duration::from_secs(u64::from(secs)));
    }
    if let Some(t) = ntfs.and_then(ntfs_to_system) {
        return Some(t);
    }
    dos_as_local(dos?, &offset_at)
}

/// NTFS ticks to a `SystemTime`, or `None` outside 1970 to [`MAX_SANE_UNIX`]. Zero, which some
/// writers leave in the field, lands in 1601 and is therefore ignored.
fn ntfs_to_system(ticks: u64) -> Option<SystemTime> {
    let secs = (ticks / NTFS_TICKS_PER_SEC).checked_sub(NTFS_TO_UNIX_SECS)?;
    if secs > MAX_SANE_UNIX {
        return None;
    }
    // Below one second, so the product is below 10^9 and fits a u32.
    let nanos = u32::try_from((ticks % NTFS_TICKS_PER_SEC) * 100).ok()?;
    UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

/// The DOS fields read as local wall time.
///
/// The offset depends on the instant, which is what is being worked out, so it is found in two
/// steps: take the offset at the fields read as UTC, step back by it, and take the offset there.
/// Away from a daylight-saving change both give the same answer. In the hour a change repeats, the
/// result is one of the two instants that show that wall time; in the hour it skips, which no
/// clock showed, the result is within an hour of the intended time.
fn dos_as_local(dt: zip::DateTime, offset_at: &impl Fn(i64) -> Option<i32>) -> Option<SystemTime> {
    let month = time::Month::try_from(dt.month()).ok()?;
    let date = time::Date::from_calendar_date(i32::from(dt.year()), month, dt.day()).ok()?;
    let tod = time::Time::from_hms(dt.hour(), dt.minute(), dt.second()).ok()?;
    let wall = date.with_time(tod).assume_utc().unix_timestamp();
    let guess = wall.checked_sub(offset(offset_at, wall))?;
    let utc = wall.checked_sub(offset(offset_at, guess))?;
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(utc).ok()?))
}

/// The offset at `unix` in seconds, or 0 (UTC) when there is none or it exceeds a day.
fn offset(offset_at: &impl Fn(i64) -> Option<i32>, unix: i64) -> i64 {
    match offset_at(unix) {
        Some(off) if off.unsigned_abs() <= MAX_OFFSET_SECS => i64::from(off),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2020-01-02T03:04:06Z, an even second (DOS steps by two).
    const WINTER: u64 = 1_577_934_246;
    /// 2020-07-01T12:00:00Z.
    const SUMMER: u64 = 1_593_604_800;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fields(dt: zip::DateTime) -> (u16, u8, u8, u8, u8, u8) {
        (
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
        )
    }

    /// A fixed zone one hour east, another five hours west, and "no answer". The result is checked
    /// against hand-computed calendar fields, so nothing depends on the machine's own zone.
    #[test]
    fn dos_is_written_as_local_wall_time() {
        let east = |_: i64| Some(3600);
        let west = |_: i64| Some(-5 * 3600);
        let none = |_: i64| None;
        assert_eq!(
            fields(dos_for_write_with(Some(at(WINTER)), east).unwrap()),
            (2020, 1, 2, 4, 4, 6)
        );
        // West of UTC crosses back into the previous day.
        assert_eq!(
            fields(dos_for_write_with(Some(at(WINTER)), west).unwrap()),
            (2020, 1, 1, 22, 4, 6)
        );
        // No offset: UTC, as Cram wrote before.
        assert_eq!(
            fields(dos_for_write_with(Some(at(WINTER)), none).unwrap()),
            (2020, 1, 2, 3, 4, 6)
        );
        // An absurd offset is treated as no offset.
        let absurd = |_: i64| Some(i32::MAX);
        assert_eq!(
            fields(dos_for_write_with(Some(at(WINTER)), absurd).unwrap()),
            (2020, 1, 2, 3, 4, 6)
        );
    }

    /// The bounds `zip_datetime` had stay in place: nothing before 1970, nothing past the year 3000,
    /// and no panic on the way.
    #[test]
    fn dos_write_keeps_the_range_guards() {
        let east = |_: i64| Some(3600);
        assert!(dos_for_write_with(None, east).is_none());
        assert!(dos_for_write_with(Some(UNIX_EPOCH), east).is_none());
        let before_1970 = UNIX_EPOCH - Duration::from_secs(86_400);
        assert!(dos_for_write_with(Some(before_1970), east).is_none());
        // ~year 5000: rejected before `time` sees it.
        assert!(dos_for_write_with(Some(at(95_617_584_000)), east).is_none());
        // 1979-12-31T23:30:00Z is 1980 local one hour east, so it is representable there...
        assert_eq!(
            fields(dos_for_write_with(Some(at(315_531_000)), east).unwrap()),
            (1980, 1, 1, 0, 30, 0)
        );
        // ...and not in UTC.
        assert!(dos_for_write_with(Some(at(315_531_000)), |_| None).is_none());
    }

    #[test]
    fn dos_is_read_as_local_wall_time() {
        let dt = zip::DateTime::from_date_and_time(2020, 1, 2, 4, 4, 6).unwrap();
        let east = |_: i64| Some(3600);
        assert_eq!(
            mtime_for_read_with(Some(dt), None, None, east),
            Some(at(WINTER))
        );
        let west = |_: i64| Some(-5 * 3600);
        assert_eq!(
            mtime_for_read_with(Some(dt), None, None, west),
            Some(at(WINTER + 6 * 3600))
        );
        // No offset: the fields are UTC.
        assert_eq!(
            mtime_for_read_with(Some(dt), None, None, |_| None),
            Some(at(WINTER + 3600))
        );
    }

    /// The lookup has to use the offset at the instant, not at the fields read as UTC. This zone
    /// switches from +1 h to +2 h at 2020-03-29T01:00:00Z, the EU rule. Wall time 01:30 that day is
    /// 00:30Z, before the switch; the fields read as UTC (01:30Z) are after it, so a single lookup
    /// would take +2 h and answer 23:30Z the day before.
    #[test]
    fn dos_read_uses_the_offset_at_the_instant() {
        const SWITCH: i64 = 1_585_443_600; // 2020-03-29T01:00:00Z
        let eu = |t: i64| Some(if t < SWITCH { 3600 } else { 7200 });
        let dt = zip::DateTime::from_date_and_time(2020, 3, 29, 1, 30, 0).unwrap();
        assert_eq!(
            mtime_for_read_with(Some(dt), None, None, eu),
            Some(at(SWITCH as u64 - 1800))
        );
        // 03:30 exists only after the switch: 01:30Z.
        let dt = zip::DateTime::from_date_and_time(2020, 3, 29, 3, 30, 0).unwrap();
        assert_eq!(
            mtime_for_read_with(Some(dt), None, None, eu),
            Some(at(SWITCH as u64 + 1800))
        );
        // Round trip across the same zone, a day either side of the switch.
        for t in [SWITCH as u64 - 86_400, SWITCH as u64 + 86_400] {
            let dos = dos_for_write_with(Some(at(t)), eu).unwrap();
            assert_eq!(mtime_for_read_with(Some(dos), None, None, eu), Some(at(t)));
        }
    }

    #[test]
    fn extended_timestamp_wins_then_ntfs_then_dos() {
        let dos = zip::DateTime::from_date_and_time(2001, 1, 1, 0, 0, 0).ok();
        let east = |_: i64| Some(3600);
        let ntfs_summer = (SUMMER + NTFS_TO_UNIX_SECS) * NTFS_TICKS_PER_SEC;
        assert_eq!(
            mtime_for_read_with(dos, Some(WINTER as u32), Some(ntfs_summer), east),
            Some(at(WINTER))
        );
        assert_eq!(
            mtime_for_read_with(dos, None, Some(ntfs_summer), east),
            Some(at(SUMMER))
        );
        // NTFS keeps its sub-second part.
        assert_eq!(
            mtime_for_read_with(dos, None, Some(ntfs_summer + 1_234_567), east),
            Some(at(SUMMER) + Duration::from_nanos(123_456_700))
        );
    }

    /// Hostile values: none may panic, and each falls through to the next source.
    #[test]
    fn hostile_extra_field_values_fall_back() {
        let dt = zip::DateTime::from_date_and_time(2020, 1, 2, 4, 4, 6).unwrap();
        let east = |_: i64| Some(3600);
        let from_dos = Some(at(WINTER));
        // An extended timestamp with the top bit set is ambiguous between tools.
        for ext in [u32::MAX, EXT_MAX + 1] {
            assert_eq!(
                mtime_for_read_with(Some(dt), Some(ext), None, east),
                from_dos
            );
        }
        // NTFS zero (1601), the largest u64, and one tick before 1970.
        let just_before_1970 = NTFS_TO_UNIX_SECS * NTFS_TICKS_PER_SEC - 1;
        for ntfs in [0, u64::MAX, just_before_1970] {
            assert_eq!(
                mtime_for_read_with(Some(dt), None, Some(ntfs), east),
                from_dos
            );
        }
        // Nothing usable anywhere.
        assert_eq!(
            mtime_for_read_with(None, Some(u32::MAX), Some(u64::MAX), east),
            None
        );
        // The extremes of the trusted range itself.
        assert_eq!(
            mtime_for_read_with(None, Some(0), None, east),
            Some(UNIX_EPOCH)
        );
        assert_eq!(
            mtime_for_read_with(None, Some(EXT_MAX), None, east),
            Some(at(u64::from(EXT_MAX)))
        );
    }

    #[test]
    fn extended_timestamp_payload_layout_and_range() {
        assert_eq!(
            ext_timestamp_payload(Some(at(WINTER))),
            Some([0x01, 0xA6, 0x5D, 0x0D, 0x5E])
        );
        assert_eq!(ext_timestamp_payload(None), None);
        assert_eq!(
            ext_timestamp_payload(Some(UNIX_EPOCH - Duration::from_secs(1))),
            None
        );
        assert!(ext_timestamp_payload(Some(at(u64::from(EXT_MAX)))).is_some());
        assert_eq!(
            ext_timestamp_payload(Some(at(u64::from(EXT_MAX) + 1))),
            None
        );
        assert_eq!(ext_timestamp_payload(Some(at(95_617_584_000))), None);
    }
}
