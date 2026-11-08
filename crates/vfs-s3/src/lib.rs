//! Amazon S3 and compatible storages (MinIO, Cloudflare R2) as a source.
//!
//! Paths: `/` lists the buckets, `/bucket/a/b` is the key `a/b` in `bucket`. Folders are prefixes up to a
//! `/`: virtual, so an empty folder exists only while its marker `a/` (an empty object, made by `mkdir`)
//! is there. There is no rename: `ops` copies and deletes.
//!
//! The SDK is async; the contract is not (it is called from worker threads only). Every call runs on
//! one shared Tokio runtime with `block_on`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use aws_sdk_s3::Client;
use aws_sdk_s3::config::SharedHttpClient;
use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region, RequestChecksumCalculation};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLocationConstraint, CompletedMultipartUpload, CompletedPart, CreateBucketConfiguration, Delete, EncodingType, ObjectCannedAcl,
    ObjectIdentifier,
};
use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
use bytes::Bytes;
use vfs::{
    ActionArgs, ActionInfo, ActionResult, Cancel, Capabilities, CreateOpts, Entry, Error, Kind, Param, Result, Root, Source, SourceInfo, VPath,
};

/// Multipart upload part: S3 wants at least 5 MB (but the last).
const PART: usize = 8 << 20;
/// S3 takes at most 10 000 parts; a file larger than that many `PART`s gets larger parts (with room left
/// for a file that grew a little since its size was taken).
const MAX_PARTS: u64 = 9_000;
/// Parts sent at once.
const PARTS_IN_FLIGHT: usize = 4;
/// CopyObject copies up to 5 GB in one call; larger objects are streamed by `ops`.
const COPY_MAX: i64 = 5 << 30;

/// Objects larger than this are copied in parts (UploadPartCopy); `UISON_S3_COPY_MAX` lowers it for checks.
fn copy_max() -> i64 {
    std::env::var("UISON_S3_COPY_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(COPY_MAX)
}

/// One part of a copy in parts: large (fewer requests), well under S3's 5 GB a part.
fn copy_part(size: i64) -> i64 {
    let min = 5i64 << 20;
    // At most 9 000 parts; whole megabytes; 512 MB unless the object is small or huge.
    let need = (size + 8999) / 9000;
    need.max((512i64 << 20).min(size / 2)).max(min)
}

/// What the user typed to connect.
#[derive(Clone, Default)]
pub struct Config {
    /// What the user calls it, shown instead of `s3:host`; may be empty.
    pub name: String,
    /// Empty — Amazon S3; otherwise `http(s)://host:port` of a compatible storage.
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// Temporary keys (STS, SSO) come with a session token.
    pub session_token: Option<String>,
}

/// Without the secret: a `{:?}` somewhere must not print it.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

/// Keys and region of an AWS profile, as `~/.aws/credentials` and `~/.aws/config` (their texts) give them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
}

/// The profile `name` from the two files (`[name]` in credentials, `[profile name]` or `[name]` in config);
/// `None` if it has no keys.
pub fn aws_profile(credentials: &str, config: &str, name: &str) -> Option<Profile> {
    fn section(text: &str, wanted: &[String]) -> Vec<(String, String)> {
        let mut on = false;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
                continue;
            }
            if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                on = wanted.iter().any(|w| w == h.trim());
            } else if on && let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        out
    }
    let get = |kv: &[(String, String)], k: &str| kv.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let cred = section(credentials, &[name.to_string()]);
    let conf = section(config, &[format!("profile {name}"), name.to_string()]);
    let pick = |k: &str| get(&cred, k).or_else(|| get(&conf, k));
    Some(Profile {
        access_key: pick("aws_access_key_id")?,
        secret_key: pick("aws_secret_access_key")?,
        session_token: pick("aws_session_token"),
        region: get(&conf, "region").or_else(|| get(&cred, "region")),
    })
}

/// FNV-1a, 32 bits: a short stable tag for a key id.
fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

/// The part size for an object of about `size` bytes.
fn part_size(size: Option<u64>) -> usize {
    let need = size.unwrap_or(0).div_ceil(MAX_PARTS);
    // Whole megabytes, at least `PART`.
    (need.div_ceil(1 << 20) << 20).max(PART as u64) as usize
}

/// The Content-Type of a name, so a public or signed link to it opens in the browser, not downloads.
fn content_type(key: &str) -> &'static str {
    let ext = key.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "log" | "md" | "csv" => "text/plain; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// A key as a listing with `encoding-type=url` gives it: `%XX` and `+` for a space.
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Config {
    /// `localhost:9000`, `aws`.
    fn host(&self) -> String {
        let e = self.endpoint.trim();
        if e.is_empty() {
            return "aws".into();
        }
        e.split_once("://").map_or(e, |(_, h)| h).trim_end_matches('/').to_string()
    }
}

pub struct S3Fs {
    cfg: Config,
    base: Client,
    /// Amazon: each bucket answers only in its own region.
    regional: Mutex<HashMap<String, Client>>,
    /// Clients without keys (what anyone on the web gets), by region.
    anonymous: Mutex<HashMap<String, Client>>,
}

use vfs_rt::{rt, run};

/// One HTTPS client (and its connection pool) for every S3 client: rustls on ring.
fn http() -> SharedHttpClient {
    static H: OnceLock<SharedHttpClient> = OnceLock::new();
    H.get_or_init(|| {
        let _rt = rt().enter();
        aws_smithy_http_client::Builder::new().tls_provider(Provider::Rustls(CryptoMode::Ring)).build_https()
    })
    .clone()
}

/// Connecting this long, or waiting this long for an answer to start: the storage is taken for gone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// An object's data that stops coming for this long (a stalled download) fails the read.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A client with the keys, or (`anonymous`) one that signs nothing: what anyone on the web would get.
fn client_as(cfg: &Config, region: &str, anonymous: bool) -> Client {
    let _rt = rt().enter();
    let timeouts = TimeoutConfig::builder().connect_timeout(CONNECT_TIMEOUT).read_timeout(READ_TIMEOUT).build();
    let mut b = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .http_client(http())
        .timeout_config(timeouts);
    b = if anonymous {
        b.allow_no_auth()
    } else {
        b.credentials_provider(Credentials::new(cfg.access_key.trim(), cfg.secret_key.trim(), cfg.session_token.clone(), None, "uison-commander"))
    };
    if !cfg.endpoint.trim().is_empty() {
        // Compatible storages: bucket in the path, checksums only where S3 requires them (R2, older MinIO).
        b = b
            .endpoint_url(cfg.endpoint.trim().trim_end_matches('/'))
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired);
    }
    Client::from_conf(b.build())
}

fn client(cfg: &Config, region: &str) -> Client {
    client_as(cfg, region, false)
}

/// The group of everyone in an ACL grant.
const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
/// Objects whose visibility is read before the Visibility dialog: enough to tell, quick to ask.
const VISIBILITY_PROBE: usize = 50;

/// SDK error → contract error.
fn err<E: ProvideErrorMetadata + std::error::Error + 'static>(p: &VPath, e: SdkError<E, HttpResponse>) -> Error {
    let status = e.raw_response().map(|r| r.status().as_u16());
    match (e.code(), status) {
        (Some("NoSuchKey" | "NoSuchBucket" | "NotFound" | "NoSuchUpload"), _) | (_, Some(404)) => Error::NotFound(p.clone()),
        (Some("AccessDenied" | "AllAccessDisabled" | "Forbidden"), _) | (_, Some(403)) => Error::PermissionDenied(p.clone()),
        (Some("BucketAlreadyOwnedByYou" | "BucketAlreadyExists"), _) => Error::AlreadyExists(p.clone()),
        (Some("BucketNotEmpty"), _) => Error::DirNotEmpty(p.clone()),
        (Some("AccessControlListNotSupported"), _) => {
            Error::msg(p, "the bucket has ACLs turned off (Object Ownership: bucket owner enforced); who can read is set by its bucket policy")
        }
        (Some(c), _) => Error::msg(p, format!("{c}: {}", e.message().unwrap_or(""))),
        // No answer from the storage: connection, TLS, timeout.
        (None, _) => Error::msg(p, DisplayErrorContext(&e).to_string()),
    }
}

/// `/bucket/a/b` → (`bucket`, `a/b`); `/` → None.
fn split(p: &VPath) -> Option<(&str, &str)> {
    let s = p.as_str().trim_start_matches('/');
    if s.is_empty() {
        return None;
    }
    Some(s.split_once('/').unwrap_or((s, "")))
}

/// Percent-encoding of a key for URLs and `x-amz-copy-source`: `/` stays.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn time(t: Option<&aws_sdk_s3::primitives::DateTime>) -> Option<SystemTime> {
    t.and_then(|t| SystemTime::try_from(*t).ok())
}

fn dir_entry(name: &str) -> Entry {
    let mut e = Entry::new(name, Kind::Dir);
    e.hidden = name.starts_with('.');
    e
}

fn file_entry(name: &str, size: Option<i64>, modified: Option<SystemTime>, class: Option<&str>, etag: Option<&str>) -> Entry {
    let mut e = Entry::new(name, Kind::File);
    e.size = size.map(|s| s.max(0) as u64);
    e.modified = modified;
    e.hidden = name.starts_with('.');
    if let Some(c) = class {
        e.extra.push(("Storage class".into(), c.into()));
    }
    if let Some(t) = etag {
        e.extra.push(("ETag".into(), t.trim_matches('"').into()));
    }
    e
}

impl S3Fs {
    /// A source for these keys. Nothing is asked yet: call `roots` to check the connection.
    pub fn new(cfg: Config) -> S3Fs {
        let region = if cfg.region.trim().is_empty() { "us-east-1".to_string() } else { cfg.region.trim().to_string() };
        let cfg = Config { region, ..cfg };
        let base = client(&cfg, &cfg.region);
        S3Fs { cfg, base, regional: Mutex::new(HashMap::new()), anonymous: Mutex::new(HashMap::new()) }
    }

    fn aws(&self) -> bool {
        self.cfg.endpoint.trim().is_empty()
    }

    /// The client of a bucket's region (asked once per bucket on Amazon).
    fn client(&self, bucket: &str) -> Client {
        if !self.aws() {
            return self.base.clone();
        }
        if let Some(c) = self.regional.lock().unwrap().get(bucket) {
            return c.clone();
        }
        // GetBucketLocation needs its own permission, which policies often leave out; HeadBucket tells the
        // region too (`x-amz-bucket-region`), even in the error of the wrong region. Whatever comes is kept:
        // a failure asked again on every call would cost a request each time.
        let region = match run(self.base.get_bucket_location().bucket(bucket).send()) {
            Ok(out) => Some(match out.location_constraint() {
                None => "us-east-1".to_string(),
                Some(BucketLocationConstraint::Eu) => "eu-west-1".to_string(),
                Some(c) if c.as_str().is_empty() => "us-east-1".to_string(),
                Some(c) => c.as_str().to_string(),
            }),
            Err(_) => match run(self.base.head_bucket().bucket(bucket).send()) {
                Ok(out) => out.bucket_region().map(str::to_string),
                Err(e) => e.raw_response().and_then(|r| r.headers().get("x-amz-bucket-region")).map(str::to_string),
            },
        };
        let region = region.unwrap_or_else(|| self.cfg.region.clone());
        let c = if region == self.cfg.region { self.base.clone() } else { client(&self.cfg, &region) };
        self.regional.lock().unwrap().insert(bucket.to_string(), c.clone());
        c
    }

    /// Is there anything under `key/` (besides the folder's own marker, if `content`)?
    fn has_prefix(&self, p: &VPath, bucket: &str, key: &str, content: bool) -> Result<bool> {
        let prefix = format!("{key}/");
        let out = run(self.client(bucket).list_objects_v2().bucket(bucket).prefix(&prefix).max_keys(2).send()).map_err(|e| err(p, e))?;
        Ok(out.contents().iter().any(|o| !content || o.key() != Some(prefix.as_str())) || !out.common_prefixes().is_empty())
    }

    fn head(&self, p: &VPath, bucket: &str, key: &str) -> Result<Option<aws_sdk_s3::operation::head_object::HeadObjectOutput>> {
        match run(self.client(bucket).head_object().bucket(bucket).key(key).send()) {
            Ok(o) => Ok(Some(o)),
            Err(e) => match err(p, e) {
                Error::NotFound(_) => Ok(None),
                e => Err(e),
            },
        }
    }

    fn file_key<'a>(&self, p: &'a VPath) -> Result<(&'a str, &'a str)> {
        match split(p) {
            Some((b, k)) if !k.is_empty() => Ok((b, k)),
            _ => Err(Error::IsADirectory(p.clone())),
        }
    }

    /// Who can read an object now: (anyone on the web, by its ACL). Anyone is asked for real — a request
    /// without keys — so a bucket policy counts too.
    fn visibility(&self, p: &VPath) -> Result<(bool, bool)> {
        let (bucket, key) = self.file_key(p)?;
        let c = self.client(bucket);
        let region = c.config().region().map(|r| r.to_string()).unwrap_or_else(|| self.cfg.region.clone());
        let anon = self.anonymous.lock().unwrap().entry(region.clone()).or_insert_with(|| client_as(&self.cfg, &region, true)).clone();
        let anyone = run(anon.head_object().bucket(bucket).key(key).send()).is_ok();
        let acl = run(c.get_object_acl().bucket(bucket).key(key).send()).is_ok_and(|out| {
            out.grants().iter().any(|g| {
                let everyone = g.grantee().and_then(|e| e.uri()) == Some(ALL_USERS);
                everyone && matches!(g.permission().map(|p| p.as_str()), Some("READ" | "FULL_CONTROL"))
            })
        });
        Ok((anyone, acl))
    }

    /// The web address of an object: public (works if the object is public) or signed for a while.
    fn link(&self, p: &VPath, how: &str) -> Result<String> {
        let (bucket, key) = self.file_key(p)?;
        let c = self.client(bucket);
        let expires = match how {
            "1h" => Duration::from_secs(3600),
            "24h" => Duration::from_secs(86_400),
            "7d" => Duration::from_secs(7 * 86_400),
            _ => {
                let region = c.config().region().map(|r| r.to_string()).unwrap_or_else(|| self.cfg.region.clone());
                return Ok(if self.aws() && bucket.contains('.') {
                    // A dot in the name breaks the certificate of the bucket's own host name: the path form.
                    format!("https://s3.{region}.amazonaws.com/{bucket}/{}", enc(key))
                } else if self.aws() {
                    format!("https://{bucket}.s3.{region}.amazonaws.com/{}", enc(key))
                } else {
                    format!("{}/{bucket}/{}", self.cfg.endpoint.trim().trim_end_matches('/'), enc(key))
                });
            }
        };
        let pc = PresigningConfig::expires_in(expires).map_err(|e| Error::msg(p, e.to_string()))?;
        let req = run(c.get_object().bucket(bucket).key(key).presigned(pc)).map_err(|e| err(p, e))?;
        Ok(req.uri().to_string())
    }
}

impl Source for S3Fs {
    fn info(&self) -> SourceInfo {
        let host = self.cfg.host();
        SourceInfo {
            // The keys tell two accounts on one host apart; a hash of the key id, not the id itself, goes into
            // the state file.
            id: format!("s3://{host}#{:08x}", fnv(self.cfg.access_key.trim())),
            title: if self.cfg.name.trim().is_empty() { format!("s3:{host}") } else { self.cfg.name.trim().to_string() },
            caps: Capabilities {
                write: true,
                rename: false,
                server_copy: true,
                real_dirs: false,
                seek: true,
                permissions: false,
                set_mtime: false,
                case_sensitive: true,
            },
        }
    }

    /// One root: the buckets are its folders. Each bucket as a root of its own made the list of sources
    /// as long as the account's buckets (and asked the network for it every time).
    fn roots(&self, _cancel: &Cancel) -> Result<Vec<Root>> {
        Ok(vec![Root { path: VPath::root(), title: "All buckets".into() }])
    }

    fn home(&self) -> VPath {
        VPath::root()
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        cancel.check()?;
        let Some((bucket, key)) = split(dir) else {
            let out = run(self.base.list_buckets().send()).map_err(|e| err(dir, e))?;
            return Ok(out
                .buckets()
                .iter()
                .filter_map(|b| {
                    let mut e = dir_entry(b.name()?);
                    e.modified = time(b.creation_date());
                    Some(e)
                })
                .collect());
        };
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let c = self.client(bucket);
        let (mut entries, mut marker, mut token) = (Vec::new(), false, None);
        loop {
            cancel.check()?;
            // Keys come URL-encoded: a control character in one key would otherwise break the XML of the whole page.
            let req = c.list_objects_v2().bucket(bucket).prefix(&prefix).delimiter("/").encoding_type(EncodingType::Url);
            // Esc on a slow listing drops the request in flight.
            let out = vfs_rt::run_cancel(req.set_continuation_token(token).send(), cancel)?.map_err(|e| err(dir, e))?;
            for p in out.common_prefixes() {
                let full = url_decode(p.prefix().unwrap_or(""));
                let name = full.strip_prefix(prefix.as_str()).unwrap_or("").trim_end_matches('/');
                // `a//b`, `./`, `../`: a prefix no path can name — left out rather than shown wrong.
                if vfs::valid_name(name) {
                    entries.push(dir_entry(name));
                }
            }
            for o in out.contents() {
                let k = url_decode(o.key().unwrap_or(""));
                if k == prefix {
                    marker = true;
                    continue;
                }
                let name = k.strip_prefix(prefix.as_str()).unwrap_or(&k);
                if vfs::valid_name(name) {
                    let class = o.storage_class().map(|s| s.as_str());
                    entries.push(file_entry(name, o.size(), time(o.last_modified()), class, o.e_tag()));
                }
            }
            token = out.next_continuation_token().map(str::to_string);
            if !out.is_truncated().unwrap_or(false) || token.is_none() {
                break;
            }
        }
        // Nothing and no marker: a file, or no such folder.
        if entries.is_empty() && !marker && !key.is_empty() {
            return Err(match self.head(dir, bucket, key)? {
                Some(_) => Error::NotADirectory(dir.clone()),
                None => Error::NotFound(dir.clone()),
            });
        }
        Ok(entries)
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        let Some((bucket, key)) = split(path) else { return Ok(dir_entry("")) };
        if key.is_empty() {
            run(self.client(bucket).head_bucket().bucket(bucket).send()).map_err(|e| err(path, e))?;
            return Ok(dir_entry(bucket));
        }
        if let Some(o) = self.head(path, bucket, key)? {
            let class = o.storage_class().map(|s| s.as_str());
            return Ok(file_entry(path.name(), o.content_length(), time(o.last_modified()), class, o.e_tag()));
        }
        if self.has_prefix(path, bucket, key, false)? { Ok(dir_entry(path.name())) } else { Err(Error::NotFound(path.clone())) }
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let (bucket, key) = self.file_key(path)?;
        let mut req = self.client(bucket).get_object().bucket(bucket).key(key);
        if offset > 0 {
            req = req.range(format!("bytes={offset}-"));
        }
        match run(req.send()) {
            Ok(out) => Ok(Box::new(Body { body: out.body, buf: Bytes::new() })),
            // At or past the end: nothing to read.
            Err(e) if offset > 0 && e.code() == Some("InvalidRange") => Ok(Box::new(io::empty())),
            Err(e) => Err(err(path, e)),
        }
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn vfs::Writer>> {
        let (bucket, key) = match split(path) {
            Some((b, k)) if !k.is_empty() => (b, k),
            _ => return Err(Error::msg(path, "files live inside a bucket")),
        };
        if !opts.overwrite && self.head(path, bucket, key)?.is_some() {
            return Err(Error::AlreadyExists(path.clone()));
        }
        Ok(Box::new(Upload {
            client: self.client(bucket),
            path: path.clone(),
            bucket: bucket.into(),
            key: key.into(),
            part: part_size(opts.size_hint),
            buf: Vec::new(),
            multi: None,
            sending: std::collections::VecDeque::new(),
            done: false,
        }))
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        let Some((bucket, key)) = split(path) else { return Err(Error::AlreadyExists(path.clone())) };
        if key.is_empty() {
            let mut req = self.base.create_bucket().bucket(bucket);
            if self.aws() && self.cfg.region != "us-east-1" {
                let loc = BucketLocationConstraint::from(self.cfg.region.as_str());
                req = req.create_bucket_configuration(CreateBucketConfiguration::builder().location_constraint(loc).build());
            }
            return run(req.send()).map(|_| ()).map_err(|e| err(path, e));
        }
        if self.stat(path).is_ok() {
            return Err(Error::AlreadyExists(path.clone()));
        }
        let parent = path.parent().unwrap_or_else(VPath::root);
        match self.stat(&parent) {
            Ok(e) if e.is_dir() => {}
            Ok(_) => return Err(Error::NotADirectory(parent)),
            Err(e) => return Err(e),
        }
        let req = self.client(bucket).put_object().bucket(bucket).key(format!("{key}/")).body(ByteStream::from_static(b""));
        run(req.send()).map(|_| ()).map_err(|e| err(path, e))
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        let Some((bucket, key)) = split(path) else { return Err(Error::PermissionDenied(path.clone())) };
        let c = self.client(bucket);
        if key.is_empty() {
            run(c.delete_bucket().bucket(bucket).send()).map_err(|e| err(path, e))?;
            self.regional.lock().unwrap().remove(bucket);
            return Ok(());
        }
        if self.head(path, bucket, key)?.is_some() {
            return run(c.delete_object().bucket(bucket).key(key).send()).map(|_| ()).map_err(|e| err(path, e));
        }
        // A folder: empty but for its marker (a virtual folder without one is gone with its last file).
        if self.has_prefix(path, bucket, key, true)? {
            return Err(Error::DirNotEmpty(path.clone()));
        }
        run(c.delete_object().bucket(bucket).key(format!("{key}/")).send()).map(|_| ()).map_err(|e| err(path, e))
    }

    /// DeleteObjects: up to 1 000 keys a request (instead of a HEAD and a DELETE each). A key that is not
    /// there counts as deleted, as S3 says.
    fn remove_files(&self, paths: &[VPath], cancel: &Cancel) -> Vec<Result<()>> {
        let mut out: Vec<Result<()>> = (0..paths.len()).map(|_| Ok(())).collect();
        // By bucket, in the order they come.
        let mut buckets: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            match self.file_key(p) {
                Ok((b, _)) => match buckets.iter_mut().find(|(x, _)| *x == b) {
                    Some((_, v)) => v.push(i),
                    None => buckets.push((b, vec![i])),
                },
                Err(e) => out[i] = Err(e),
            }
        }
        for (bucket, idx) in buckets {
            let c = self.client(bucket);
            for chunk in idx.chunks(1000) {
                if let Err(e) = cancel.check() {
                    chunk.iter().for_each(|&i| out[i] = Err(e.clone()));
                    continue;
                }
                let objects: Vec<ObjectIdentifier> =
                    chunk.iter().filter_map(|&i| ObjectIdentifier::builder().key(self.file_key(&paths[i]).ok()?.1).build().ok()).collect();
                let delete = match Delete::builder().set_objects(Some(objects)).quiet(true).build() {
                    Ok(d) => d,
                    Err(e) => {
                        let e = Error::msg(&paths[chunk[0]], e.to_string());
                        chunk.iter().for_each(|&i| out[i] = Err(e.clone()));
                        continue;
                    }
                };
                match vfs_rt::run_cancel(c.delete_objects().bucket(bucket).delete(delete).send(), cancel) {
                    Err(e) => chunk.iter().for_each(|&i| out[i] = Err(e.clone())),
                    Ok(Err(e)) => {
                        let e = err(&paths[chunk[0]], e);
                        chunk.iter().for_each(|&i| out[i] = Err(e.clone()));
                    }
                    // Quiet: only what failed is listed.
                    Ok(Ok(res)) => {
                        for fe in res.errors() {
                            let Some(&i) = chunk.iter().find(|&&i| self.file_key(&paths[i]).is_ok_and(|(_, k)| Some(k) == fe.key())) else {
                                continue;
                            };
                            out[i] = Err(match fe.code() {
                                Some("AccessDenied") => Error::PermissionDenied(paths[i].clone()),
                                c => Error::msg(&paths[i], format!("{}: {}", c.unwrap_or("error"), fe.message().unwrap_or(""))),
                            });
                        }
                    }
                }
            }
        }
        out
    }

    fn copy_within(&self, from: &VPath, to: &VPath, cancel: &Cancel) -> Result<()> {
        cancel.check()?;
        let (Ok((fb, fk)), Ok((tb, tk))) = (self.file_key(from), self.file_key(to)) else { return Err(Error::Unsupported) };
        let size = match self.head(from, fb, fk)? {
            Some(o) => o.content_length().unwrap_or(0),
            // A folder (a prefix) is no object to copy: ops walks it.
            None if self.stat(from).is_ok_and(|e| e.is_dir()) => return Err(Error::Unsupported),
            None => return Err(Error::NotFound(from.clone())),
        };
        let source = format!("{fb}/{}", enc(fk));
        let c = self.client(tb);
        if size <= copy_max() {
            let req = c.copy_object().bucket(tb).key(tk).copy_source(&source).content_type(content_type(tk));
            return vfs_rt::run_cancel(req.send(), cancel)?.map(|_| ()).map_err(|e| err(to, e));
        }
        // Larger than one CopyObject takes: a multipart upload whose parts are ranges of the source, copied by
        // the storage itself — nothing goes through this machine.
        let made = vfs_rt::run_cancel(c.create_multipart_upload().bucket(tb).key(tk).content_type(content_type(tk)).send(), cancel)?
            .map_err(|e| err(to, e))?;
        let id = made.upload_id().unwrap_or_default().to_string();
        let abort = || {
            let _ = run(c.abort_multipart_upload().bucket(tb).key(tk).upload_id(&id).send());
        };
        let part = copy_part(size);
        let mut parts = Vec::new();
        let mut at = 0i64;
        while at < size {
            let end = (at + part).min(size) - 1;
            let number = parts.len() as i32 + 1;
            let req = c
                .upload_part_copy()
                .bucket(tb)
                .key(tk)
                .upload_id(&id)
                .part_number(number)
                .copy_source(&source)
                .copy_source_range(format!("bytes={at}-{end}"));
            match vfs_rt::run_cancel(req.send(), cancel) {
                Ok(Ok(out)) => {
                    let tag = out.copy_part_result().and_then(|r| r.e_tag()).map(str::to_string);
                    parts.push(CompletedPart::builder().part_number(number).set_e_tag(tag).build());
                }
                Ok(Err(e)) => {
                    abort();
                    return Err(err(to, e));
                }
                Err(e) => {
                    abort();
                    return Err(e);
                }
            }
            at = end + 1;
        }
        let done = CompletedMultipartUpload::builder().set_parts(Some(parts)).build();
        let req = c.complete_multipart_upload().bucket(tb).key(tk).upload_id(&id).multipart_upload(done);
        match vfs_rt::run_cancel(req.send(), cancel) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                abort();
                Err(err(to, e))
            }
            Err(e) => {
                abort();
                Err(e)
            }
        }
    }

    fn actions(&self, dir: &VPath, sel: &[Entry]) -> Vec<ActionInfo> {
        if sel.is_empty() || dir.is_root() || sel.iter().all(|e| e.is_dir()) {
            return Vec::new();
        }
        let link = Param::Choice {
            id: "how".into(),
            title: "Link".into(),
            options: vec![
                ("public".into(), "Public (the object must be public)".into()),
                ("1h".into(), "Signed, for 1 hour".into()),
                ("24h".into(), "Signed, for 1 day".into()),
                ("7d".into(), "Signed, for 7 days".into()),
            ],
            default: "1h".into(),
        };
        let acl = Param::Choice {
            id: "acl".into(),
            title: "Who can read".into(),
            options: vec![("private".into(), "Only the owner (private)".into()), ("public-read".into(), "Everyone (public-read)".into())],
            default: "private".into(),
        };
        vec![
            ActionInfo { id: "s3.copy-url".into(), title: "Copy link".into(), multi: true, params: vec![link] },
            ActionInfo { id: "s3.visibility".into(), title: "Visibility".into(), multi: true, params: vec![acl] },
        ]
    }

    fn prepare_action(&self, action: &ActionInfo, sel: &[VPath], cancel: &Cancel) -> Result<ActionInfo> {
        if action.id != "s3.visibility" {
            return Ok(action.clone());
        }
        let (mut public, mut private, mut by_policy) = (0, 0, 0);
        for p in sel.iter().filter(|p| split(p).is_some_and(|(_, k)| !k.is_empty())).take(VISIBILITY_PROBE) {
            cancel.check()?;
            // A folder in the selection: no object, nothing to tell.
            let Ok((anyone, acl)) = self.visibility(p) else { continue };
            if anyone {
                public += 1;
                by_policy += usize::from(!acl);
            } else {
                private += 1;
            }
        }
        let now = match (public, private) {
            (0, _) => "only the owner".to_string(),
            (_, 0) => "everyone".to_string(),
            _ => format!("{public} public, {private} private"),
        };
        let mut title = format!("Who can read — now: {now}");
        if by_policy > 0 {
            title.push_str(". Public through the bucket policy: the ACL can't hide it");
        }
        let mut a = action.clone();
        for p in &mut a.params {
            if let Param::Choice { id, title: t, default, .. } = p
                && id == "acl"
            {
                *t = title.clone();
                *default = if public > 0 && private == 0 { "public-read".into() } else { "private".into() };
            }
        }
        Ok(a)
    }

    fn run_action(&self, id: &str, sel: &[VPath], args: &ActionArgs, cancel: &Cancel) -> Result<ActionResult> {
        // Files only: folders are prefixes, they have no link or ACL of their own.
        let files: Vec<&VPath> = sel.iter().filter(|p| split(p).is_some_and(|(_, k)| !k.is_empty())).collect();
        match id {
            "s3.copy-url" => {
                let how = args.get("how").unwrap_or("1h");
                let mut links = Vec::new();
                for p in files {
                    cancel.check()?;
                    if self.stat(p).is_ok_and(|e| !e.is_dir()) {
                        links.push(self.link(p, how)?);
                    }
                }
                Ok(ActionResult::Clipboard(links.join("\n")))
            }
            "s3.visibility" => {
                let acl = args.get("acl").unwrap_or("private");
                let mut n = 0;
                for p in files {
                    cancel.check()?;
                    let (bucket, key) = self.file_key(p)?;
                    if self.head(p, bucket, key)?.is_none() {
                        continue;
                    }
                    let req = self.client(bucket).put_object_acl().bucket(bucket).key(key).acl(ObjectCannedAcl::from(acl));
                    run(req.send()).map_err(|e| err(p, e))?;
                    n += 1;
                }
                Ok(ActionResult::Message(format!("{n} object(s) are now {acl}")))
            }
            _ => Err(Error::Unsupported),
        }
    }
}

/// The body of an object being read.
struct Body {
    body: ByteStream,
    buf: Bytes,
}

impl Read for Body {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.buf.is_empty() {
            // The worker's cancel (Esc) stops a slow download at once.
            let next = vfs_rt::run_current(async { tokio::time::timeout(STALL_TIMEOUT, self.body.next()).await })
                .map_err(|e| io::Error::other(e.to_string()))?;
            match next {
                Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, format!("no data from the storage for {} s", STALL_TIMEOUT.as_secs()))),
                Ok(None) => return Ok(0),
                Ok(Some(Ok(b))) => self.buf = b,
                Ok(Some(Err(e))) => return Err(io::Error::other(DisplayErrorContext(&e).to_string())),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf = self.buf.slice(n..);
        Ok(n)
    }
}

/// A part being sent: its number and the answer (its ETag).
type PartJob = tokio::task::JoinHandle<Result<CompletedPart>>;

/// An object being written: one PutObject at `finish`, or a multipart upload once there is more than a part,
/// up to `PARTS_IN_FLIGHT` parts sent at once. Until `finish` nothing is visible; dropped without it, the
/// multipart upload is aborted.
struct Upload {
    client: Client,
    path: VPath,
    bucket: String,
    key: String,
    part: usize,
    buf: Vec<u8>,
    /// Upload id and the parts done.
    multi: Option<(String, Vec<CompletedPart>)>,
    sending: std::collections::VecDeque<PartJob>,
    done: bool,
}

impl Upload {
    fn fail(&self, e: Error) -> io::Error {
        io::Error::other(e.to_string())
    }

    /// Wait for the oldest part in flight.
    fn wait_one(&mut self) -> Result<()> {
        if let Some(job) = self.sending.pop_front() {
            let part = vfs_rt::run_current(job)?.map_err(|e| Error::msg(&self.path, e.to_string()))??;
            self.multi.as_mut().unwrap().1.push(part);
        }
        Ok(())
    }

    /// Send the first `n` bytes of the buffer as the next part.
    fn send_part(&mut self, n: usize) -> Result<()> {
        if self.multi.is_none() {
            let req = self.client.create_multipart_upload().bucket(&self.bucket).key(&self.key).content_type(content_type(&self.key));
            let out = run(req.send()).map_err(|e| err(&self.path, e))?;
            self.multi = Some((out.upload_id().unwrap_or_default().to_string(), Vec::new()));
        }
        while self.sending.len() >= PARTS_IN_FLIGHT {
            self.wait_one()?;
        }
        let rest = self.buf.split_off(n);
        let chunk = std::mem::replace(&mut self.buf, rest);
        let (id, parts) = self.multi.as_ref().unwrap();
        let number = (parts.len() + self.sending.len()) as i32 + 1;
        let req = self.client.upload_part().bucket(&self.bucket).key(&self.key).upload_id(id).part_number(number).body(ByteStream::from(chunk));
        let path = self.path.clone();
        self.sending.push_back(rt().spawn(async move {
            let out = req.send().await.map_err(|e| err(&path, e))?;
            Ok(CompletedPart::builder().part_number(number).set_e_tag(out.e_tag().map(str::to_string)).build())
        }));
        Ok(())
    }
}

impl Write for Upload {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= self.part {
            let n = self.part;
            self.send_part(n).map_err(|e| self.fail(e))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl vfs::Writer for Upload {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let p = self.path.clone();
        if self.multi.is_none() {
            let body = ByteStream::from(std::mem::take(&mut self.buf));
            let req = self.client.put_object().bucket(&self.bucket).key(&self.key).content_type(content_type(&self.key)).body(body);
            vfs_rt::run_current(req.send())?.map_err(|e| err(&p, e))?;
        } else {
            if !self.buf.is_empty() {
                let n = self.buf.len();
                self.send_part(n)?;
            }
            while !self.sending.is_empty() {
                self.wait_one()?;
            }
            let (id, mut parts) = self.multi.clone().unwrap();
            parts.sort_by_key(|p| p.part_number());
            let done = CompletedMultipartUpload::builder().set_parts(Some(parts)).build();
            let req = self.client.complete_multipart_upload().bucket(&self.bucket).key(&self.key).upload_id(id).multipart_upload(done);
            run(req.send()).map_err(|e| err(&p, e))?;
        }
        self.done = true;
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let (false, Some((id, _))) = (self.done, &self.multi) {
            // Parts still on their way first, then the abort: a part landing after it would be billed.
            for job in std::mem::take(&mut self.sending) {
                let _ = run(job);
            }
            let _ = run(self.client.abort_multipart_upload().bucket(&self.bucket).key(&self.key).upload_id(id).send());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_names() {
        assert_eq!(split(&VPath::root()), None);
        assert_eq!(split(&VPath::new("/b")), Some(("b", "")));
        assert_eq!(split(&VPath::new("/b/a/c.txt")), Some(("b", "a/c.txt")));
        assert_eq!(enc("a b/проц%#.md"), "a%20b/%D0%BF%D1%80%D0%BE%D1%86%25%23.md");
        let c = Config { endpoint: "http://localhost:9000/".into(), ..Config::default() };
        assert_eq!(c.host(), "localhost:9000");
        assert_eq!(Config::default().host(), "aws");
        assert_eq!(url_decode("a+b%2Fc%25%2B"), "a b/c%+");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(content_type("site/index.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }

    #[test]
    fn parts_fit_the_limit() {
        assert_eq!(part_size(None), PART);
        assert_eq!(part_size(Some(1 << 30)), PART);
        // 200 GB: no more than 9 000 parts, in whole megabytes.
        let p = part_size(Some(200 << 30)) as u64;
        assert!((200u64 << 30).div_ceil(p) <= MAX_PARTS && p.is_multiple_of(1 << 20), "{p}");
        let c = Config { secret_key: "s3cr3t".into(), session_token: Some("t0ken".into()), ..Config::default() };
        assert!(!format!("{c:?}").contains("s3cr3t") && !format!("{c:?}").contains("t0ken"));
    }

    #[test]
    fn copy_parts() {
        assert_eq!(copy_part(12 << 20), 6 << 20);
        assert_eq!(copy_part(6 << 30), 512 << 20);
        let huge = 50i64 << 40;
        let p = copy_part(huge);
        assert!((huge + p - 1) / p <= 9000);
        assert!(copy_part(1) >= 5 << 20);
    }

    #[test]
    fn aws_profiles() {
        let creds = "[default]\naws_access_key_id = AKIADEF\naws_secret_access_key = s1\n\n[work]\naws_access_key_id=AKIAWORK\naws_secret_access_key=s2\naws_session_token = tok\n";
        let conf = "[default]\nregion = eu-west-1\n[profile work]\nregion = us-west-2\n";
        let d = aws_profile(creds, conf, "default").unwrap();
        assert_eq!((d.access_key.as_str(), d.region.as_deref(), d.session_token), ("AKIADEF", Some("eu-west-1"), None));
        let w = aws_profile(creds, conf, "work").unwrap();
        assert_eq!((w.secret_key.as_str(), w.session_token.as_deref(), w.region.as_deref()), ("s2", Some("tok"), Some("us-west-2")));
        assert_eq!(aws_profile(creds, conf, "none"), None);
    }
}
