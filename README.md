# icloud-linux

Mount iCloud Drive on Linux as a normal folder. Folders open the way they do in Finder, a file downloads when you open it or choose **Download from iCloud** in its right-click menu, and what you change is uploaded for you.

```
~/iCloud/
├── Documents/        ← listed the first time you open it
├── Photos Export/
└── notes.txt         ← a placeholder until you open it, then a real local file
```

It is a FUSE filesystem backed by a persistent local cache. Reads come from disk, writes land on disk immediately, and a sync engine reconciles with iCloud in the background.

## Status

This is a Rust rewrite of the earlier Python implementation. What has and has not been checked matters, so plainly:

| | |
|---|---|
| Sync engine, cache, FUSE layer | Tested against an in-memory iCloud and through **real kernel FUSE mounts** (`std::fs` calls on a live mount). |
| Sign-in (SRP), two-factor, Drive requests | Ported from `pyicloud` 2.7.0. The SRP maths is checked byte for byte against the reference Python library and the request sequences against scripted stand-ins. **Confirmed on a real account:** sign-in with a text-message code, listing folders and downloading files. **Not confirmed yet:** uploads, and the code-on-your-devices method (see Signing in). |
| Installer window | Run headlessly in demo mode and inspected screenshot by screenshot; used on a real desktop as far as the sign-in step. |

`icloudctl auth` sends the password proof once and never retries, so a mismatch cannot lock your account by itself.

Linux only. FUSE 3 and systemd (user services) are required.

## Install

### One command

```bash
curl -fsSL https://raw.githubusercontent.com/antoniopicone/icloud-linux/master/install.sh | sh
```

It installs what is missing (system packages through `sudo` after asking, a Rust toolchain through rustup after asking), downloads the source, builds it, installs `icloudctl`, `icloudd`, `icloud-status` and `icloud-installer` into `~/.local/bin` with an entry in the applications menu, and starts the guided setup: the window if you are on a desktop, the terminal version otherwise. It refuses to run as root, touches nothing outside your home directory except those packages, and does not repeat the sign-in if you are already set up, so running it again is how you update.

```bash
curl -fsSL …/install.sh | sh -s -- --yes --no-run     # unattended, no setup afterwards
curl -fsSL …/install.sh | sh -s -- --no-gui           # skip the window (no GTK needed)
curl -fsSL …/install.sh | sh -s -- --ref v0.2.0       # a branch, tag or commit
curl -fsSL …/install.sh | sh -s -- --uninstall        # remove it (keeps your data)
```

Read it first if you like: it is [`install.sh`](install.sh), plain POSIX `sh`. Supported package managers: `apt`, `dnf`, `pacman`, `zypper`.

### From a checkout

```bash
git clone https://github.com/antoniopicone/icloud-linux.git
cd icloud-linux
./install.sh            # builds and installs this checkout
```

or by hand, if you would rather not use the script. Build needs Rust 1.88+, a C compiler, `pkg-config`, `cmake` and (for the window) GTK 4 development files; at run time `fuse3` and `systemd --user`:

```bash
cargo build --release
install -m 755 target/release/{icloudctl,icloudd,icloud-status,icloud-installer} ~/.local/bin/
```

### Guided setup

```bash
icloud-installer        # or "iCloud for Linux Setup" in your app menu
```

Six pages: requirements → folder → Apple ID → verification code → setup → done. It runs the same steps as `icloudctl init`, `configure`, `auth` and `start`, including the two-factor prompt. `icloud-installer --demo` walks through it without an Apple account and without changing anything (code `123456`).

### Or from the terminal

```bash
icloudctl quickstart ~/iCloud
```

or step by step:

```bash
icloudctl init ~/iCloud
icloudctl configure you@example.com
icloudctl auth
icloudctl start
```

## Signing in

Apple ID sign-in uses SRP, so your password is never sent to Apple in the clear, and it is **not stored unless you ask** (`icloudctl configure --store-password`, or the checkbox in the installer). Without it, an expired session means running `icloudctl auth` again, which happens every few weeks. With it, the service renews a trusted session by itself.

Two-factor works three ways:

| Method | |
|---|---|
| Text message | `icloudctl auth --force-sms`, or type `sms` at the prompt. Works for every account with a trusted phone number, and is what the installer offers. |
| Code from a trusted device | Type the six digits your iPhone/iPad/Mac shows. Only for accounts where Apple sends it by itself; see below. |
| Browser trust token | `icloudctl auth --trust-token <X-APPLE-WEBAUTH-HSA-TRUST from icloud.com cookies>` skips the code. |

Not supported: Apple's newer push-to-device bridge and hardware security keys. On most accounts Apple no longer shows a code on your devices unless the app performs that push handshake (the sign-in page announces it as `auth/bridge/step`); nothing would ever arrive, so those accounts are steered to the text message instead of being left waiting. If your account can only use those, sign in at icloud.com once and import the trust token above. If a push popup appears instead of a code, use the text message.

A wrong code counts towards Apple locking the account, so the installer and `icloudctl auth` both stop after three.

## Everyday commands

```
icloudctl start | stop | restart | status | logs
icloudctl download PATH...         download these files or folders now (the right-click entry runs this)
icloudctl menu-install | menu-uninstall   add or remove "Download from iCloud" in Files' right-click menu
icloudctl sync [--timeout SEC]     refresh from iCloud now and wait for it
icloudctl refresh                  ask for a refresh without waiting
icloudctl hydrate [--dry-run]      download every eligible file that is not local yet
icloudctl doctor                   check the installation and say how to fix what is wrong
icloudctl clear-cache              delete the local cache (asks first)
icloudctl status-install           show what iCloud is doing next to it in the Files sidebar
icloudctl trackerignore            keep GNOME's search indexer out of the mount
icloudctl uninstall [--purge]
```

## How it works

**Listing.** Nothing is crawled at startup. A folder is listed from iCloud the first time something reads it, and at most once per `remote_refresh_interval_seconds`. `ls ~/iCloud` lists the root and nothing else. The mount is up in seconds however large the drive is. Listing never downloads file contents: each file is a sparse placeholder with the right name, size and date. (`crawl_mode: full` restores a whole-drive crawl if you prefer it.)

**Downloading.** A file's contents are fetched only when a person asks: by opening it (double click) or by choosing **Download from iCloud** on it (or on a folder) in the right-click menu of Files, under *Scripts*. It is downloaded once into `~/.cache/icloud-linux/mirror`, writing to a temporary file and renaming into place so a reader never sees half a file. A download that raced with a local delete or an edit is discarded rather than committed.

Programs that walk the drive on their own do not trigger downloads. The daemon looks at which program is reading: search indexers (Tracker, LocalSearch, Baloo) are turned away with "permission denied", and thumbnailers get a preview only for files up to `preview_max_bytes` (200 kB by default). Bigger files show a generic icon until you download them. This keeps a folder from being pulled down file by file just because you looked at it, and keeps that queue from delaying the file you did open. Files that are already local are read freely by everyone.

A thumbnailer that was turned away is remembered by Files as "failed" (in `~/.cache/thumbnails/fail`), so `icloudctl download` clears that note for the files it fetched and the thumbnail appears the next time the folder is drawn. For a file you open some other way, `rm -r ~/.cache/thumbnails/fail` does the same for everything.

**Uploading.** Writes go to the mirror and mark the file dirty; a pass every 30 seconds sends what changed. A rename or move is a rename or move on iCloud, not a re-upload. If a file changes *while* it is uploading, it stays dirty and goes up again. Deletes go to iCloud's "Recently Deleted" (`delete_mode: permanent` changes that).

**Conflicts.** If a path changed on both sides, the local version is kept as `<name>.local-conflict-<timestamp>` next to the remote one. Nothing is overwritten silently.

**Boundary.** `sync_paths` / `exclude_paths` make a hard boundary: outside it, names and sizes are visible but contents cannot be read (`EACCES`, never empty data) and nothing can be changed. See [`config.example.yaml`](config.example.yaml) for every option.

**Behaviour that follows POSIX.** A file cannot be renamed over a directory; a directory can only replace an empty one; `rmdir` on a folder you have never opened lists it first instead of mistaking it for empty.

## Desktop integration

**Sidebar.** iCloud appears once in the Files sidebar, as a folder named *iCloud*, and `icloud-status` adds what it is doing to the name: `iCloud (downloading: invoice.pdf)`. The mount is deliberately not listed as a drive: it would otherwise show up a second time as a removable disk with an eject button. Use `icloudctl stop` to unmount. (Files shows a cloud icon and a progress indicator only for sync clients that register with libcloudproviders, which needs a file installed system-wide; not done here.)

**Right-click.** *Scripts ▸ Download from iCloud* on any file or folder inside iCloud Drive. It is a script in `~/.local/share/nautilus/scripts`, so nothing has to be loaded into Files and no extra package is needed. A desktop notification reports the start and the result. Add or remove it with `icloudctl menu-install` / `menu-uninstall`.

**Search indexer.** GNOME's file indexer walks `$HOME` and opens every file. On a mount that looks like a person opening everything, and it would download the whole drive within seconds. The daemon writes a `.trackerignore` marker into the mirror to prevent that; `icloudctl trackerignore` applies it to an existing install, and `icloudctl doctor` reports whether it is in place. As a second line of defence the daemon refuses the indexer's reads of files that are not downloaded (see Downloading).

`icloudctl status-install` puts what the daemon is doing in the sidebar label of the `iCloud` entry: `iCloud (listing: Documents)`, `iCloud (downloading: invoice.pdf)`, plain `iCloud` when idle. It is a small user service, `icloud-status`, that follows the daemon's log and rewrites one line of `~/.config/gtk-3.0/bookmarks`, which Nautilus and every GTK file dialog watch, so it needs no Nautilus plugin and no Python. It stops with the daemon and puts the plain label back.

This replaces the old `icloud_status.py` Nautilus extension, minus one thing: that extension also added an "iCloud sync status…" entry to the context menu. A menu entry has to live inside Nautilus's own process, which in Rust means a native plugin library with C-ABI FFI, and nothing here uses `unsafe`. The label shows the same information.

## Security

- No `unsafe` code anywhere in the workspace (`unsafe_code = "forbid"`); system calls go through `rustix`.
- Passwords are wrapped so they cannot reach a log or a `Debug` print, and are not stored by default.
- Session, config, cache and log files are created with mode 0600/0700 and written atomically.
- Paths are a validated type that cannot contain `..`, so neither a hostile file name from iCloud nor a crafted request can leave the mirror. Server-sent names that are not valid file names are ignored.
- A server asking for an absurd PBKDF2 iteration count is refused instead of burning CPU.
- One sign-in attempt sends one password proof; nothing retries it. A daemon whose session is gone starts read-only ("unauthenticated") rather than crash-looping.

## Moving from the Python version

`config.yaml` is read unchanged, and the existing cache and state database are reused. What changes:

- **Sign in again once.** Session files are in a new format: `icloudctl auth`.
- `fuse_options` (`ro`, `allow_other`) now actually take effect; the old code never read them.
- Deletes go to the trash by default instead of being permanent.
- Mode options with an unknown value are an error instead of silently falling back.
- Fixed while porting: subtree queries treated `_` and `%` in names as wildcards (removing `/my_docs` could remove `/myXdocs`); renaming over a synced file violated a database constraint; renaming a file over a folder deleted the folder; a renamed placeholder read back as zeros; `write` hashed the whole file on every call.
- Removed: the virtualenv, `hydrate_dir.py`, `fix_filenames.py` (an ad-hoc NAS filename sanitiser that edited the database directly), and the Python Nautilus extension (see Desktop integration; `icloudctl nautilus-install` still works as an alias for `status-install`).

## Development

```
crates/
  icloud-api/        HTTP client: SRP, two-factor, sessions, the Drive service. `MemoryDrive` for tests.
  icloud-core/       config, state database, mirror, sync engine, filesystem semantics, setup, installer logic.
  icloudd/           the FUSE daemon (a thin adapter over icloud-core).
  icloudctl/         the command line tool.
  icloud-status/     the sidebar label watcher (its logic lives in icloud-core::status).
  icloud-installer/  the GTK4 window (a view over icloud-core's installer logic).
install.sh           the curl | sh installer.
tools/ci/            container recipes for Linux tests and installer screenshots.
```

Everything that decides behaviour sits behind the `Drive` trait and `FsCore`, so it is tested without a network or a display. iCloud Notes, when it comes, is another service module in `icloud-api` next to `drive`, reached through the same `Client`.

```bash
cargo test -p icloud-api -p icloud-core -p icloudctl -p icloud-status   # anywhere, including macOS
cargo test --workspace                                 # everything, on Linux
tools/ci/run.sh cargo test --workspace                 # the same in a container (podman or docker), works on macOS
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
cargo run -p icloud-installer -- --demo                # the installer without an account
tools/ci/screenshots.sh                                # the installer's pages, headless, into target-shots/
```

The mount tests skip themselves when `/dev/fuse` is unavailable; the container provides it. `install.sh` itself is checked with `shellcheck` and was run end to end in a clean Debian container, from an empty machine to installed binaries and back to uninstalled.

## Troubleshooting

```bash
icloudctl doctor      # first thing to try
icloudctl logs
```

- **Sign-in says the email/password is wrong** — the message ends with what Apple actually answered, e.g. `(Apple replied: … -20101 …)` for a wrong password or `403` for a locked or throttled account. Run `icloudctl -v auth` to see each request's address and status code (never passwords, tokens or cookies); the installer does the same with `ICLOUD_LOG=debug icloud-installer`. Check the password at <https://account.apple.com> first, and do not retry more than a couple of times: repeated failures lock the account.
- **A picture or PDF shows a generic icon** — its contents are not downloaded just to draw a thumbnail when the file is larger than `preview_max_bytes`. Right-click ▸ Scripts ▸ Download from iCloud, or open it.
- **`UNAUTHENTICATED mode` in the logs** — the session is gone. `icloudctl auth`, then `icloudctl restart`.
- **The whole drive starts downloading by itself** — the desktop indexer is walking the mount: `icloudctl trackerignore`.
- **A folder looks empty** — check `icloudctl logs` for a `list-directory-start` with no matching `list-directory-complete`; the listing request failed.
- **`icloudctl hydrate` finds little to do** — in lazy mode only folders you have opened are known. Walk a tree first (`find ~/iCloud/Downloads -type d >/dev/null`) or set `crawl_mode: full`.
- **Start over** — `icloudctl clear-cache` deletes the cache; it is rebuilt from iCloud. Changes not yet uploaded are lost with it, so let `icloudctl sync` finish first.
