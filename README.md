# uison-commander

A two-panel file manager in the spirit of Norton and Total Commander, written in Rust on
[uison](https://github.com/uisonlab/uison), a small dependency-free UI framework.

Each panel shows a **source**: the local disk, an S3 bucket, a server over SFTP, FTP/FTPS or
WebDAV. Copying, moving and deleting are the same code for all of them, so local → S3 or
SFTP → WebDAV works just like local → local. The release binary is about 6 MB with every
network source and about 1 MB without them.

> **Status:** early (0.1). Developed on macOS (Apple Silicon), also runs on Windows 10/11
> (x86_64); there the tests pass, but network sources have not yet been tried against live servers.
> On Linux (Ubuntu 24.04, GNOME on Wayland) it builds, the tests pass and the window works; F4 opens the
> default text editor, "Choose…" uses zenity or kdialog, disks under `/media`, `/run/media` and `/mnt` are
> roots. File and folder icons come from the desktop's icon theme (GNOME, KDE or the Yaru/Adwaita fallbacks).
> Passwords are not remembered there yet (no Secret Service support).

## Features

- **Keyboard-first, Total Commander keys:** F3 view, F4 edit, F5 copy, F6 move, F7 new folder,
  F8 delete, Insert / Space / Shift+arrows to select, `+` / `-` / `*` masks, quick search by
  typing, Alt+F1 / Alt+F2 to pick a source, Ctrl+U to swap panels. F1 lists every key. On a Mac
  ⌘ works as Ctrl.
- **Never blocks the UI.** Sources are called only from worker threads; a slow server or a
  folder of 50,000 files does not freeze the window. Every long operation shows progress and can
  be cancelled, even in the middle of a network request.
- **Safe writes.** A file is written under a temporary name next to its place and renamed over
  it when done, so an interrupted copy leaves nothing half-written behind.
- **Fast local copies.** A copy within one APFS volume is a clone (1 GB in about 1 ms).
- **Viewer (F3)** for text, hex and images. It reads a window around what is on screen, so a
  multi-gigabyte log opens at once from any source, with search forwards and backwards.
- **Remembered connections.** Ctrl+F opens the list of storages. Settings go to
  `~/.config/uison-commander/connections.json` (on Windows `%APPDATA%\uison-commander`),
  passwords go to the macOS Keychain or the Windows Credential Manager.
- **Light and dark themes, English and Russian** (the gear in the toolbar).

### Sources

| Source | Notes |
|---|---|
| Local disk | The only place that touches `std::fs`; APFS clones |
| Amazon S3 and compatible (MinIO, Cloudflare R2, …) | Buckets as roots, prefixes as folders, multipart uploads, object visibility (ACL), copy pre-signed / public links |
| SFTP | Password, key file (with passphrase), ssh-agent or default keys; host keys checked against `~/.ssh/known_hosts` like `ssh` does; keepalive and reconnect; optional start folder |
| FTP / FTPS | Plain, explicit (`AUTH TLS`) and implicit TLS; `MLSD` or `LIST` (Unix and DOS); connection pool; self-signed certificates trusted by fingerprint |
| WebDAV | Nextcloud, ownCloud, Apache mod_dav, nginx, rclone; http and https, Basic auth, ranged reads, server-side copy |

## Building

You need Rust 1.88 or newer. uison is a path dependency, so clone both repositories side by side:

```sh
git clone https://github.com/uisonlab/uison.git
git clone https://github.com/uisonlab/uison-commander.git
cd uison-commander
build_scripts/build.sh          # release build → target/release/uison-commander
```

Plain `cargo build --release -p commander` works too.

Network sources are Cargo features (`s3`, `sftp`, `ftp`, `webdav`, all on by default). For a
small local-only build:

```sh
cargo build --release -p commander --no-default-features
```

### Windows

`build_scripts/build.sh` is for macOS. On Windows (MSVC toolchain) build with Cargo:

```sh
cargo build --release -p commander   # → target\release\uison-commander.exe
```

The root `/` of the local disk lists the drives (`C:`, `D:`, …); paths are shown and can be typed
as Windows writes them (`C:\Users\me`). F4 opens a file in Notepad.

### Code signing on macOS

`build_scripts/build.sh` and `cargo run` sign the binary with an Apple Development identity
(`tools/sign.sh`). Without a stable Team ID the Keychain treats every rebuild as a new program
and asks for your login password again to read saved connection passwords. Set
`UISON_SIGN_IDENTITY` to the name of your own certificate. Without one, the binary stays
ad-hoc signed. It still works, with that extra prompt after each rebuild.

## Running

```sh
target/release/uison-commander                    # panels restored from the last session
target/release/uison-commander ~/Downloads        # open a folder in the left panel
target/release/uison-commander --mem              # add an in-memory source with sample files
target/release/uison-commander --mem --slow 30    # make it slow (ms per call): watch progress and cancel
```

Panels, theme and language are saved in `~/.config/uison-commander/state.json`. Trusted FTPS
certificates are saved in `~/.config/uison-commander/known_certs`. On Windows that folder is
`%APPDATA%\uison-commander`.

## Project layout

```text
crates/
  vfs/         the Source contract: paths, entries, capabilities, errors, cancellation; MemFs; conformance tests
  vfs-local/   local disk
  vfs-rt/      the shared Tokio runtime of the network sources, cancellable calls
  vfs-s3/      S3 and compatible storages (aws-sdk-s3)
  vfs-sftp/    SFTP (russh)
  vfs-ftp/     FTP and FTPS (own blocking client on std::net + rustls)
  vfs-webdav/  WebDAV (hyper + rustls)
  ops/         copy, move, delete, mkdir, folder size, masks; no UI
  commander/   the application: panels, keys, dialogs, viewer, connections, settings
```

A panel never calls its source directly. Requests go to worker threads, and the answers come
back to the UI thread through a mailbox. Outdated answers are dropped. Recursion (copying a
tree, deleting a folder) lives in `ops`, not in the sources, so progress and cancellation work
the same for every source. Every source passes the shared conformance suite in
`vfs::conformance`.

## Testing

```sh
cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --check
```

> Run `cargo fmt` **without** `--all`: with it, cargo also reformats `../uison`.

Network tests are skipped unless test servers are configured. `tools/test-env.sh` starts them in
Docker: MinIO (S3), OpenSSH (SFTP), toxiproxy (latency), vsftpd and pure-ftpd (FTP/FTPS), and
rclone and Apache mod_dav (WebDAV). It then prints the environment variables for the tests:

```sh
eval "$(tools/test-env.sh)" && UISON_REQUIRE_NET=1 cargo test --workspace
tools/test-env.sh down
```

With `UISON_REQUIRE_NET=1`, a missing server fails the test instead of skipping it.

UI checks without a window:

```sh
cargo run -p commander -- --png "$PWD/out/png"     # render screens to PNG (light, dark, Russian); absolute path
cargo run --release -p commander -- --bench <dir>  # time to open a folder and handle keys
UISON_SCRIPT=scenarios/window.json UISON_CAPTURE=out/window target/release/uison-commander --mem   # scripted run of the real window, with captures
```

## License

[MIT](LICENSE)
