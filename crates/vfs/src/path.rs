//! A path inside a source: always absolute, always '/', independent of the OS.

use std::fmt;

/// `/`, `/home/u`, `/bucket/dir/key`. Normalized: no empty, `.` or `..` components, no trailing '/'
/// (except the root). How it maps onto the real thing is the source's business: LocalFs on Windows turns
/// `/C:/Users` into `C:\Users`, S3 turns `/bucket/a/b` into bucket + key `a/b`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct VPath(String);

impl VPath {
    pub fn root() -> VPath {
        VPath("/".into())
    }

    /// Parse and normalize; a relative path counts from the root, `..` above the root stays at the root.
    pub fn new(s: &str) -> VPath {
        VPath::root().join(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// The last component; empty for the root.
    pub fn name(&self) -> &str {
        &self.0[self.0.rfind('/').map_or(0, |i| i + 1)..]
    }

    pub fn parent(&self) -> Option<VPath> {
        if self.is_root() {
            return None;
        }
        let i = self.0.rfind('/').unwrap_or(0);
        Some(if i == 0 { VPath::root() } else { VPath(self.0[..i].into()) })
    }

    /// `rel` may hold several components, `.` and `..`; starting with '/' it is absolute.
    pub fn join(&self, rel: &str) -> VPath {
        let mut parts: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { self.components().collect() };
        for c in rel.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                c => parts.push(c),
            }
        }
        if parts.is_empty() {
            return VPath::root();
        }
        let mut s = String::with_capacity(rel.len() + self.0.len() + 1);
        for p in parts {
            s.push('/');
            s.push_str(p);
        }
        VPath(s)
    }

    /// A single child by name: unlike `join`, the name is taken as it is (a name may be "..." or contain '\').
    /// The name must be valid ([`valid_name`]); a name from a person goes through [`VPath::try_child`].
    pub fn child(&self, name: &str) -> VPath {
        debug_assert!(!name.is_empty() && !name.contains('/') && name != "." && name != "..", "bad name '{name}'");
        if self.is_root() { VPath(format!("/{name}")) } else { VPath(format!("{}/{name}", self.0)) }
    }

    /// A child by a name a person typed: `None` for an empty name, `.`, `..` or a name with '/'.
    pub fn try_child(&self, name: &str) -> Option<VPath> {
        valid_name(name).then(|| self.child(name))
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// `self` is `base` or lies inside it (by components: `/ab` is not inside `/a`).
    pub fn starts_with(&self, base: &VPath) -> bool {
        base.is_root() || self.0 == base.0 || (self.0.starts_with(&base.0) && self.0.as_bytes()[base.0.len()] == b'/')
    }

    /// The part of `self` below `base`: `/a/b/c` below `/a` is `b/c`; `None` if not inside.
    pub fn strip_prefix(&self, base: &VPath) -> Option<&str> {
        if !self.starts_with(base) {
            return None;
        }
        let rest = if base.is_root() { &self.0[1..] } else { &self.0[base.0.len()..] };
        Some(rest.trim_start_matches('/'))
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A name that can be one component of a path: not empty, not `.` or `..`, without '/' or NUL.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Split a name into base and extension as file managers show it: `report.pdf` → (`report`, `pdf`),
/// `.bashrc` → (`.bashrc`, ``), `archive.tar.gz` → (`archive.tar`, `gz`).
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_navigate() {
        assert_eq!(VPath::new("").as_str(), "/");
        assert_eq!(VPath::new("/a//b/./c/").as_str(), "/a/b/c");
        assert_eq!(VPath::new("/a/b/../c").as_str(), "/a/c");
        assert_eq!(VPath::new("/../..").as_str(), "/");
        let p = VPath::new("/home/u");
        assert_eq!(p.name(), "u");
        assert_eq!(p.parent().unwrap().as_str(), "/home");
        assert_eq!(p.parent().unwrap().parent().unwrap(), VPath::root());
        assert_eq!(VPath::root().parent(), None);
        assert_eq!(VPath::root().name(), "");
        assert_eq!(p.join("x/y").as_str(), "/home/u/x/y");
        assert_eq!(p.join("/etc").as_str(), "/etc");
        assert_eq!(p.child("a b").as_str(), "/home/u/a b");
        assert_eq!(VPath::root().child("x").as_str(), "/x");
        assert_eq!(p.try_child("a/b"), None);
        assert_eq!(p.try_child(".."), None);
        assert_eq!(p.try_child(""), None);
        assert_eq!(p.try_child("..."), Some(VPath::new("/home/u/...")));
    }

    #[test]
    fn prefixes() {
        let a = VPath::new("/a");
        assert!(VPath::new("/a/b").starts_with(&a));
        assert!(a.starts_with(&a));
        assert!(!VPath::new("/ab").starts_with(&a));
        assert!(a.starts_with(&VPath::root()));
        assert_eq!(VPath::new("/a/b/c").strip_prefix(&a), Some("b/c"));
        assert_eq!(VPath::new("/a/b").strip_prefix(&VPath::root()), Some("a/b"));
        assert_eq!(a.strip_prefix(&a), Some(""));
        assert_eq!(VPath::new("/ab").strip_prefix(&a), None);
    }

    #[test]
    fn extensions() {
        assert_eq!(split_ext("report.pdf"), ("report", "pdf"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(split_ext("archive.tar.gz"), ("archive.tar", "gz"));
        assert_eq!(split_ext("noext"), ("noext", ""));
        assert_eq!(split_ext("dot."), ("dot.", ""));
    }
}
