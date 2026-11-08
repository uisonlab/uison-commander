//! Listings as servers send them: `MLSD` / `MLST` facts (RFC 3659), and `LIST` — which has no standard: the
//! `ls -l` of Unix servers and the DOS form of IIS. Times are taken as UTC (what `MLSD` and vsftpd send).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vfs::{Entry, Kind};

/// `type=file;size=3;modify=20200101000000;UNIX.mode=0640; name` — the entry, or `None` for `.` and `..`
/// (`cdir`, `pdir`) and what isn't facts.
pub fn mlsx(line: &str) -> Option<Entry> {
    let (facts, name) = line.split_once(' ')?;
    let mut kind = Kind::File;
    let (mut size, mut modified, mut mode, mut perm) = (None, None, None, None);
    for f in facts.split(';').filter(|f| !f.is_empty()) {
        let (k, v) = f.split_once('=')?;
        match k.to_ascii_lowercase().as_str() {
            "type" => {
                let v = v.to_ascii_lowercase();
                kind = match v.as_str() {
                    "file" => Kind::File,
                    "dir" => Kind::Dir,
                    "cdir" | "pdir" => return None,
                    "os.unix=symlink" => Kind::Symlink { target: None, to_dir: false },
                    _ => match v.strip_prefix("os.unix=slink:") {
                        Some(_) => Kind::Symlink { target: f.split_once(":").map(|(_, t)| t.to_string()), to_dir: false },
                        None => Kind::Other,
                    },
                }
            }
            "size" => size = v.parse().ok(),
            "modify" => modified = timeval(v),
            "unix.mode" => mode = u32::from_str_radix(v, 8).ok(),
            "perm" => perm = Some(v.to_ascii_lowercase()),
            _ => {}
        }
    }
    let name = name.to_string();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut e = Entry::new(name, kind);
    e.size = if e.kind == Kind::File { size } else { None };
    e.modified = modified;
    e.mode = mode;
    // `perm` says what we may do: a file without `w` (write) nor `a` (append) is read-only for us.
    e.readonly = perm.is_some_and(|p| e.kind == Kind::File && !p.contains('w') && !p.contains('a'));
    e.hidden = e.name.starts_with('.');
    Some(e)
}

/// `YYYYMMDDHHMMSS` with or without `.sss` (`MLSD`'s `modify`, the reply of `MDTM`).
pub fn timeval(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let digits = s.split('.').next()?;
    if digits.len() != 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |a: usize, b: usize| digits[a..b].parse::<i64>().ok();
    epoch(n(0, 4)?, n(4, 6)?, n(6, 8)?, n(8, 10)?, n(10, 12)?, n(12, 14)?)
}

/// The other way: `YYYYMMDDHHMMSS` of a time (UTC), for `MFMT` and `MDTM`.
pub fn timeval_of(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A line of `LIST`: Unix `ls -l` or DOS; `None` for `.`, `..`, `total 12` and what can't be read.
/// `now` decides the year of a recent date (`Nov  8 10:16`: this year, unless that is in the future).
pub fn list_line(line: &str, now: SystemTime) -> Option<Entry> {
    let e = match line.as_bytes().first()? {
        b'0'..=b'9' => dos(line)?,
        _ => unix(line, now)?,
    };
    (e.name != "." && e.name != "..").then_some(e)
}

/// Whitespace-separated words of `s` with where each starts.
fn words(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(b)) => {
                out.push((b, &s[b..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(b) = start {
        out.push((b, &s[b..]));
    }
    out
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// `drwxr-xr-x  2 user group  4096 Nov  8 10:16 name` (`-> target` for a link). The owner and group may be
/// missing or numbers; the date is found by its month.
fn unix(line: &str, now: SystemTime) -> Option<Entry> {
    let w = words(line);
    let perms = w.first()?.1;
    if perms.len() < 10 {
        return None;
    }
    // The date: a month, a day, a time or a year; the size is the word before it.
    let i = (2..w.len().saturating_sub(3))
        .find(|&i| MONTHS.contains(&w[i].1.to_ascii_lowercase().as_str()) && w[i + 1].1.parse::<u8>().is_ok_and(|d| (1..=31).contains(&d)))?;
    let (month, day, time) = (w[i].1, w[i + 1].1, w[i + 2].1);
    let size: Option<u64> = w[i - 1].1.parse().ok();
    // The name: after one space past the time or year (it may start with spaces of its own).
    let after = w[i + 2].0 + time.len();
    let name_all = line.get(after + 1..)?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as i64 + 1;
    let day: i64 = day.parse().ok()?;
    let modified = if let Some((h, m)) = time.split_once(':') {
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        let this_year = civil(now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400).0;
        let t = epoch(this_year, month, day, h, m, 0)?;
        // A date ahead of now (more than a day, for time zones) is of last year.
        if t > now + Duration::from_secs(86_400) { epoch(this_year - 1, month, day, h, m, 0) } else { Some(t) }
    } else {
        epoch(time.parse().ok()?, month, day, 0, 0, 0)
    };
    let kind = match perms.as_bytes()[0] {
        b'd' => Kind::Dir,
        b'-' => Kind::File,
        b'l' => Kind::Symlink { target: None, to_dir: false },
        _ => Kind::Other,
    };
    let (name, kind) = match kind {
        Kind::Symlink { .. } => match name_all.split_once(" -> ") {
            Some((n, t)) => (n, Kind::Symlink { target: Some(t.to_string()), to_dir: false }),
            None => (name_all, kind),
        },
        k => (name_all, k),
    };
    if name.is_empty() {
        return None;
    }
    let mut e = Entry::new(name, kind);
    e.size = if e.kind == Kind::File { size } else { None };
    e.modified = modified;
    e.mode = mode_bits(perms);
    e.hidden = e.name.starts_with('.');
    Some(e)
}

/// `rwxr-x---` (after the type letter) as bits; `s`, `t` and their capitals too.
fn mode_bits(perms: &str) -> Option<u32> {
    let b = perms.as_bytes().get(1..10)?;
    let mut m = 0u32;
    for (i, &c) in b.iter().enumerate() {
        let bit = 1 << (8 - i);
        match c {
            b'r' | b'w' | b'x' => m |= bit,
            b's' | b't' => m |= bit,
            b'-' | b'S' | b'T' => {}
            _ => return None,
        }
        // s, S on the owner's and group's x; t, T on the others'.
        match (i, c) {
            (2, b's' | b'S') => m |= 0o4000,
            (5, b's' | b'S') => m |= 0o2000,
            (8, b't' | b'T') => m |= 0o1000,
            _ => {}
        }
    }
    Some(m)
}

/// IIS: `11-08-26  10:16AM       <DIR>          name`, `01-01-2020  12:00PM   3 t.txt`.
fn dos(line: &str) -> Option<Entry> {
    let w = words(line);
    if w.len() < 4 {
        return None;
    }
    let (date, time, what) = (w[0].1, w[1].1, w[2].1);
    let name = line.get(w[3].0..)?;
    let d: Vec<i64> = date.split(['-', '/']).map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [mon, day, y] = d.as_slice() else { return None };
    let year = if *y < 70 {
        2000 + y
    } else if *y < 100 {
        1900 + y
    } else {
        *y
    };
    let upper = time.to_ascii_uppercase();
    let (hm, pm) = match (upper.strip_suffix("PM"), upper.strip_suffix("AM")) {
        (Some(t), _) => (t.to_string(), Some(true)),
        (_, Some(t)) => (t.to_string(), Some(false)),
        _ => (upper.clone(), None),
    };
    let (h, m) = hm.split_once(':')?;
    let (mut h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    match pm {
        Some(true) if h < 12 => h += 12,
        Some(false) if h == 12 => h = 0,
        _ => {}
    }
    let dir = what.eq_ignore_ascii_case("<DIR>");
    let mut e = Entry::new(name, if dir { Kind::Dir } else { Kind::File });
    e.size = if dir { None } else { Some(what.parse().ok()?) };
    e.modified = epoch(year, *mon, *day, h, m, 0);
    e.hidden = e.name.starts_with('.');
    Some(e)
}

/// Seconds since 1970 of a UTC date and time (H. Hinnant's days_from_civil).
fn epoch(y: i64, mon: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<SystemTime> {
    if !(1..=12).contains(&mon) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let y2 = if mon <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mon > 2 { mon - 3 } else { mon + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok().map(|x| UNIX_EPOCH + Duration::from_secs(x))
}

/// The date of a day since 1970 (civil_from_days).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> SystemTime {
        timeval(s).unwrap()
    }

    #[test]
    fn times() {
        assert_eq!(at("20200101000000"), UNIX_EPOCH + Duration::from_secs(1_577_836_800));
        assert_eq!(at("20261108101635.123"), at("20261108101635"));
        assert_eq!(timeval_of(at("20261108101635")), "20261108101635");
        assert_eq!(timeval_of(UNIX_EPOCH + Duration::from_secs(1_600_000_000)), "20200913122640");
        assert!(timeval("2020").is_none() && timeval("2020010100000x").is_none());
    }

    /// pure-ftpd's MLSD and MLST.
    #[test]
    fn facts() {
        let e = mlsx("type=file;size=3;modify=20200101000000;UNIX.mode=0640;UNIX.uid=1000;UNIX.gid=1000;unique=87g33e87; t.txt").unwrap();
        assert_eq!((e.name.as_str(), &e.kind, e.size, e.mode), ("t.txt", &Kind::File, Some(3), Some(0o640)));
        assert_eq!(e.modified, Some(at("20200101000000")));
        let d = mlsx("type=dir;sizd=10;modify=20261108101635;UNIX.mode=0755; with space ").unwrap();
        assert_eq!((d.name.as_str(), &d.kind, d.size), ("with space ", &Kind::Dir, None));
        assert!(mlsx("type=cdir;sizd=10;modify=20261108101635; .").is_none());
        assert!(mlsx("type=pdir;modify=20261108101635; ..").is_none());
        let ro = mlsx("type=file;perm=r;size=1; .hidden").unwrap();
        assert!(ro.readonly && ro.hidden);
        let l = mlsx("type=OS.unix=slink:/etc/hosts;modify=20200101000000; hosts").unwrap();
        assert_eq!(l.kind, Kind::Symlink { target: Some("/etc/hosts".into()), to_dir: false });
    }

    /// vsftpd's, pure-ftpd's, ProFTPD's and a NAS's `ls -l`.
    #[test]
    fn unix_lines() {
        let now = at("20261108120000");
        let e = list_line("-rw-r-----    1 1000     1000            3 Nov 08 10:11 t.txt", now).unwrap();
        assert_eq!((e.name.as_str(), e.size, e.mode), ("t.txt", Some(3), Some(0o640)));
        assert_eq!(e.modified, Some(at("20261108101100")));
        let e = list_line("-rw-r-----    1 1000       uison               3 Jan  1  2020 t.txt", now).unwrap();
        assert_eq!(e.modified, Some(at("20200101000000")));
        // December seen in November is last year's.
        let e = list_line("drwxr-xr-x    2 ftp      ftp          4096 Dec 24 18:00 проценты %20 и #хеш", now).unwrap();
        assert_eq!((e.name.as_str(), &e.kind, e.size), ("проценты %20 и #хеш", &Kind::Dir, None));
        assert_eq!(e.modified, Some(at("20251224180000")));
        let e = list_line("lrwxrwxrwx   1 root root  11 Mar  3  2021 link name -> ../target dir", now).unwrap();
        assert_eq!((e.name.as_str(), &e.kind), ("link name", &Kind::Symlink { target: Some("../target dir".into()), to_dir: false }));
        // No group column (some NAS), the sticky and setuid bits.
        let e = list_line("drwxrwxrwt 1 owner 0 Feb 29 2024 tmp", now).unwrap();
        assert_eq!((e.name.as_str(), e.mode), ("tmp", Some(0o1777)));
        let e = list_line("-rwsr-xr-x   1 root     root        12 Nov  8 09:00  two spaces", now).unwrap();
        assert_eq!((e.name.as_str(), e.mode), (" two spaces", Some(0o4755)));
        assert!(list_line("drwxr-xr-x    1 1000       0                  10 Nov  8 10:16 .", now).is_none());
        assert!(list_line("total 12", now).is_none());
    }

    #[test]
    fn dos_lines() {
        let now = at("20261108120000");
        let d = list_line("11-08-26  10:16AM       <DIR>          My Documents", now).unwrap();
        assert_eq!((d.name.as_str(), &d.kind), ("My Documents", &Kind::Dir));
        assert_eq!(d.modified, Some(at("20261108101600")));
        let f = list_line("01-01-2020  12:30PM                 1234 a b.txt", now).unwrap();
        assert_eq!((f.name.as_str(), f.size), ("a b.txt", Some(1234)));
        assert_eq!(f.modified, Some(at("20200101123000")));
        assert_eq!(list_line("01-01-20  12:05AM                 0 z", now).unwrap().modified, Some(at("20200101000500")));
    }
}
