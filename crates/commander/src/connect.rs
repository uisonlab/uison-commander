//! Connecting to a storage: what to ask and how to connect. The UI knows only this list — which kinds
//! exist is a matter of cargo features (`s3`, `sftp`) — and never a concrete source.
//!
//! A connection that worked is remembered as a profile in `~/.config/uison-commander/connections.json`:
//! what was typed, without the secrets (passwords, secret keys). One that needs no secret (ssh-agent, a key
//! without a passphrase) connects again by itself; one that does asks for it, with the rest filled in.

use std::io::{Read, Write};
use std::sync::Arc;

use vfs::{ActionArgs, CreateOpts, Param, Source, VPath};
use vfs_local::LocalFs;

/// A remembered connection.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// The connector: `s3`, `s3-compatible`, `sftp`, `ftp`, `webdav`.
    pub kind: String,
    /// The source's id once connected (`sftp://u@host`): what the state file names.
    pub id: String,
    pub title: String,
    /// What was typed, secrets left out.
    pub args: Vec<(String, String)>,
    /// The secret parameters that were filled in: they have to be asked again.
    pub secrets: Vec<String>,
}

impl Profile {
    /// From a dialog's answers and the source they connected to.
    pub fn of(c: &Connector, args: &ActionArgs, src: &dyn Source) -> Profile {
        let secret_ids: Vec<String> = (c.params)().iter().filter(|p| matches!(p, Param::Secret { .. })).map(|p| p.id().to_string()).collect();
        let info = src.info();
        Profile {
            kind: c.id.to_string(),
            id: info.id,
            title: info.title,
            args: args.0.iter().filter(|(k, _)| !secret_ids.contains(k) && !NOT_KEPT.contains(&k.as_str())).cloned().collect(),
            secrets: args.0.iter().filter(|(k, v)| secret_ids.contains(k) && !v.is_empty()).map(|(k, _)| k.clone()).collect(),
        }
    }

    /// The connector's parameters with what the profile remembers as the defaults.
    pub fn params(&self, c: &Connector) -> Vec<Param> {
        let mut ps = (c.params)();
        for p in &mut ps {
            if let Param::Text { id, default, .. } | Param::Choice { id, default, .. } = p
                && let Some((_, v)) = self.args.iter().find(|(k, _)| k == id)
            {
                *default = v.clone();
            }
        }
        ps
    }
}

fn file() -> Option<VPath> {
    Some(crate::state::config_dir()?.child("connections.json"))
}

/// At startup: a missing or broken file is no profiles.
pub fn load() -> Vec<Profile> {
    let Some(path) = file() else { return Vec::new() };
    let mut s = String::new();
    if LocalFs.open_read(&path, 0).and_then(|mut r| r.read_to_string(&mut s).map_err(|e| vfs::Error::io(&path, &e))).is_err() {
        return Vec::new();
    }
    parse(&s)
}

fn parse(s: &str) -> Vec<Profile> {
    let Ok(v) = uison::json::parse(s) else { return Vec::new() };
    let Some(list) = v.get("connections").and_then(|l| l.as_array()) else { return Vec::new() };
    let pairs = |o: Option<&uison::json::Value<'_>>| -> Vec<(String, String)> {
        o.and_then(|o| o.as_object())
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.to_string(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default()
    };
    list.iter()
        .filter_map(|p| {
            let s = |k: &str| p.get(k).and_then(|x| x.as_str()).map(str::to_string);
            let args = pairs(p.get("args"));
            Some(Profile {
                kind: s3_kind(s("kind")?, &args),
                id: s("id")?,
                title: s("title").unwrap_or_default(),
                args,
                secrets: p
                    .get("secrets")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Amazon and the compatible storages were one connector, told apart by the endpoint: a profile of then
/// with one is of the compatible storages now.
fn s3_kind(kind: String, args: &[(String, String)]) -> String {
    if kind == "s3" && args.iter().any(|(k, v)| k == "endpoint" && !v.trim().is_empty()) { "s3-compatible".into() } else { kind }
}

pub fn to_json(list: &[Profile]) -> String {
    let w = |out: &mut String, s: &str| uison::json::write_str(out, s);
    let mut s = String::from("{\n  \"connections\": [");
    for (i, p) in list.iter().enumerate() {
        s.push_str(if i == 0 { "\n    { \"kind\": " } else { ",\n    { \"kind\": " });
        w(&mut s, &p.kind);
        s.push_str(", \"id\": ");
        w(&mut s, &p.id);
        s.push_str(", \"title\": ");
        w(&mut s, &p.title);
        s.push_str(", \"args\": {");
        for (j, (k, v)) in p.args.iter().enumerate() {
            s.push_str(if j == 0 { " " } else { ", " });
            w(&mut s, k);
            s.push_str(": ");
            w(&mut s, v);
        }
        s.push_str(" }, \"secrets\": [");
        for (j, k) in p.secrets.iter().enumerate() {
            if j > 0 {
                s.push_str(", ");
            }
            w(&mut s, k);
        }
        s.push_str("] }");
    }
    s.push_str("\n  ]\n}\n");
    s
}

/// In a worker: the UI thread doesn't touch the disk.
pub fn save(list: &[Profile]) {
    let json = to_json(list);
    crate::jobs::spawn(move || {
        let Some(path) = file() else { return };
        let fs = LocalFs;
        let mut d = VPath::root();
        for c in path.parent().unwrap_or_else(VPath::root).components() {
            d = d.child(c);
            let _ = fs.mkdir(&d);
        }
        if let Ok(mut w) = fs.create(&path, &CreateOpts { overwrite: true, ..CreateOpts::default() })
            && w.write_all(json.as_bytes()).is_ok()
        {
            let _ = w.finish();
        }
    });
}

/// A connected source and a note for the user.
pub type Connected = Result<(Arc<dyn Source>, Option<String>), Failure>;

/// Why a connection didn't happen.
pub enum Failure {
    Error(vfs::Error),
    /// The user has to agree first (a server's key never seen, a certificate the system doesn't trust):
    /// `text` asks, and connecting again with `(key, value)` added to the answers means yes.
    #[cfg_attr(not(any(feature = "sftp", feature = "ftp")), allow(dead_code))]
    Confirm {
        title: String,
        text: String,
        button: String,
        key: String,
        value: String,
    },
}

impl From<vfs::Error> for Failure {
    fn from(e: vfs::Error) -> Failure {
        Failure::Error(e)
    }
}

/// Answers that are not the connection itself: whether to remember the password, a trusted fingerprint.
const NOT_KEPT: [&str; 2] = ["remember", "trust"];

pub struct Connector {
    /// `s3`, `s3-compatible`, `sftp`, `ftp`, `webdav`.
    pub id: &'static str,
    /// Short: in a profile's kind and an id.
    pub title: &'static str,
    /// In the Connect menu and the form's title: "Amazon S3", "New connection: Amazon S3".
    pub label: &'static str,
    pub intro: &'static str,
    pub params: fn() -> Vec<Param>,
    /// Grey text in an empty field: an example, or what empty means.
    pub hints: &'static [(&'static str, &'static str)],
    /// Fields that are a file on this machine: they get a "Choose…" button.
    pub files: &'static [&'static str],
    /// In a worker: the source, and a note for the user (a new SSH host remembered).
    pub connect: fn(&ActionArgs) -> Connected,
}

/// The Connect menu's order: indexes into `all()`.
pub fn menu_order() -> Vec<usize> {
    let all = all();
    let rank = |id: &str| ["sftp", "ftp", "s3", "s3-compatible", "webdav"].iter().position(|x| *x == id).unwrap_or(usize::MAX);
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_by_key(|&i| rank(all[i].id));
    order
}

pub fn all() -> Vec<Connector> {
    vec![
        #[cfg(feature = "s3")]
        Connector {
            id: "s3",
            title: "S3",
            label: "Amazon S3",
            intro: "An AWS account: its access key and the secret of it. Each bucket's region is found by itself.",
            params: s3::aws_params,
            hints: s3::AWS_HINTS,
            files: &[],
            connect: s3::connect_aws,
        },
        #[cfg(feature = "s3")]
        Connector {
            id: "s3-compatible",
            title: "S3",
            label: "S3-compatible storage",
            intro: "A storage that speaks the S3 protocol: MinIO, Cloudflare R2, Wasabi, Backblaze B2, Yandex Object Storage.",
            params: s3::compatible_params,
            hints: s3::COMPATIBLE_HINTS,
            files: &[],
            connect: s3::connect_compatible,
        },
        #[cfg(feature = "sftp")]
        Connector {
            id: "sftp",
            title: "SFTP",
            label: "SFTP",
            intro: "Files on a server over SSH.",
            params: sftp::params,
            hints: sftp::HINTS,
            files: &["key"],
            connect: sftp::connect,
        },
        #[cfg(feature = "ftp")]
        Connector {
            id: "ftp",
            title: "FTP",
            label: "FTP / FTPS",
            intro: "Files on an FTP server: a hosting, a NAS, a router. FTPS encrypts the password and the files.",
            params: ftp::params,
            hints: ftp::HINTS,
            files: &[],
            connect: ftp::connect,
        },
        #[cfg(feature = "webdav")]
        Connector {
            id: "webdav",
            title: "WebDAV",
            label: "WebDAV",
            intro: "Files on a WebDAV server: Nextcloud, ownCloud, a NAS, Apache.",
            params: webdav::params,
            hints: webdav::HINTS,
            files: &[],
            connect: webdav::connect,
        },
    ]
}

#[allow(dead_code)]
fn text(id: &str, title: &str, default: &str) -> Param {
    Param::Text { id: id.into(), title: title.into(), default: default.into() }
}

#[allow(dead_code)]
fn secret(id: &str, title: &str) -> Param {
    Param::Secret { id: id.into(), title: title.into() }
}

/// A value as typed, trimmed; `None` if empty.
#[allow(dead_code)]
fn get(args: &ActionArgs, k: &str) -> Option<String> {
    args.get(k).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

#[cfg(feature = "s3")]
mod s3 {
    use super::*;
    use vfs::Cancel;

    /// Amazon: as CloudBerry asks it — a name and the keys; the region of each bucket is asked of Amazon.
    pub fn aws_params() -> Vec<Param> {
        vec![text("name", "Display name", ""), text("access", "Access key ID", ""), secret("secret", "Secret access key")]
    }

    /// Another storage: its address too, and a region if it wants one.
    pub fn compatible_params() -> Vec<Param> {
        vec![
            text("name", "Display name", ""),
            text("endpoint", "Endpoint", ""),
            text("region", "Region", ""),
            text("access", "Access key ID", ""),
            secret("secret", "Secret access key"),
        ]
    }

    pub const AWS_HINTS: &[(&str, &str)] =
        &[("name", "e.g. Work AWS; empty — s3:aws"), ("access", "empty — ~/.aws/credentials (AWS_PROFILE, or default), or the AWS_* variables")];

    pub const COMPATIBLE_HINTS: &[(&str, &str)] = &[
        ("name", "e.g. MinIO at home; empty — s3:<host>"),
        ("endpoint", "required: http://localhost:9000, https://<account>.r2.cloudflarestorage.com"),
        ("region", "empty — us-east-1"),
        ("access", "empty — ~/.aws/credentials (AWS_PROFILE, or default), or the AWS_* variables"),
    ];

    /// A file under the home folder, through the local disk (empty if there is none).
    fn home_file(env: &str, rel: &str) -> String {
        use vfs::Source;
        let path = match (std::env::var_os(env), std::env::var_os("HOME")) {
            (Some(p), _) => vfs_local::vpath(std::path::Path::new(&p)),
            (None, Some(h)) => vfs_local::vpath(std::path::Path::new(&h)).join(rel),
            _ => return String::new(),
        };
        let mut s = String::new();
        if let Ok(mut r) = vfs_local::LocalFs::new().open_read(&path, 0) {
            let _ = std::io::Read::read_to_string(&mut r, &mut s);
        }
        s
    }

    /// Keys typed in the dialog; otherwise the AWS_* variables (no profile named); otherwise the profile of
    /// `~/.aws/credentials` (and its region from `~/.aws/config`), as the AWS tools find them.
    fn keys(args: &ActionArgs) -> Result<vfs_s3::Profile, Failure> {
        if let Some(access_key) = get(args, "access") {
            return Ok(vfs_s3::Profile { access_key, secret_key: get(args, "secret").unwrap_or_default(), ..Default::default() });
        }
        let named = get(args, "profile");
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if named.is_none()
            && let (Some(access_key), Some(secret_key)) = (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY"))
        {
            return Ok(vfs_s3::Profile { access_key, secret_key, session_token: var("AWS_SESSION_TOKEN"), region: var("AWS_REGION") });
        }
        let name = named.or_else(|| var("AWS_PROFILE")).unwrap_or_else(|| "default".into());
        let creds = home_file("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials");
        let conf = home_file("AWS_CONFIG_FILE", ".aws/config");
        vfs_s3::aws_profile(&creds, &conf, &name).ok_or_else(|| {
            Failure::Error(vfs::Error::Io {
                path: None,
                msg: t!("no keys: type them, or name a profile of ~/.aws/credentials (\"{}\" has none)", name),
            })
        })
    }

    pub fn connect_aws(args: &ActionArgs) -> Connected {
        connect(args, String::new())
    }

    pub fn connect_compatible(args: &ActionArgs) -> Connected {
        let Some(endpoint) = get(args, "endpoint") else {
            let msg = t!("no endpoint: type the storage's address, e.g. http://localhost:9000").into();
            return Err(Failure::Error(vfs::Error::Io { path: None, msg }));
        };
        connect(args, endpoint)
    }

    fn connect(args: &ActionArgs, endpoint: String) -> Connected {
        let k = keys(args)?;
        let cfg = vfs_s3::Config {
            name: get(args, "name").unwrap_or_default(),
            endpoint,
            region: get(args, "region").or(k.region).unwrap_or_default(),
            access_key: k.access_key,
            secret_key: k.secret_key,
            session_token: k.session_token,
        };
        let s = vfs_s3::S3Fs::new(cfg);
        // Listing the buckets proves the address and the keys.
        s.list(&vfs::VPath::root(), &Cancel::new())?;
        Ok((Arc::new(s), None))
    }
}

/// `~/.ssh/config`, read through the local disk like any file (empty if there is none).
#[cfg(feature = "sftp")]
pub fn ssh_config_text() -> String {
    use vfs::Source;
    // `UISON_SSH_CONFIG` puts it elsewhere (checks).
    let path = match (std::env::var_os("UISON_SSH_CONFIG"), std::env::var_os("HOME")) {
        (Some(p), _) => vfs_local::vpath(std::path::Path::new(&p)),
        (None, Some(home)) => vfs_local::vpath(std::path::Path::new(&home)).join(".ssh/config"),
        _ => return String::new(),
    };
    let mut s = String::new();
    if let Ok(mut r) = vfs_local::LocalFs::new().open_read(&path, 0) {
        let _ = std::io::Read::read_to_string(&mut r, &mut s);
    }
    s
}

#[cfg(feature = "sftp")]
mod sftp {
    use super::*;
    use std::path::PathBuf;

    pub fn params() -> Vec<Param> {
        vec![
            text("server", "Server", ""),
            secret("password", "Password"),
            text("key", "Key file", ""),
            secret("passphrase", "Key passphrase"),
            text("folder", "Folder", ""),
        ]
    }

    pub const HINTS: &[(&str, &str)] = &[
        ("server", "user@host:port, or a Host of ~/.ssh/config"),
        ("password", "empty — sign in with a key"),
        ("key", "empty — ssh-agent, then ~/.ssh/id_ed25519, id_ecdsa, id_rsa"),
        ("passphrase", "if the key has one"),
        ("folder", "empty — the home folder; /var/www/site, or www from the home"),
    ];

    pub fn connect(args: &ActionArgs) -> Connected {
        let bad = |m: String| Failure::Error(vfs::Error::Io { path: None, msg: m });
        let (user, name, port) = vfs_sftp::split_server(args.get("server").unwrap_or("")).map_err(bad)?;
        // A Host of ~/.ssh/config: its HostName, User, Port and IdentityFile, unless typed here.
        let conf = vfs_sftp::ssh_config(&ssh_config_text(), &name);
        let host = conf.host_name.clone().unwrap_or(name);
        let user = user.or(conf.user.clone()).unwrap_or_else(|| std::env::var("USER").unwrap_or_default());
        if user.is_empty() {
            return Err(bad(t!("no user: type user@host").into()));
        }
        let port = port.or(conf.port).unwrap_or(22);
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        // A password is taken as typed: spaces may be part of it.
        let typed = |k: &str| args.get(k).filter(|v| !v.is_empty()).map(str::to_string);
        let cfg = vfs_sftp::Config {
            host,
            port,
            user,
            password: typed("password"),
            key: get(args, "key").or(conf.identity_file).map(|k| match k.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(k),
            }),
            passphrase: typed("passphrase"),
            // Ours besides ~/.ssh/known_hosts; `UISON_KNOWN_HOSTS` puts it elsewhere (as in uison-ssh-player).
            known_hosts: std::env::var_os("UISON_KNOWN_HOSTS").map_or_else(|| home.join(".config/uison-commander/known_hosts"), PathBuf::from),
            // A key never seen is asked about; yes is this fingerprint and no other.
            trust: get(args, "trust").map_or(vfs_sftp::Trust::Ask, vfs_sftp::Trust::Fingerprint),
            folder: get(args, "folder").unwrap_or_default(),
        };
        let host = cfg.host.clone();
        match vfs_sftp::SftpFs::connect(cfg) {
            Ok((s, note)) => Ok((Arc::new(s), note)),
            Err(e) => match vfs_sftp::unknown_host(&e) {
                Some(fp) => Err(Failure::Confirm {
                    title: t!("A server never seen before").into(),
                    text: t!(
                        "This is the first connection to {}. Its key fingerprint is\n\n{}\n\nIf you expected this server (its administrator can confirm the fingerprint), trust it: it will be checked on every connection from now on.",
                        host,
                        fp
                    ),
                    button: t!("Trust and connect").into(),
                    key: "trust".into(),
                    value: fp,
                }),
                None => Err(Failure::Error(e)),
            },
        }
    }
}

#[cfg(feature = "webdav")]
mod webdav {
    use super::*;

    pub fn params() -> Vec<Param> {
        vec![text("url", "Address", ""), text("user", "User", ""), secret("password", "Password")]
    }

    pub const HINTS: &[(&str, &str)] = &[
        ("url", "https://cloud.example.com/remote.php/dav/files/me/"),
        ("user", "empty — no sign-in"),
        ("password", "the password, or an app password of Nextcloud"),
    ];

    pub fn connect(args: &ActionArgs) -> Connected {
        // A password is taken as typed: spaces may be part of it.
        let cfg = vfs_webdav::Config {
            url: get(args, "url").unwrap_or_default(),
            user: get(args, "user").unwrap_or_default(),
            password: args.get("password").unwrap_or("").to_string(),
        };
        Ok((Arc::new(vfs_webdav::DavFs::connect(cfg)?), None))
    }
}

#[cfg(feature = "ftp")]
mod ftp {
    use super::*;
    use vfs_ftp::Security;

    pub fn params() -> Vec<Param> {
        vec![
            text("server", "Server", ""),
            text("user", "User", ""),
            secret("password", "Password"),
            Param::Choice {
                id: "security".into(),
                title: "Encryption".into(),
                options: vec![
                    ("explicit".into(), "FTPS: TLS after connecting (port 21, usual)".into()),
                    ("implicit".into(), "FTPS: TLS from the start (port 990)".into()),
                    ("none".into(), "None: plain FTP, the password goes in the clear".into()),
                ],
                default: "explicit".into(),
            },
        ]
    }

    pub const HINTS: &[(&str, &str)] = &[("server", "host or host:port"), ("user", "empty — anonymous")];

    /// Certificates the user trusted: `host:port SHA256:…` a line. `UISON_KNOWN_CERTS` puts it elsewhere.
    fn known_file() -> Option<VPath> {
        match std::env::var_os("UISON_KNOWN_CERTS") {
            Some(p) => Some(vfs_local::vpath(std::path::Path::new(&p))),
            None => Some(crate::state::config_dir()?.child("known_certs")),
        }
    }

    fn known_text() -> String {
        let mut s = String::new();
        if let Some(path) = known_file()
            && let Ok(mut r) = LocalFs.open_read(&path, 0)
        {
            let _ = r.read_to_string(&mut s);
        }
        s
    }

    /// The fingerprints trusted for `place` (`host:port`).
    fn known(place: &str) -> Vec<String> {
        known_text().lines().filter_map(|l| l.split_once(' ')).filter(|(p, _)| *p == place).map(|(_, f)| f.trim().to_string()).collect()
    }

    /// Adds `fp` for `place`, in place of what was trusted for it before.
    fn remember(place: &str, fp: &str) -> vfs::Result<()> {
        let path = known_file().ok_or(vfs::Error::Unsupported)?;
        let mut text: String = known_text().lines().filter(|l| !l.starts_with(&format!("{place} "))).map(|l| format!("{l}\n")).collect();
        text.push_str(&format!("{place} {fp}\n"));
        let mut d = VPath::root();
        for c in path.parent().unwrap_or_else(VPath::root).components() {
            d = d.child(c);
            let _ = LocalFs.mkdir(&d);
        }
        let mut w = LocalFs.create(&path, &CreateOpts { overwrite: true, ..CreateOpts::default() })?;
        w.write_all(text.as_bytes()).map_err(|e| vfs::Error::io(&path, &e))?;
        w.finish()
    }

    pub fn connect(args: &ActionArgs) -> Connected {
        let bad = |m: String| Failure::Error(vfs::Error::Io { path: None, msg: m });
        let (user, host, port) = vfs_ftp::split_server(args.get("server").unwrap_or("")).map_err(bad)?;
        let security = match args.get("security") {
            Some("implicit") => Security::Implicit,
            Some("none") => Security::None,
            _ => Security::Explicit,
        };
        // A password is taken as typed: spaces may be part of it.
        let mut cfg = vfs_ftp::Config {
            host,
            port: port.unwrap_or(0),
            user: get(args, "user").or(user).unwrap_or_default(),
            password: args.get("password").unwrap_or("").to_string(),
            security,
            trusted: Vec::new(),
        };
        let place = format!("{}:{}", cfg.host, cfg.port());
        let before = known(&place);
        cfg.trusted = before.clone();
        // A yes in the dialog: this certificate and no other.
        let yes = get(args, "trust");
        cfg.trusted.extend(yes.clone());
        match vfs_ftp::FtpFs::connect(cfg) {
            Ok(s) => {
                let note = yes.filter(|fp| !before.contains(fp)).map(|fp| match remember(&place, &fp) {
                    Ok(()) => t!("First connection to {}: its certificate ({}) is saved and checked next time.", place, fp),
                    Err(e) => {
                        t!("Connected to {}, but its certificate could not be saved ({}): it will be asked again.", place, crate::i18n::err(&e))
                    }
                });
                Ok((Arc::new(s), note))
            }
            Err(e) => match vfs_ftp::untrusted_certificate(&e) {
                Some((fp, why)) => {
                    let why = crate::i18n::error(&why);
                    let (title, text) = if before.is_empty() {
                        (
                            t!("A certificate this Mac does not trust").to_string(),
                            t!(
                                "The certificate of {} is not trusted: {}. Its fingerprint is\n\n{}\n\nIf you expected this server (its administrator can confirm the fingerprint), trust it: it will be checked on every connection from now on.",
                                place,
                                why,
                                fp
                            ),
                        )
                    } else {
                        (
                            t!("The server's certificate has changed").to_string(),
                            t!(
                                "The certificate of {} is not the one trusted before. Its fingerprint now is\n\n{}\n\nA new certificate of the server, or someone in between. Trust it only if its administrator confirms the fingerprint.",
                                place,
                                fp
                            ),
                        )
                    };
                    Err(Failure::Confirm { title, text, button: t!("Trust and connect").into(), key: "trust".into(), value: fp })
                }
                None => Err(Failure::Error(e)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_round_trip_without_secrets() {
        let p = Profile {
            kind: "sftp".into(),
            id: "sftp://u@h:2222".into(),
            title: "sftp:u@h:2222".into(),
            args: vec![("server".into(), "u@h:2222".into()), ("key".into(), "~/.ssh/my \"key\"".into())],
            secrets: vec!["password".into()],
        };
        let back = parse(&to_json(std::slice::from_ref(&p)));
        assert_eq!(back, [p]);
        assert!(parse("").is_empty() && parse("{}").is_empty());
    }

    /// S3 with the keys left empty: they come from a profile of the credentials file.
    #[cfg(feature = "s3")]
    #[test]
    fn s3_keys_from_a_profile() {
        let (Ok(endpoint), Ok(key), Ok(secret)) =
            (std::env::var("UISON_S3_ENDPOINT"), std::env::var("UISON_S3_KEY"), std::env::var("UISON_S3_SECRET"))
        else {
            return vfs::conformance::skip("UISON_S3_ENDPOINT");
        };
        use vfs::Source;
        let local = LocalFs::new();
        let path = vfs_local::vpath(&std::env::temp_dir()).child(&format!("uc-aws-credentials-{}", std::process::id()));
        let mut w = local.create(&path, &CreateOpts { overwrite: true, ..CreateOpts::default() }).unwrap();
        w.write_all(format!("[ucprofile]\naws_access_key_id = {key}\naws_secret_access_key = {secret}\n").as_bytes()).unwrap();
        w.finish().unwrap();
        // Only this test sets it (the others type their keys).
        unsafe { std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", crate::os_path(&path)) };
        let c = all().into_iter().find(|c| c.id == "s3-compatible").unwrap();
        let args = ActionArgs(vec![("endpoint".into(), endpoint), ("profile".into(), "ucprofile".into())]);
        let res = (c.connect)(&args);
        let missing = (c.connect)(&ActionArgs(vec![("endpoint".into(), "http://localhost:1".into()), ("profile".into(), "nope".into())]));
        let _ = local.remove(&path);
        assert!(res.is_ok(), "connected with the profile's keys");
        assert!(matches!(missing, Err(Failure::Error(e)) if e.to_string().contains("\"nope\" has none")));
    }

    /// The compatible storages need their address; one forgotten is said before anything is asked.
    #[cfg(feature = "s3")]
    #[test]
    fn s3_compatible_needs_an_endpoint() {
        let c = all().into_iter().find(|c| c.id == "s3-compatible").unwrap();
        let res = (c.connect)(&ActionArgs(vec![("endpoint".into(), "  ".into()), ("access".into(), "k".into())]));
        assert!(matches!(res, Err(Failure::Error(e)) if e.to_string().contains("no endpoint")));
        let aws = all().into_iter().find(|c| c.id == "s3").unwrap();
        assert!(!(aws.params)().iter().any(|p| p.id() == "endpoint"), "Amazon asks no address");
    }

    /// An S3 profile with an endpoint, from before the two connectors, opens as a compatible storage.
    #[test]
    fn old_s3_profiles_with_an_endpoint_are_compatible() {
        let json = r#"{ "connections": [
            { "kind": "s3", "id": "s3://localhost:9000#1", "title": "s3:localhost:9000", "args": { "endpoint": "http://localhost:9000" }, "secrets": [] },
            { "kind": "s3", "id": "s3://aws#2", "title": "s3:aws", "args": { "endpoint": "", "access": "AK" }, "secrets": ["secret"] }
        ] }"#;
        let kinds: Vec<String> = parse(json).into_iter().map(|p| p.kind).collect();
        assert_eq!(kinds, ["s3-compatible", "s3"]);
    }
}
