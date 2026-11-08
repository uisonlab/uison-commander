//! The local file system as a source. The only place in the program that touches `std::fs`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use vfs::{Cancel, Capabilities, CreateOpts, Entry, Error, Kind, Result, Root, Source, SourceInfo, VPath, Writer};

pub struct LocalFs;

impl LocalFs {
    pub fn new() -> LocalFs {
        LocalFs
    }
}

impl Default for LocalFs {
    fn default() -> Self {
        LocalFs::new()
    }
}

/// `/Users/x` → the OS path. Windows: `/C:/Users` → `C:\Users`, a share `/\\srv\share/x` → `\\srv\share\x`;
/// the root `/` holds the drives and has no OS path of its own.
pub fn os_path(p: &VPath) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.as_str().trim_start_matches('/');
        let mut out = s.replace('/', "\\");
        // A drive or a share by itself: its root folder (`C:` alone is the current folder on that drive).
        if !out.is_empty() && !p.as_str()[1..].contains('/') {
            out.push('\\');
        }
        PathBuf::from(out)
    }
    #[cfg(not(windows))]
    PathBuf::from(p.as_str())
}

/// The OS path → `VPath` (for the start folder given on the command line).
pub fn vpath(p: &Path) -> VPath {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut out = VPath::root();
        for c in p.components() {
            out = match c {
                Component::Prefix(pre) => match pre.kind() {
                    Prefix::Disk(d) | Prefix::VerbatimDisk(d) => out.child(&format!("{}:", (d as char).to_ascii_uppercase())),
                    Prefix::UNC(srv, share) | Prefix::VerbatimUNC(srv, share) => {
                        out.child(&format!(r"\\{}\{}", srv.to_string_lossy(), share.to_string_lossy()))
                    }
                    _ => out.child(&pre.as_os_str().to_string_lossy().replace('/', "\\")),
                },
                Component::RootDir | Component::CurDir => out,
                Component::ParentDir => out.parent().unwrap_or_else(VPath::root),
                Component::Normal(n) => out.child(&n.to_string_lossy()),
            };
        }
        out
    }
    #[cfg(not(windows))]
    VPath::new(&p.to_string_lossy())
}

/// Linux: disks mounted where the desktop (udisks) or the user put them — `/media/<user>/…`,
/// `/run/media/<user>/…` (Fedora, Arch), `/mnt/…` — from the mount table, in its order.
#[cfg(target_os = "linux")]
fn linux_mounts() -> Vec<String> {
    let Ok(table) = fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for line in table.lines() {
        let Some(dir) = line.split(' ').nth(1) else { continue };
        let dir = unescape_mount(dir);
        let under = ["/media/", "/run/media/", "/mnt/"].iter().any(|p| dir.starts_with(p));
        if under && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// The mount table writes a space, tab, newline and backslash in octal: `\040` → ` `.
#[cfg(target_os = "linux")]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Windows: the drives there are (`/C:`, `/D:`…) — what the root `/` holds.
#[cfg(windows)]
fn drives() -> Vec<String> {
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:", (b'A' + i) as char)).collect()
}

/// Windows: free and total bytes of the volume holding `p`.
#[cfg(windows)]
fn volume_space(p: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0).then_some((avail, total))
}

/// A small system file that is not a source's content (a desktop's theme, the MIME database): its text, or `None`
/// if it is not there or not text.
pub fn read_system_text(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok()
}

/// A path typed by the user (the command line): absolute, links resolved; `None` if it doesn't exist.
pub fn resolve(arg: &str) -> Option<VPath> {
    fs::canonicalize(arg).ok().map(|p| vpath(&p))
}

fn entry(name: String, os: &Path, md: &fs::Metadata) -> Entry {
    let ft = md.file_type();
    let mut e = Entry::new(name, Kind::Other);
    let target_md = ft.is_symlink().then(|| fs::metadata(os).ok()).flatten();
    e.kind = if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else if ft.is_symlink() {
        let target = fs::read_link(os).ok().map(|t| t.to_string_lossy().into_owned());
        Kind::Symlink { target, to_dir: target_md.as_ref().is_some_and(|m| m.is_dir()) }
    } else {
        Kind::Other
    };
    // A link shows what it points to: size and time of the target.
    let shown = target_md.as_ref().unwrap_or(md);
    if !shown.is_dir() {
        e.size = Some(shown.len());
    }
    e.modified = shown.modified().ok();
    e.readonly = shown.permissions().readonly();
    e.hidden = e.name.starts_with('.');
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        e.mode = Some(shown.permissions().mode() & 0o7777);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const UF_HIDDEN: u32 = 0x8000;
        e.hidden |= md.st_flags() & UF_HIDDEN != 0;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        e.hidden |= md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
    }
    e
}

/// Same file (the case-only rename on a case-insensitive volume finds the source as the "existing" target).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    // Windows: the full path the volume itself gives, in the case it is stored in.
    #[cfg(not(unix))]
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// macOS: what `statfs` says of the volume holding `p` — its file system type ("apfs", "exfat", "msdos",
/// "smbfs"…) and free bytes.
#[cfg(target_os = "macos")]
fn volume(p: &Path) -> Option<(String, u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    /// `struct statfs` with 64-bit inodes (the only kind on arm64; `statfs$INODE64` on x86_64).
    #[repr(C)]
    struct StatFs {
        bsize: u32,
        iosize: i32,
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        fsid: [i32; 2],
        owner: u32,
        kind: u32,
        flags: u32,
        fssubtype: u32,
        fstypename: [u8; 16],
        mntonname: [u8; 1024],
        mntfromname: [u8; 1024],
        flags_ext: u32,
        reserved: [u32; 7],
    }
    unsafe extern "C" {
        #[cfg_attr(target_arch = "x86_64", link_name = "statfs$INODE64")]
        fn statfs(path: *const std::ffi::c_char, buf: *mut StatFs) -> i32;
    }
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<StatFs>::zeroed();
    if unsafe { statfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    let n = st.fstypename.iter().position(|&b| b == 0).unwrap_or(16);
    let bs = st.bsize as u64;
    Some((String::from_utf8_lossy(&st.fstypename[..n]).into_owned(), st.bavail.saturating_mul(bs), st.blocks.saturating_mul(bs)))
}

/// File systems without extended attributes, where macOS keeps them in `._name` files next to the files
/// (AppleDouble). The kernel creates and removes those itself; Finder hides them, and so do we.
#[cfg(target_os = "macos")]
fn apple_double(dir: &Path) -> bool {
    volume(dir).is_some_and(|(t, _, _)| matches!(t.as_str(), "msdos" | "exfat" | "smbfs" | "nfs" | "webdav"))
}

/// Windows keeps a name only if it can hold it as it is: it drops a trailing dot or space (`report.` would
/// land on `report`), refuses `<>:"|?*` and device names (`CON`, `nul.txt`). Such a name is refused here,
/// before it reaches another file.
fn check_name(path: &VPath) -> Result<()> {
    if !cfg!(windows) || path.parent().is_none_or(|p| p.is_root()) {
        return Ok(());
    }
    let n = path.name();
    let stem = n.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    let bad_char = n.chars().any(|c| c < ' ' || "<>:\"|?*\\".contains(c));
    if n.ends_with(['.', ' ']) || bad_char || device {
        return Err(Error::msg(path, "Windows does not allow this name"));
    }
    Ok(())
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A hidden temporary name next to the target: the rename at the end stays on the same volume. Short and
/// without the target's name, so a name near the 255-byte limit still has room.
fn tmp_near(target: &Path) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".uc-{}-{n}.tmp", std::process::id()))
}

/// Rename without replacing: `AlreadyExists` if `b` is taken, decided by the kernel in one step (macOS:
/// `renamex_np` with `RENAME_EXCL`). Where that is not available — check, then rename.
fn rename_excl(a: &Path, b: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn renamex_np(from: *const std::ffi::c_char, to: *const std::ffi::c_char, flags: u32) -> i32;
        }
        const RENAME_EXCL: u32 = 0x4;
        if let (Ok(ca), Ok(cb)) = (CString::new(a.as_os_str().as_bytes()), CString::new(b.as_os_str().as_bytes())) {
            if unsafe { renamex_np(ca.as_ptr(), cb.as_ptr(), RENAME_EXCL) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            // ENOTSUP, EINVAL: this file system can't do it — the fallback below.
            if !matches!(e.raw_os_error(), Some(45 | 22)) {
                return Err(e);
            }
        }
    }
    if fs::symlink_metadata(b).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(a, b)
}

struct LocalWriter {
    file: Option<File>,
    tmp: PathBuf,
    target: PathBuf,
    vpath: VPath,
    opts: CreateOpts,
}

impl Write for LocalWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.file.as_mut().unwrap().write(b)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().unwrap().flush()
    }
}

impl Writer for LocalWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let f = self.file.take().unwrap();
        let p = self.vpath.clone();
        // The time and permissions asked for: on the temporary file, before it takes the name. A failure here
        // is not worth failing the copy.
        if let Some(t) = self.opts.modified {
            let _ = f.set_modified(t);
        }
        #[cfg(unix)]
        if let Some(m) = self.opts.mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(fs::Permissions::from_mode(m));
        }
        // No fsync, as `cp` and TC: it costs ~0.8 s per GB (measured). The rename below still keeps a cancelled
        // or failed copy from leaving a half-written file under the real name.
        drop(f);
        let r = if self.opts.overwrite {
            fs::rename(&self.tmp, &self.target)
        } else {
            // Without replacing: a hard link fails if the name is taken — atomically. Volumes without
            // hard links (FAT, exFAT) — check, then rename.
            match fs::hard_link(&self.tmp, &self.target) {
                Ok(()) => fs::remove_file(&self.tmp),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
                Err(_) if fs::symlink_metadata(&self.target).is_ok() => Err(io::ErrorKind::AlreadyExists.into()),
                Err(_) => fs::rename(&self.tmp, &self.target),
            }
        };
        r.map_err(|e| Error::io(&p, &e))
    }
}

impl Drop for LocalWriter {
    fn drop(&mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.tmp);
    }
}

impl Source for LocalFs {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: "local".into(),
            title: "local".into(),
            caps: Capabilities {
                write: true,
                rename: true,
                server_copy: cfg!(target_os = "macos"),
                real_dirs: true,
                seek: true,
                permissions: cfg!(unix),
                set_mtime: true,
                // macOS and Windows volumes usually ignore case; a case-sensitive APFS volume is rare.
                case_sensitive: !cfg!(any(target_os = "macos", windows)),
            },
        }
    }

    fn roots(&self, _: &Cancel) -> Result<Vec<Root>> {
        let mut out = vec![Root { path: VPath::root(), title: "/".into() }];
        if cfg!(target_os = "macos")
            && let Ok(rd) = fs::read_dir("/Volumes")
        {
            for e in rd.flatten() {
                // The boot volume is a link to "/" there.
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    out.push(Root { path: VPath::new("/Volumes").child(&name), title: name });
                }
            }
        }
        #[cfg(windows)]
        {
            out.clear();
            out.extend(drives().into_iter().map(|d| Root { path: VPath::root().child(&d), title: d }));
        }
        #[cfg(target_os = "linux")]
        out.extend(linux_mounts().into_iter().map(|m| Root { title: m.rsplit('/').next().unwrap_or(&m).to_string(), path: VPath::new(&m) }));
        if let Some(h) = std::env::home_dir() {
            out.push(Root { path: vpath(&h), title: "~".into() });
        }
        Ok(out)
    }

    fn home(&self) -> VPath {
        std::env::home_dir().map_or_else(VPath::root, |h| vpath(&h))
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        cancel.check()?;
        #[cfg(windows)]
        if dir.is_root() {
            return Ok(drives().into_iter().map(|d| Entry::new(d, Kind::Dir)).collect());
        }
        let os = os_path(dir);
        let rd = fs::read_dir(&os).map_err(|e| Error::io(dir, &e))?;
        #[cfg(target_os = "macos")]
        let sidecars = apple_double(&os);
        #[cfg(not(target_os = "macos"))]
        let sidecars = false;
        let mut out = Vec::new();
        for (i, e) in rd.enumerate() {
            if i % 256 == 0 {
                cancel.check()?;
            }
            let e = e.map_err(|e| Error::io(dir, &e))?;
            let name = e.file_name().to_string_lossy().into_owned();
            if sidecars && name.starts_with("._") {
                continue;
            }
            let path = e.path();
            // An entry that vanished or can't be read is skipped rather than failing the whole folder.
            if let Ok(md) = fs::symlink_metadata(&path) {
                out.push(entry(name, &path, &md));
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        #[cfg(windows)]
        if path.is_root() {
            return Ok(Entry::new(String::new(), Kind::Dir));
        }
        let os = os_path(path);
        let md = fs::symlink_metadata(&os).map_err(|e| Error::io(path, &e))?;
        Ok(entry(path.name().to_string(), &os, &md))
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let os = os_path(path);
        // Look before opening: opening a pipe waits for a writer, a device never ends.
        let md = fs::metadata(&os).map_err(|e| Error::io(path, &e))?;
        if md.is_dir() {
            return Err(Error::IsADirectory(path.clone()));
        }
        if !md.is_file() {
            return Err(Error::msg(path, "not a regular file (a pipe, socket or device)"));
        }
        let mut f = File::open(&os).map_err(|e| Error::io(path, &e))?;
        if offset > 0 {
            f.seek(SeekFrom::Start(offset)).map_err(|e| Error::io(path, &e))?;
        }
        Ok(Box::new(f))
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn Writer>> {
        check_name(path)?;
        let target = os_path(path);
        match fs::symlink_metadata(&target) {
            Ok(m) if m.is_dir() => return Err(Error::IsADirectory(path.clone())),
            Ok(_) if !opts.overwrite => return Err(Error::AlreadyExists(path.clone())),
            _ => {}
        }
        let tmp = tmp_near(&target);
        let file = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| Error::io(path, &e))?;
        Ok(Box::new(LocalWriter { file: Some(file), tmp, target, vpath: path.clone(), opts: opts.clone() }))
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        check_name(path)?;
        let os = os_path(path);
        fs::create_dir(&os).map_err(|e| {
            // Windows says "access denied" for a drive's root (`C:\`): it is there all the same.
            if e.kind() != io::ErrorKind::AlreadyExists && fs::symlink_metadata(&os).is_ok() {
                return Error::AlreadyExists(path.clone());
            }
            Error::io(path, &e)
        })
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        let os = os_path(path);
        let md = fs::symlink_metadata(&os).map_err(|e| Error::io(path, &e))?;
        // Windows: a link to a folder (or a junction) goes away as a folder, though it is not one.
        #[cfg(windows)]
        let dir = md.is_dir() || std::os::windows::fs::FileTypeExt::is_symlink_dir(&md.file_type());
        #[cfg(not(windows))]
        let dir = md.is_dir();
        if dir { fs::remove_dir(&os) } else { fs::remove_file(&os) }.map_err(|e| Error::io(path, &e))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        check_name(to)?;
        let (a, b) = (os_path(from), os_path(to));
        fs::symlink_metadata(&a).map_err(|e| Error::io(from, &e))?;
        if to.starts_with(from) && to != from {
            return Err(Error::msg(to, "cannot move a folder into itself"));
        }
        // The same file under another case of the name: a case-only rename, the one rename that "replaces".
        if same_file(&a, &b) {
            return fs::rename(&a, &b).map_err(|e| Error::io(to, &e));
        }
        rename_excl(&a, &b).map_err(|e| Error::io(to, &e))
    }

    /// APFS clone: instant and takes no space. Anything else (other volume, other file system) —
    /// `Unsupported`, and the data is streamed with progress.
    #[cfg(target_os = "macos")]
    fn copy_within(&self, from: &VPath, to: &VPath, cancel: &Cancel) -> Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn clonefile(src: *const std::ffi::c_char, dst: *const std::ffi::c_char, flags: u32) -> i32;
        }
        cancel.check()?;
        let (a, b) = (os_path(from), os_path(to));
        if !fs::symlink_metadata(&a).map_err(|e| Error::io(from, &e))?.is_file() {
            return Err(Error::Unsupported);
        }
        // clonefile doesn't replace: clone next to the target, then rename over it.
        let tmp = tmp_near(&b);
        let (ca, ct) = (CString::new(a.as_os_str().as_bytes()), CString::new(tmp.as_os_str().as_bytes()));
        let (Ok(ca), Ok(ct)) = (ca, ct) else { return Err(Error::Unsupported) };
        if unsafe { clonefile(ca.as_ptr(), ct.as_ptr(), 0) } != 0 {
            let e = io::Error::last_os_error();
            // ENOTSUP, EXDEV: not this volume pair.
            return match e.raw_os_error() {
                Some(45 | 18) => Err(Error::Unsupported),
                _ => Err(Error::io(to, &e)),
            };
        }
        fs::rename(&tmp, &b).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Error::io(to, &e)
        })
    }

    fn set_mtime(&self, path: &VPath, t: SystemTime) -> Result<()> {
        let os = os_path(path);
        // Windows changes times only through a handle opened for that (a folder's — with backup semantics).
        #[cfg(windows)]
        let f = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(&os)
        };
        #[cfg(not(windows))]
        let f = File::open(&os);
        f.map_err(|e| Error::io(path, &e))?.set_modified(t).map_err(|e| Error::io(path, &e))
    }

    #[cfg(unix)]
    fn set_mode(&self, path: &VPath, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(os_path(path), fs::Permissions::from_mode(mode)).map_err(|e| Error::io(path, &e))
    }

    /// macOS asks the volume (`pathconf` `_PC_CASE_SENSITIVE`): APFS and HFS+ ignore case unless formatted
    /// case-sensitive, exFAT always does.
    #[cfg(target_os = "macos")]
    fn case_sensitive(&self, dir: &VPath) -> bool {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn pathconf(path: *const std::ffi::c_char, name: i32) -> std::ffi::c_long;
        }
        const PC_CASE_SENSITIVE: i32 = 11;
        // The nearest existing folder: the target may not be made yet.
        let mut p = os_path(dir);
        while fs::symlink_metadata(&p).is_err() && p.pop() {}
        let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
        unsafe { pathconf(c.as_ptr(), PC_CASE_SENSITIVE) == 1 }
    }

    fn local_path(&self, path: &VPath) -> Option<PathBuf> {
        Some(os_path(path))
    }

    #[cfg(target_os = "macos")]
    fn space(&self, dir: &VPath) -> Option<(u64, u64)> {
        volume(&os_path(dir)).map(|(_, free, total)| (free, total))
    }

    #[cfg(windows)]
    fn space(&self, dir: &VPath) -> Option<(u64, u64)> {
        if dir.is_root() {
            return None;
        }
        volume_space(&os_path(dir))
    }

    /// The same inode: `/tmp/x` and `/private/tmp/x`, a path through a link, a hard link, another case.
    fn same_file(&self, a: &VPath, b: &VPath) -> bool {
        a == b || same_file(&os_path(a), &os_path(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_table_escapes() {
        assert_eq!(unescape_mount(r"/media/u/My\040Disk"), "/media/u/My Disk");
        assert_eq!(unescape_mount(r"/mnt/a\134b"), r"/mnt/a\b");
        assert_eq!(unescape_mount(r"/mnt/x\04"), r"/mnt/x\04");
        assert!(linux_mounts().iter().all(|m| m.starts_with('/')));
    }

    /// A fresh empty folder under the system temp folder.
    pub fn scratch(tag: &str) -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("uc-test-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn local_passes_conformance() {
        let dir = scratch("conf");
        vfs::conformance::run(&LocalFs, &vpath(&dir));
        fs::remove_dir_all(dir).unwrap();
    }

    /// The contract on another file system: `UISON_LOCAL_VOLUME=/Volumes/x` (an exFAT or HFS+ disk image).
    #[test]
    fn conformance_on_another_volume() {
        let Some(v) = std::env::var_os("UISON_LOCAL_VOLUME") else { return eprintln!("UISON_LOCAL_VOLUME is not set: skipped") };
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(&v).join(format!("uc-conf-{}-{n}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        vfs::conformance::run(&LocalFs, &vpath(&dir));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths() {
        for (os, v) in [(r"C:\Users\x", "/C:/Users/x"), (r"\\?\C:\Users", "/C:/Users"), (r"d:\", "/D:"), (r"\\srv\share\a", r"/\\srv\share/a")] {
            assert_eq!(vpath(Path::new(os)).as_str(), v, "{os}");
        }
        for (v, os) in [("/C:/Users/x", r"C:\Users\x"), ("/D:", r"D:\"), (r"/\\srv\share", r"\\srv\share\"), ("/", "")] {
            assert_eq!(os_path(&VPath::new(v)), PathBuf::from(os), "{v}");
        }
        let drives = LocalFs.list(&VPath::root(), &Cancel::new()).unwrap();
        assert!(drives.iter().any(|e| e.name == "C:" && e.is_dir()), "{drives:?}");
        let tmp = vpath(&std::env::temp_dir());
        assert!(LocalFs.list(&tmp, &Cancel::new()).is_ok(), "{tmp}");
        assert!(LocalFs.space(&tmp).is_some_and(|(free, total)| free > 0 && total >= free));
    }

    #[cfg(unix)]
    #[test]
    fn links_show_their_target() {
        let dir = scratch("links");
        fs::create_dir(dir.join("d")).unwrap();
        fs::write(dir.join("f"), b"12345").unwrap();
        std::os::unix::fs::symlink(dir.join("d"), dir.join("ld")).unwrap();
        std::os::unix::fs::symlink("f", dir.join("lf")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.join("broken")).unwrap();
        let mut es = LocalFs.list(&vpath(&dir), &Cancel::new()).unwrap();
        es.sort_by(|a, b| a.name.cmp(&b.name));
        let kinds: Vec<(&str, bool, Option<u64>)> = es.iter().map(|e| (e.name.as_str(), e.is_dir(), e.size)).collect();
        assert_eq!(kinds, [("broken", false, Some(7)), ("d", true, None), ("f", false, Some(5)), ("ld", true, None), ("lf", false, Some(5))]);
        assert_eq!(es[4].kind, Kind::Symlink { target: Some("f".into()), to_dir: false });
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clone_replaces_the_target() {
        let dir = scratch("clone");
        fs::write(dir.join("a"), b"new").unwrap();
        fs::write(dir.join("b"), b"old").unwrap();
        let base = vpath(&dir);
        LocalFs.copy_within(&base.child("a"), &base.child("b"), &Cancel::new()).unwrap();
        assert_eq!(fs::read(dir.join("b")).unwrap(), b"new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "no temporary files left");
        fs::remove_dir_all(dir).unwrap();
    }
}
