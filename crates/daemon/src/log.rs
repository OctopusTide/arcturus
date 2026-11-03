//! Daemon log lines on stderr: `<time> LEVEL message`. Time is UTC by default; the binary
//! installs the platform's local-time formatter at startup.

use std::fmt::Arguments;
use std::sync::OnceLock;

static TIME_FORMAT: OnceLock<fn(i64) -> String> = OnceLock::new();

/// Installs the timestamp formatter (unix ms → text). Only the first call counts.
pub fn set_time_format(f: fn(i64) -> String) {
    let _ = TIME_FORMAT.set(f);
}

/// `YYYY-MM-DD HH:MM:SSZ` in UTC, computed without OS calls.
pub fn utc_time(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

pub fn emit(level: &str, args: Arguments<'_>) {
    let ts = TIME_FORMAT.get().copied().unwrap_or(utc_time)(crate::store::now_ms());
    eprintln!("{ts} {level:<5} {args}");
}

/// `dlog!("INFO", "listening on {endpoint}")`
#[macro_export]
macro_rules! dlog {
    ($level:literal, $($arg:tt)*) => {
        $crate::log::emit($level, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_time_is_correct() {
        assert_eq!(super::utc_time(0), "1970-01-01 00:00:00Z");
        assert_eq!(super::utc_time(1_700_000_000_000), "2023-11-14 22:13:20Z");
        assert_eq!(super::utc_time(951_782_400_000), "2000-02-29 00:00:00Z");
    }
}
