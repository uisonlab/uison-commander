//! Sizes, dates and attributes as Total Commander shows them.

use std::time::{SystemTime, UNIX_EPOCH};

/// `2113004` → `2 113 004`.
pub fn bytes(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Short size, everywhere the same (binary, as TC): `0 B`, `512 B`, `12 KB`, `12.4 MB`, `3.1 GB`
/// (`12,4 МБ` in Russian).
pub fn short(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if n < 1024 {
        format!("{n} {}", t!("B"))
    } else if f < K * K {
        format!("{} {}", bytes(n.div_ceil(1024)), t!("KB"))
    } else if f < K * K * K {
        format!("{} {}", crate::i18n::decimal(f / (K * K)), t!("MB"))
    } else {
        format!("{} {}", crate::i18n::decimal(f / (K * K * K)), t!("GB"))
    }
}

/// `1 file`, `3 files` (the noun in its Russian form, in Russian).
pub fn count(n: u64, one: &str) -> String {
    crate::i18n::count(n, one)
}

/// Time left, roughly: `5 s`, `2 min 10 s`, `1 h 05 min`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => t!("{} s", secs),
        60..3600 => t!("{} min {} s", secs / 60, format!("{:02}", secs % 60)),
        _ => t!("{} h {} min", secs / 3600, format!("{:02}", secs / 60 % 60)),
    }
}

/// Local date and time: `30.10.26 14:02`.
pub fn date(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let tm = local(secs);
    format!("{:02}.{:02}.{:02} {:02}:{:02}", tm.mday, tm.mon + 1, tm.year.rem_euclid(100), tm.hour, tm.min)
}

/// Full date for Properties: `2026-10-30 14:02:05`.
pub fn date_long(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let tm = local(secs);
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02}", tm.year, tm.mon + 1, tm.mday, tm.hour, tm.min, tm.sec)
}

/// `rwxr-xr-x`.
pub fn mode(m: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let b = (m >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub struct Tm {
    pub year: i32,
    /// 0…11.
    pub mon: i32,
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
}

/// std has no local time: `localtime_r` from libc (the time zone and DST of the system).
#[cfg(unix)]
fn local(secs: i64) -> Tm {
    #[repr(C)]
    struct CTm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: std::ffi::c_long,
        zone: *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut CTm) -> *mut CTm;
    }
    let mut tm = CTm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 70, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null() };
    unsafe { localtime_r(&secs, &mut tm) };
    Tm { year: tm.year + 1900, mon: tm.mon, mday: tm.mday, hour: tm.hour, min: tm.min, sec: tm.sec }
}

/// Elsewhere for now: UTC.
#[cfg(not(unix))]
fn local(secs: i64) -> Tm {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as i32;
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = (yoe + era * 400) as i32 + (m <= 2) as i32;
    Tm { year: y, mon: m - 1, mday: d, hour: rem / 3600, min: rem / 60 % 60, sec: rem % 60 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(bytes(0), "0");
        assert_eq!(bytes(999), "999");
        assert_eq!(bytes(1000), "1 000");
        assert_eq!(bytes(2_113_004), "2 113 004");
        assert_eq!(short(0), "0 B");
        assert_eq!(short(1023), "1023 B");
        assert_eq!(short(1025), "2 KB");
        assert_eq!(short(5 * 1024 * 1024 + 300_000), "5.3 MB");
        assert_eq!((count(1, "file"), count(1200, "folder")), ("1 file".to_string(), "1 200 folders".to_string()));
        assert_eq!((duration(5), duration(130), duration(3900)), ("5 s".to_string(), "2 min 10 s".to_string(), "1 h 05 min".to_string()));
        assert_eq!(mode(0o755), "rwxr-xr-x");
        assert_eq!(mode(0o640), "rw-r-----");
    }

    #[test]
    fn dates_are_local() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let s = date(t);
        assert_eq!(s.len(), "14.11.23 22:13".len(), "{s}");
        assert!(s.contains(".11.23 "), "{s}");
    }
}
