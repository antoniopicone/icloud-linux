#!/bin/sh
# icloud-linux installer: fetch, build, install and start everything.
#
#   curl -fsSL https://raw.githubusercontent.com/antoniopicone/icloud-linux/master/install.sh | sh
#
# Pass options after `-s --`:
#
#   curl -fsSL .../install.sh | sh -s -- --yes --no-run
#
# What it does, in order:
#   1. installs build and run-time packages with your package manager (sudo,
#      after asking) - only the ones that are missing;
#   2. installs a Rust toolchain with rustup (asks first) if `cargo` is missing
#      or older than 1.88;
#   3. downloads the source from GitHub and builds it in release mode;
#   4. installs icloudctl, icloudd, icloud-status and icloud-installer into
#      ~/.local/bin, plus an entry in the applications menu;
#   5. starts the guided setup: the window if there is a desktop, otherwise the
#      terminal version.
#
# It touches nothing outside your home directory except the packages in step 1.
# It refuses to run as root. Run it again at any time to update; it will not
# repeat the sign-in if you are already set up.
#
# Options:
#   -y, --yes        do not ask questions (assume yes)
#       --no-run     install, but do not start the guided setup
#       --no-gui     skip the GTK installer window (no GTK development files needed)
#       --no-deps    do not install system packages (you have them)
#       --prefix DIR install under DIR instead of ~/.local
#       --ref REF    build this branch, tag or commit instead of master
#       --uninstall  stop the services and remove what this script installed
#   -h, --help       show this text
#
# Environment: ICLOUD_LINUX_REPO (repository URL), ICLOUD_LINUX_REF,
# ICLOUD_LINUX_SRC (use this existing checkout instead of downloading), PREFIX.
#
# Everything below is inside functions and `main` runs on the last line, so a
# download that is cut short runs nothing.

set -eu

REPO_URL="${ICLOUD_LINUX_REPO:-https://github.com/antoniopicone/icloud-linux}"
REF="${ICLOUD_LINUX_REF:-master}"
SRC_OVERRIDE="${ICLOUD_LINUX_SRC:-}"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icloud-linux"
MIN_RUST_MINOR=88          # 1.88 is the first release with let-chains in edition 2024

ASSUME_YES=0
RUN=1
GUI=1
DEPS=1
UNINSTALL=0
TMP_DIR=""

# ---- output -------------------------------------------------------------------------

if [ -t 1 ]; then
    BOLD=$(printf '\033[1m'); RED=$(printf '\033[31m'); YELLOW=$(printf '\033[33m'); RESET=$(printf '\033[0m')
else
    BOLD=""; RED=""; YELLOW=""; RESET=""
fi

say()  { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

cleanup() {
    if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

have() { command -v "$1" >/dev/null 2>&1; }

# Is there a terminal we can talk to, even though stdin is the script itself?
have_tty() { ( : </dev/tty ) 2>/dev/null; }

# ask "question": succeed on yes. With --yes always succeeds; without a terminal
# it refuses to guess.
ask() {
    if [ "$ASSUME_YES" = 1 ]; then
        return 0
    fi
    if have_tty; then
        printf '%s [y/N] ' "$1" >/dev/tty
        reply=""
        read -r reply </dev/tty || return 1
        case $reply in
            y|Y|yes|YES|Yes) return 0 ;;
            *) return 1 ;;
        esac
    fi
    die "$1 - I cannot ask without a terminal. Run again with --yes to accept."
}

usage() {
    cat <<'EOF_USAGE'
icloud-linux installer: fetch, build, install and start everything.

  curl -fsSL https://raw.githubusercontent.com/antoniopicone/icloud-linux/master/install.sh | sh
  curl -fsSL .../install.sh | sh -s -- --yes --no-run

Options:
  -y, --yes        do not ask questions (assume yes)
      --no-run     install, but do not start the guided setup
      --no-gui     skip the GTK installer window (no GTK development files needed)
      --no-deps    do not install system packages (you have them)
      --prefix DIR install under DIR instead of ~/.local
      --ref REF    build this branch, tag or commit instead of master
      --uninstall  stop the services and remove what this script installed
  -h, --help       show this text

Environment: ICLOUD_LINUX_REPO, ICLOUD_LINUX_REF, ICLOUD_LINUX_SRC (use an
existing checkout instead of downloading), PREFIX.
EOF_USAGE
}

# ---- arguments ----------------------------------------------------------------------

parse_args() {
    while [ $# -gt 0 ]; do
        case $1 in
            -y|--yes) ASSUME_YES=1 ;;
            --no-run) RUN=0 ;;
            --no-gui) GUI=0 ;;
            --no-deps) DEPS=0 ;;
            --uninstall) UNINSTALL=1 ;;
            --prefix)
                [ $# -ge 2 ] || die "--prefix needs a directory"
                PREFIX=$2; shift ;;
            --ref)
                [ $# -ge 2 ] || die "--ref needs a branch, tag or commit"
                REF=$2; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1 (try --help)" ;;
        esac
        shift
    done
}

# ---- environment checks --------------------------------------------------------------

check_environment() {
    [ "$(uname -s)" = "Linux" ] || die "icloud-linux runs on Linux only."
    [ "$(id -u)" -ne 0 ] || die "do not run this as root: it installs a service for your own user. It uses sudo only to install packages."
    if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
        die "HOME is not set to a directory."
    fi
    case $PREFIX in
        /*) ;;
        *) die "--prefix must be an absolute path." ;;
    esac
}

# Where do packages come from on this machine?
detect_package_manager() {
    for pm in apt-get dnf pacman zypper; do
        if have "$pm"; then
            echo "$pm"
            return 0
        fi
    done
    return 1
}

# ---- system packages ----------------------------------------------------------------------

missing_tools() {
    missing=""
    have cc || have gcc || missing="$missing compiler"
    have pkg-config || missing="$missing pkg-config"
    have cmake || missing="$missing cmake"
    have fusermount3 || have fusermount || missing="$missing fuse"
    if [ "$GUI" = 1 ] && have pkg-config && ! pkg-config --exists gtk4 2>/dev/null; then
        missing="$missing gtk4-dev"
    fi
    echo "$missing"
}

package_list() {
    case $1 in
        apt-get) pkgs="build-essential pkg-config cmake curl ca-certificates fuse3"; gui="libgtk-4-dev" ;;
        dnf)     pkgs="gcc gcc-c++ make pkgconf-pkg-config cmake curl fuse3";       gui="gtk4-devel" ;;
        pacman)  pkgs="base-devel cmake curl fuse3";                                 gui="gtk4" ;;
        zypper)  pkgs="gcc gcc-c++ make pkg-config cmake curl fuse3";                gui="gtk4-devel" ;;
        *)       pkgs=""; gui="" ;;
    esac
    if [ "$GUI" = 1 ]; then
        pkgs="$pkgs $gui"
    fi
    echo "$pkgs"
}

as_root() {
    if have sudo; then
        sudo "$@"
    else
        die "sudo is not available. Install these packages as root, then run again with --no-deps: $PKGS"
    fi
}

install_packages() {
    [ "$DEPS" = 1 ] || return 0
    need=$(missing_tools)
    if [ -z "$need" ]; then
        say "System packages: everything needed is already installed"
        return 0
    fi

    if ! pm=$(detect_package_manager); then
        wanted="a C compiler, pkg-config, cmake and fuse3"
        if [ "$GUI" = 1 ]; then
            wanted="$wanted, and GTK 4 development files (or use --no-gui)"
        fi
        die "missing:$need. I do not know this system's package manager; install $wanted, then run again with --no-deps."
    fi
    PKGS=$(package_list "$pm")
    say "System packages needed:$need"
    note "$pm: $PKGS"
    ask "Install them now (uses sudo)?" || die "cannot continue without them. Install them and run again with --no-deps."

    # $PKGS is deliberately unquoted below: it is a list of separate package names.
    # shellcheck disable=SC2086
    case $pm in
        apt-get)
            as_root apt-get update
            as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $PKGS ;;
        dnf)    as_root dnf install -y $PKGS ;;
        pacman) as_root pacman -S --needed --noconfirm $PKGS ;;
        zypper) as_root zypper --non-interactive install $PKGS ;;
    esac

    # The GUI is a nicety: without GTK the command line setup still works.
    if [ "$GUI" = 1 ] && ! pkg-config --exists gtk4 2>/dev/null; then
        warn "GTK 4 development files are still missing; building without the installer window."
        GUI=0
    fi
}

# ---- Rust -------------------------------------------------------------------------------------

# Minor version of the cargo on PATH, or 0.
cargo_minor() {
    have cargo || { echo 0; return; }
    ver=$(cargo --version 2>/dev/null | sed -n 's/^cargo 1\.\([0-9][0-9]*\)\..*/\1/p')
    echo "${ver:-0}"
}

ensure_rust() {
    # A rustup-managed toolchain in the home directory wins over an old system one.
    if [ -d "$HOME/.cargo/bin" ]; then
        PATH="$HOME/.cargo/bin:$PATH"; export PATH
    fi
    minor=$(cargo_minor)
    if [ "$minor" -ge "$MIN_RUST_MINOR" ]; then
        say "Rust: cargo 1.$minor found"
        return 0
    fi

    if have rustup; then
        say "Rust: updating the toolchain (cargo 1.$minor is too old, 1.$MIN_RUST_MINOR needed)"
        rustup update stable --no-self-update >/dev/null 2>&1 || rustup toolchain install stable --profile minimal
        rustup default stable >/dev/null 2>&1 || true
    else
        say "Rust: cargo 1.$MIN_RUST_MINOR or newer is needed and was not found"
        note "I can install rustup (https://sh.rustup.rs) into ~/.cargo. It does not touch your shell profile."
        ask "Install the Rust toolchain now?" || die "cannot build without Rust. Install rustup and run again."
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$TMP_DIR/rustup-init.sh" \
            || die "could not download the rustup installer."
        sh "$TMP_DIR/rustup-init.sh" -y --profile minimal --default-toolchain stable --no-modify-path \
            || die "the rustup installer failed."
        PATH="$HOME/.cargo/bin:$PATH"; export PATH
    fi

    minor=$(cargo_minor)
    [ "$minor" -ge "$MIN_RUST_MINOR" ] || die "cargo is still too old (1.$minor); need 1.$MIN_RUST_MINOR or newer."
}

# ---- source ---------------------------------------------------------------------------------------

# Sets SRC to a directory containing the workspace.
fetch_source() {
    if [ -n "$SRC_OVERRIDE" ]; then
        [ -f "$SRC_OVERRIDE/Cargo.toml" ] || die "ICLOUD_LINUX_SRC=$SRC_OVERRIDE has no Cargo.toml."
        SRC=$SRC_OVERRIDE
        say "Source: using $SRC"
        return 0
    fi

    # Run from inside a checkout (./install.sh)? Build that.
    here=""
    if [ -f "$0" ]; then
        here=$(cd "$(dirname "$0")" 2>/dev/null && pwd) || here=""
    fi
    if [ -n "$here" ] && [ -f "$here/Cargo.toml" ] && [ -d "$here/crates" ] && [ -f "$here/install.sh" ]; then
        SRC=$here
        say "Source: using this checkout ($SRC)"
        return 0
    fi

    have curl || die "curl is required."
    have tar || die "tar is required."
    url="$REPO_URL/archive/$REF.tar.gz"
    say "Source: downloading $url"
    curl --proto '=https,file' --tlsv1.2 -fsSL "$url" -o "$TMP_DIR/source.tar.gz" \
        || die "could not download $url (is the branch, tag or commit '$REF' right?)"
    tar tzf "$TMP_DIR/source.tar.gz" >/dev/null 2>&1 || die "the download is not a valid archive."

    SRC="$DATA_DIR/src"
    rm -rf "$SRC"
    mkdir -p "$SRC"
    tar xzf "$TMP_DIR/source.tar.gz" -C "$SRC" --strip-components=1
    [ -f "$SRC/Cargo.toml" ] || die "the archive does not contain a Cargo workspace."
}

# ---- build ---------------------------------------------------------------------------------------------

build() {
    # Reuse compiled dependencies between updates. Without this every update of
    # a freshly extracted source tree would rebuild everything.
    if [ -z "${CARGO_TARGET_DIR:-}" ]; then
        case $SRC in
            "$DATA_DIR"/*) CARGO_TARGET_DIR="$DATA_DIR/target"; export CARGO_TARGET_DIR ;;
        esac
    fi
    TARGET="${CARGO_TARGET_DIR:-$SRC/target}"

    packages="-p icloudctl -p icloudd -p icloud-status"
    BINS="icloudctl icloudd icloud-status"
    if [ "$GUI" = 1 ]; then
        packages="$packages -p icloud-installer"
        BINS="$BINS icloud-installer"
    fi
    locked=""
    [ -f "$SRC/Cargo.lock" ] && locked="--locked"

    say "Building (a few minutes the first time)"
    # Both variables hold several separate arguments (or nothing at all).
    # shellcheck disable=SC2086
    ( cd "$SRC" && cargo build --release $locked $packages ) || die "the build failed."
    for bin in $BINS; do
        [ -x "$TARGET/release/$bin" ] || die "the build did not produce $bin."
    done
}

# ---- install ---------------------------------------------------------------------------------------------

# Was this service running before we replaced its binary?
service_active() { have systemctl && systemctl --user is-active --quiet "$1" 2>/dev/null; }

install_files() {
    say "Installing into $PREFIX/bin"
    mkdir -p "$PREFIX/bin"
    for bin in $BINS; do
        # Write beside, then rename: replacing a running binary must not fail
        # with "text file busy", and must never leave a half-written one.
        cp "$TARGET/release/$bin" "$PREFIX/bin/.$bin.new"
        chmod 755 "$PREFIX/bin/.$bin.new"
        mv -f "$PREFIX/bin/.$bin.new" "$PREFIX/bin/$bin"
    done

    if [ "$GUI" = 1 ] && [ -f "$SRC/packaging/icloud-installer.desktop" ]; then
        apps="$PREFIX/share/applications"
        mkdir -p "$apps"
        # An absolute Exec= works whether or not ~/.local/bin is on the menu's PATH.
        sed "s|^Exec=.*|Exec=$PREFIX/bin/icloud-installer|" "$SRC/packaging/icloud-installer.desktop" \
            > "$apps/icloud-installer.desktop"
        if have update-desktop-database; then
            update-desktop-database "$apps" >/dev/null 2>&1 || true
        fi
    fi

    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) warn "$PREFIX/bin is not on your PATH. Add it to your shell profile to run icloudctl by name." ;;
    esac
}

restart_running_services() {
    restarted=""
    for unit in icloud.service icloud-status.service; do
        if service_active "$unit"; then
            systemctl --user restart "$unit" 2>/dev/null && restarted="$restarted $unit"
        fi
    done
    [ -z "$restarted" ] || say "Restarted:$restarted"
}

# ---- setup ---------------------------------------------------------------------------------------------------

already_configured() {
    cfg="${XDG_CONFIG_HOME:-$HOME/.config}/icloud-linux/config.yaml"
    [ -f "$cfg" ] && grep -Eq "^username:[[:space:]]*['\"]?[^'\" ]" "$cfg"
}

# An update keeps the user's setup and adds what newer versions bring: the
# right-click entry and the activity label in the sidebar. Both are idempotent.
refresh_integrations() {
    if "$PREFIX/bin/icloudctl" menu-install >/dev/null 2>&1; then
        note "Right-click a file in iCloud Drive, then Scripts > Download from iCloud, to keep it on this computer."
    fi
    "$PREFIX/bin/icloudctl" status-install >/dev/null 2>&1 || true
}

run_setup() {
    if already_configured; then
        refresh_integrations
    fi
    if [ "$RUN" != 1 ]; then
        next_steps
        return 0
    fi
    if already_configured; then
        say "Already set up: nothing more to do. Run 'icloudctl doctor' if something looks wrong."
        return 0
    fi

    if [ "$GUI" = 1 ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
        say "Starting the guided setup window"
        cleanup; TMP_DIR=""      # `exec` skips the exit trap
        exec "$PREFIX/bin/icloud-installer"
    fi
    if have_tty; then
        say "Starting the guided setup in this terminal"
        cleanup; TMP_DIR=""
        exec "$PREFIX/bin/icloudctl" quickstart </dev/tty
    fi
    warn "no desktop and no terminal to run the guided setup in."
    next_steps
}

next_steps() {
    say "Installed. To finish setting up:"
    if [ "$GUI" = 1 ]; then
        note "icloud-installer            guided setup window"
    fi
    note "icloudctl quickstart        the same, in the terminal"
    note "icloudctl doctor            check the installation"
}

# ---- uninstall -----------------------------------------------------------------------------------------------------

uninstall() {
    say "Uninstalling"
    if [ -x "$PREFIX/bin/icloudctl" ]; then
        "$PREFIX/bin/icloudctl" uninstall --yes 2>/dev/null || warn "icloudctl could not remove the services; continuing."
    fi
    for bin in icloudctl icloudd icloud-status icloud-installer; do
        rm -f "$PREFIX/bin/$bin"
    done
    rm -f "$PREFIX/share/applications/icloud-installer.desktop"
    rm -rf "$DATA_DIR"
    say "Removed. Your configuration, iCloud session and cache were kept."
    note "To delete them too, run 'icloudctl uninstall --purge' BEFORE removing the program,"
    note "or remove ~/.config/icloud-linux, ~/.cache/icloud-linux and ~/.local/state/icloud-linux by hand."
}

# ---- main ------------------------------------------------------------------------------------------------------------------

main() {
    parse_args "$@"
    check_environment
    if [ "$UNINSTALL" = 1 ]; then
        uninstall
        return 0
    fi

    have curl || die "curl is required."
    TMP_DIR=$(mktemp -d)
    say "icloud-linux installer ($REF)"
    if have systemctl && ! systemctl --user show-environment >/dev/null 2>&1; then
        warn "no systemd user session detected: the service cannot be started from here. Log in to a normal desktop session for the guided setup."
    fi

    install_packages
    ensure_rust
    fetch_source
    build
    install_files
    restart_running_services
    run_setup
}

main "$@"
