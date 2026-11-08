//! Rules of the architecture that the compiler doesn't check.

use std::path::Path;

/// `std::fs` lives only in `vfs-local`: everything else reaches files through `vfs::Source`.
/// (Tests may prepare the disk with it.)
#[test]
fn std_fs_only_in_vfs_local() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut bad = Vec::new();
    for c in std::fs::read_dir(crates).unwrap().flatten() {
        if c.file_name() == "vfs-local" {
            continue;
        }
        let mut stack = vec![c.path().join("src")];
        while let Some(p) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&p) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    for (i, line) in src.lines().enumerate() {
                        let code = line.split("//").next().unwrap_or("");
                        if code.contains("std::fs") || code.contains("use std::{fs") {
                            bad.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                        }
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(), "std::fs outside vfs-local:\n{}", bad.join("\n"));
}

/// A Rust string literal starting at `s[i]` (the `"`): its text and where it ends.
fn literal(s: &str, i: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let (mut out, mut j) = (String::new(), i + 1);
    while j < b.len() && b[j] != b'"' {
        if b[j] == b'\\' {
            match b.get(j + 1)? {
                b'n' => out.push('\n'),
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                // A line continuation: the line break and the next line's indent are not in the text.
                b'\n' => {
                    j += 2;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    continue;
                }
                c => out.push(*c as char),
            }
            j += 2;
        } else {
            let ch = s[j..].chars().next()?;
            out.push(ch);
            j += ch.len_utf8();
        }
    }
    Some((out, j + 1))
}

/// Every `t!("…")` of the program has its Russian (the screens checked in Russian don't reach them all).
#[test]
fn every_text_is_in_the_russian_table() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let table = std::fs::read_to_string(src.join("i18n.rs")).unwrap();
    let start = table.find("const RU: &[(&str, &str)] = &[").unwrap();
    let body = &table[start..start + table[start..].find("\n];").unwrap()];
    let mut known = std::collections::HashSet::new();
    // Each entry: `(` then its English literal (on the same line or the next).
    let mut at = 0;
    while let Some(k) = body[at..].find('(') {
        let i = at + k + 1;
        let j = i + body[i..].len() - body[i..].trim_start().len();
        match literal(body, j) {
            Some((en, end)) => {
                known.insert(en);
                // Past the Russian too.
                let r = end + body[end..].find('"').unwrap_or(0);
                at = literal(body, r).map_or(end, |(_, e)| e);
            }
            None => at = i,
        }
    }
    assert!(known.len() > 100, "the table was not read: {}", known.len());
    let mut missing = Vec::new();
    let mut stack = vec![src];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let s = std::fs::read_to_string(&p).unwrap();
            let mut at = 0;
            while let Some(k) = s[at..].find("t!(") {
                let i = at + k;
                at = i + 3;
                // `t!(`, not `format!(`; and not in a comment.
                let line = &s[s[..i].rfind('\n').map_or(0, |n| n + 1)..i];
                if s[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') || line.contains("//") {
                    continue;
                }
                let j = at + s[at..].len() - s[at..].trim_start().len();
                if let Some((en, _)) = literal(&s, j)
                    && en.bytes().any(|b| b.is_ascii_alphabetic())
                    && !known.contains(&en)
                {
                    missing.push(format!("{}: {en:?}", p.file_name().unwrap().to_string_lossy()));
                }
            }
        }
    }
    assert!(missing.is_empty(), "not in the Russian table of i18n.rs:\n{}", missing.join("\n"));
}
