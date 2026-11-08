//! Rename masks as in Total Commander: `*.bak`, `name.*`, `*_old.*`, `???-*`.

use vfs::split_ext;

/// Apply a mask to a name. In the mask, `*` stands for the original part (base or extension), `?` for the
/// original character at that place; everything else is taken literally.
pub fn apply_mask(name: &str, mask: &str) -> String {
    let (base, ext) = split_ext(name);
    let (mbase, mext) = match mask.rfind('.') {
        Some(i) => (&mask[..i], Some(&mask[i + 1..])),
        None => (mask, None),
    };
    let mut out = part(base, mbase);
    // A mask without a dot keeps no extension, as in TC ("*" → base only).
    if let Some(m) = mext {
        let e = part(ext, m);
        if !e.is_empty() {
            out.push('.');
            out.push_str(&e);
        }
    }
    out
}

fn part(orig: &str, mask: &str) -> String {
    let chars: Vec<char> = orig.chars().collect();
    let mut out = String::new();
    for (i, c) in mask.chars().enumerate() {
        match c {
            '*' => out.push_str(orig),
            '?' => {
                if let Some(&o) = chars.get(i) {
                    out.push(o);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// A mask has wildcards: the target is a pattern, not a name.
pub fn is_mask(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Does a name match a selection mask (`*.txt`, `a*`, `*.*`)? Several masks are separated by `;` or spaces;
/// a mask with a space goes in quotes (`"my file*"`), as in TC. Case-insensitive, as selection masks in TC.
pub fn matches_mask(name: &str, masks: &str) -> bool {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    split_masks(masks).iter().any(|m| {
        // `*.*` matches names without a dot too.
        let m = if m == "*.*" { "*" } else { m.as_str() };
        glob(&m.to_lowercase().chars().collect::<Vec<_>>(), &name)
    })
}

fn split_masks(masks: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in masks.chars() {
        match c {
            '"' => quoted = !quoted,
            ';' | ' ' if !quoted => out.extend((!cur.is_empty()).then(|| std::mem::take(&mut cur))),
            c => cur.push(c),
        }
    }
    out.extend((!cur.is_empty()).then_some(cur));
    out
}

/// `*` and `?` without backtracking blow-up: on a mismatch go back only to the last `*` (O(pattern × name)).
fn glob(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
            }
            Some(&c) if c == '?' || c == s[si] => {
                pi += 1;
                si += 1;
            }
            _ => match star {
                // The last `*` takes one more character.
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_masks() {
        assert_eq!(apply_mask("report.txt", "*.bak"), "report.bak");
        assert_eq!(apply_mask("report.txt", "new.*"), "new.txt");
        assert_eq!(apply_mask("report.txt", "*_old.*"), "report_old.txt");
        assert_eq!(apply_mask("report.txt", "*.*"), "report.txt");
        assert_eq!(apply_mask("report.txt", "???.*"), "rep.txt");
        assert_eq!(apply_mask("noext", "*.bak"), "noext.bak");
        assert_eq!(apply_mask("noext", "*.*"), "noext");
        assert_eq!(apply_mask("a.tar.gz", "*.tgz"), "a.tar.tgz");
    }

    #[test]
    fn selection_masks() {
        assert!(matches_mask("Photo.JPG", "*.jpg"));
        assert!(matches_mask("noext", "*.*"));
        assert!(matches_mask("a.txt", "*.md;*.txt"));
        assert!(!matches_mask("a.txt", "*.md"));
        assert!(matches_mask("abc", "a?c"));
        assert!(!matches_mask("abcd", "a?c"));
        // Quotes keep a space inside one mask.
        assert!(matches_mask("my file.txt", "\"my file*\" *.md"));
        assert!(!matches_mask("my", "\"my file*\""));
    }

    #[test]
    fn many_stars_are_fast() {
        let name = "a".repeat(60);
        let t = std::time::Instant::now();
        assert!(!matches_mask(&name, "*a*a*a*a*a*a*a*a*b"));
        assert!(matches_mask(&name, "*a*a*a*a*a*a*a*a*"));
        assert!(t.elapsed().as_millis() < 100, "{:?}", t.elapsed());
    }
}
