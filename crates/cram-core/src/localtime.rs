//! The local time zone's offset from UTC at a given instant, asked of the operating system.
//!
//! ZIP's DOS date-time is local wall time with no zone, so writing or reading it needs the offset
//! that was in force on that date, daylight saving included. The `time` crate offers this only
//! behind its `local-offset` feature, which would add a dependency (`num_threads`) to the build, so
//! this module calls the platform directly, the way [`crate::hw`] does for hardware facts. The
//! platform split lives here and nowhere else.
//!
//! The answer is `None` when the platform gives none (an unsupported OS, a date the OS call
//! rejects, or an offset beyond a day). Callers then treat the DOS fields as UTC, which is what Cram
//! did everywhere before this module existed.
//!
//! The zone is read on every call, so a long-running process follows a zone change.

/// Largest offset accepted, in seconds. Real zones stay within ±14 h; anything beyond a day is a
/// platform answer that cannot be right and is discarded.
const MAX_OFFSET_SECS: i32 = 86_400;

/// Seconds east of UTC in force at `unix` (seconds since 1970-01-01T00:00:00Z), or `None` when the
/// platform cannot say.
pub(crate) fn utc_offset_at(unix: i64) -> Option<i32> {
    imp::offset_at(unix).filter(|off| off.unsigned_abs() <= MAX_OFFSET_SECS.unsigned_abs())
}

/// Linux and macOS: `localtime_r`, then the `tm_gmtoff` field it fills in.
///
/// `localtime_r` is the reentrant form. It reads `TZ` through `getenv`, which is only unsafe against
/// a concurrent `setenv`; `std::env::set_var` puts that obligation on its caller, and no non-test
/// code in Cram sets environment variables.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    // `time_t` and `c_long` are `i64` on the 64-bit targets CI builds, which makes these
    // conversions identities there; they are kept fallible for the targets where they are not.
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    pub(super) fn offset_at(unix: i64) -> Option<i32> {
        let t = libc::time_t::try_from(unix).ok()?;
        // SAFETY: `libc::tm` is plain data, so all-zero is a valid value of it.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers come from live locals of the right types; `tm` is read only after
        // a non-null return says it was filled.
        let filled = unsafe { libc::localtime_r(&t, &mut tm) };
        if filled.is_null() {
            return None;
        }
        i32::try_from(i64::try_from(tm.tm_gmtoff).ok()?).ok()
    }
}

/// Windows: `SystemTimeToTzSpecificLocalTime` with no explicit zone, which converts with the active
/// zone's rules for the year of the date given. The offset is the difference between the two
/// calendar times.
#[cfg(windows)]
mod imp {
    /// `SYSTEMTIME` from `minwinbase.h`: eight `WORD`s.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SystemTimeToTzSpecificLocalTime(
            zone: *const core::ffi::c_void,
            universal: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }

    pub(super) fn offset_at(unix: i64) -> Option<i32> {
        let utc = time::OffsetDateTime::from_unix_timestamp(unix).ok()?;
        // SYSTEMTIME starts at 1601; the year must also fit a WORD.
        let year = u16::try_from(utc.year()).ok().filter(|y| *y >= 1601)?;
        let universal = SystemTime {
            year,
            month: u16::from(u8::from(utc.month())),
            day_of_week: u16::from(utc.weekday().number_days_from_sunday()),
            day: u16::from(utc.day()),
            hour: u16::from(utc.hour()),
            minute: u16::from(utc.minute()),
            second: u16::from(utc.second()),
            milliseconds: 0,
        };
        let mut local = SystemTime::default();
        // SAFETY: a null zone pointer selects the active time zone, as documented; the other two
        // pointers come from live locals of the documented layout. `local` is read only after a
        // non-zero return says it was filled.
        let ok =
            unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &universal, &mut local) };
        if ok == 0 {
            return None;
        }
        let month = time::Month::try_from(u8::try_from(local.month).ok()?).ok()?;
        let date = time::Date::from_calendar_date(
            i32::from(local.year),
            month,
            u8::try_from(local.day).ok()?,
        )
        .ok()?;
        let tod = time::Time::from_hms(
            u8::try_from(local.hour).ok()?,
            u8::try_from(local.minute).ok()?,
            u8::try_from(local.second).ok()?,
        )
        .ok()?;
        let local_as_utc = date.with_time(tod).assume_utc().unix_timestamp();
        i32::try_from(local_as_utc.checked_sub(unix)?).ok()
    }
}

/// Every other platform: no answer, so callers use UTC.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    pub(super) fn offset_at(_unix: i64) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests cannot assume any particular zone, so they assert what holds in all of them: the
    /// supported platforms answer, the answer is within a day, and it is a whole number of minutes
    /// (every zone in use since 1980 is; the check catches a units mistake such as minutes for
    /// seconds).
    #[test]
    fn supported_platforms_answer_with_a_sane_offset() {
        for unix in [315_532_800_i64, 1_577_934_246, 1_593_561_600, 2_147_483_647] {
            let off = utc_offset_at(unix);
            if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
                let off = off.unwrap_or_else(|| panic!("no offset for {unix}"));
                assert!(off.unsigned_abs() <= 86_400, "offset {off} at {unix}");
                assert_eq!(off % 60, 0, "offset {off} at {unix} is not whole minutes");
            }
        }
    }

    /// A time the OS calls cannot represent must come back as `None`, not as a panic.
    #[test]
    fn out_of_range_instants_do_not_panic() {
        for unix in [i64::MIN, i64::MAX, -20_000_000_000, 400_000_000_000] {
            let _ = utc_offset_at(unix);
        }
    }

    /// A zone with daylight saving, written as a POSIX `TZ` rule so that it needs no zone
    /// database: UTC+1 in winter, UTC+2 from the last Sunday of March to the last Sunday of
    /// October.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const FIXED_TZ: &str = "CET-1CEST,M3.5.0,M10.5.0/3";

    /// Set by [`zone_dependent_tests_pass_under_a_fixed_zone`] on the child process it starts.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const CHILD_FLAG: &str = "CRAM_LOCALTIME_FIXED_ZONE_CHILD";

    /// The offsets under [`FIXED_TZ`], compared with values written here rather than asked of the
    /// platform. A normal run does not know its zone (CI runs in UTC), so this is ignored there and
    /// does nothing without [`CHILD_FLAG`]; the test below runs it with `TZ` set.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "needs TZ set; zone_dependent_tests_pass_under_a_fixed_zone runs it"]
    fn fixed_zone_offsets() {
        if std::env::var_os(CHILD_FLAG).is_none() {
            return;
        }
        assert_eq!(
            std::env::var("TZ").as_deref(),
            Ok(FIXED_TZ),
            "the child must run under the fixed zone"
        );
        // 2020-01-02T03:04:06Z and 2020-07-01T12:00:00Z.
        assert_eq!(utc_offset_at(1_577_934_246), Some(3_600), "January");
        assert_eq!(utc_offset_at(1_593_604_800), Some(7_200), "July");
    }

    /// The zone-dependent tests, run again in a child process whose zone is [`FIXED_TZ`].
    ///
    /// In UTC, local time and UTC are the same reading, so a ZIP writer or reader that ignores the
    /// zone, an offset of 0 and an offset with the wrong sign all pass the ZIP tests, whose expected
    /// values come from [`utc_offset_at`]. Under a zone one or two hours from UTC, the first fails
    /// the ZIP tests and the other two fail [`fixed_zone_offsets`].
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn zone_dependent_tests_pass_under_a_fixed_zone() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "localtime::tests::fixed_zone_offsets",
                "formats::zip::mtime_tests::",
                "formats::zip_write::mtime_guard_tests::mtime_survives_both_writer_paths",
                "formats::zip_write::mtime_guard_tests::encrypted_entries_keep_their_time_on_both_writer_paths",
                "--include-ignored",
            ])
            .env("TZ", FIXED_TZ)
            .env(CHILD_FLAG, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "the run under TZ={FIXED_TZ} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A filter that matches nothing also exits 0, so each test must be seen to pass.
        for name in [
            "localtime::tests::fixed_zone_offsets",
            "formats::zip::mtime_tests::dos_only_entry_reads_as_local_time",
            "formats::zip::mtime_tests::hostile_time_fields_do_not_panic",
            "formats::zip_write::mtime_guard_tests::mtime_survives_both_writer_paths",
            "formats::zip_write::mtime_guard_tests::encrypted_entries_keep_their_time_on_both_writer_paths",
        ] {
            assert!(
                stdout.contains(&format!("test {name} ... ok")),
                "{name} did not pass under TZ={FIXED_TZ}:\n{stdout}"
            );
        }
    }

    /// Windows: the offset in force now must be the zone's current bias as
    /// `GetTimeZoneInformation` reports it. That call does none of the calendar arithmetic that
    /// [`utc_offset_at`] does, so an offset of 0 or one with the wrong sign fails here. On a
    /// machine set to UTC both sides are 0 and this checks only that they agree; the CI Windows
    /// runner is such a machine.
    #[cfg(windows)]
    #[test]
    fn offset_now_matches_the_zone_bias() {
        /// `TIME_ZONE_INFORMATION` from `timezoneapi.h`; each `SYSTEMTIME` is eight `WORD`s. The
        /// names and dates are there for the layout and are not read.
        #[repr(C)]
        #[allow(dead_code)]
        struct TimeZoneInformation {
            bias: i32,
            standard_name: [u16; 32],
            standard_date: [u16; 8],
            standard_bias: i32,
            daylight_name: [u16; 32],
            daylight_date: [u16; 8],
            daylight_bias: i32,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // SAFETY: the struct is plain data, so all-zero is a valid value of it.
        let mut tzi: TimeZoneInformation = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer comes from a live local of the documented layout (172 bytes).
        let id = unsafe { GetTimeZoneInformation(&mut tzi) };
        // Bias is in minutes, and UTC = local time + bias.
        let bias = match id {
            0 => tzi.bias,                     // TIME_ZONE_ID_UNKNOWN: no daylight saving
            1 => tzi.bias + tzi.standard_bias, // TIME_ZONE_ID_STANDARD
            2 => tzi.bias + tzi.daylight_bias, // TIME_ZONE_ID_DAYLIGHT
            other => panic!("GetTimeZoneInformation returned {other:#x}"),
        };
        let expected = -bias * 60;
        if expected == 0 {
            eprintln!("offset_now_matches_the_zone_bias: this machine is on UTC, so 0 = 0");
        }
        assert_eq!(
            utc_offset_at(i64::try_from(now).unwrap()),
            Some(expected),
            "utc_offset_at disagrees with the zone bias ({bias} min, zone id {id})"
        );
    }
}
