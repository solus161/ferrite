#!/usr/bin/env bash
# Build and install ferrite-sdr from source (Debian, Ubuntu, Raspberry Pi OS).
#
# Usage: ./install.sh [options]
#   --pipewire       build with the PipeWire audio backend instead of ALSA
#   --deps-only      install system packages only; do not build
#   --no-blacklist   leave the kernel DVB-T driver alone
#   --prefix DIR     install the binary to DIR/bin (default: ~/.local)
#   --uninstall      remove the binary, udev rule and blacklist this script added
#
# Run as your normal user; the script calls sudo for the system steps. It is
# safe to re-run, which is also how you upgrade after a `git pull`.
set -euo pipefail

BIN=ferrite-sdr
MIN_RUST=1.88
UDEV_RULE=/etc/udev/rules.d/20-rtlsdr.rules
BLACKLIST=/etc/modprobe.d/blacklist-rtlsdr.conf

WITH_PIPEWIRE=0
DEPS_ONLY=0
BLACKLIST_DVB=1
UNINSTALL=0
PREFIX="$HOME/.local"

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --pipewire) WITH_PIPEWIRE=1 ;;
        --deps-only) DEPS_ONLY=1 ;;
        --no-blacklist) BLACKLIST_DVB=0 ;;
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

step() { printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# rustup and the binary belong to the user; under sudo they would land in /root.
[ "$(id -u)" -ne 0 ] || die "run as your normal user, not root; the script uses sudo where needed"
command -v sudo >/dev/null || die "sudo not found"

cd "$(dirname "$0")"

if [ "$UNINSTALL" -eq 1 ]; then
    step "Removing $BIN"
    rm -fv "$PREFIX/bin/$BIN"
    if grep -qs "# ferrite-sdr: PATH" "$HOME/.bashrc"; then
        sed -i '/# ferrite-sdr: PATH$/d' "$HOME/.bashrc"
        echo "removed the PATH line from ~/.bashrc"
    fi
    for f in "$UDEV_RULE" "$BLACKLIST"; do
        if [ -f "$f" ] && grep -q 'ferrite-sdr' "$f"; then
            sudo rm -fv "$f"
        fi
    done
    sudo udevadm control --reload-rules 2>/dev/null || true
    echo "System packages and the Rust toolchain were left installed."
    exit 0
fi

command -v apt-get >/dev/null || die "apt-get not found; see README.md for building on other systems"

# ── System packages ──────────────────────────────────────────────────────────
PACKAGES=(
    build-essential
    pkg-config
    curl ca-certificates  # rustup
    librtlsdr-dev         # RTL-SDR driver (rtlsdr_mt links it)
    libasound2-dev        # cpal ALSA backend
)
if [ "$WITH_PIPEWIRE" -eq 1 ]; then
    PACKAGES+=(
        libpipewire-0.3-dev
        libspa-0.2-dev
        clang libclang-dev  # bindgen for libspa-sys / pipewire-sys
    )
fi

step "Installing system packages"
sudo apt-get update
sudo apt-get install -y "${PACKAGES[@]}"

# ── Dongle access ────────────────────────────────────────────────────────────
# At plug-in the kernel claims the dongle as a DVB-T tuner. librtlsdr detaches
# it on open, but blacklisting avoids the race and the "usb_claim_interface
# error -6" some kernels give.
if [ "$BLACKLIST_DVB" -eq 1 ] && [ ! -f "$BLACKLIST" ]; then
    step "Blacklisting the DVB-T driver ($BLACKLIST)"
    sudo tee "$BLACKLIST" >/dev/null <<'EOF'
# Added by ferrite-sdr install.sh: keep the DVB-T driver off RTL-SDR dongles.
blacklist dvb_usb_rtl28xxu
blacklist rtl2832_sdr
blacklist rtl2832
EOF
    sudo modprobe -r dvb_usb_rtl28xxu rtl2832_sdr rtl2832 2>/dev/null || true
fi

# Without a rule, opening the dongle needs root.
if ! grep -rqs '0bda.*2838' /etc/udev/rules.d /lib/udev/rules.d /usr/lib/udev/rules.d; then
    step "Adding udev rule for RTL-SDR dongles ($UDEV_RULE)"
    sudo tee "$UDEV_RULE" >/dev/null <<'EOF'
# Added by ferrite-sdr install.sh: let plugdev members and the local session use RTL-SDR dongles.
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0660", GROUP="plugdev", TAG+="uaccess"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2832", MODE="0660", GROUP="plugdev", TAG+="uaccess"
EOF
    sudo udevadm control --reload-rules
    sudo udevadm trigger
fi
if getent group plugdev >/dev/null && ! id -nG | grep -qw plugdev; then
    sudo usermod -aG plugdev "$USER"
    warn "added $USER to the plugdev group; log out and back in for it to apply"
fi

if [ "$DEPS_ONLY" -eq 1 ]; then
    echo; echo "Dependencies installed. Build with: cargo build --release"
    exit 0
fi

# ── Rust toolchain ───────────────────────────────────────────────────────────
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v cargo >/dev/null; then
    step "Installing Rust via rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
fi

have_rust=$(rustc --version | awk '{print $2}')
if [ "$(printf '%s\n%s\n' "$MIN_RUST" "$have_rust" | sort -V | head -n1)" != "$MIN_RUST" ]; then
    if command -v rustup >/dev/null; then
        step "Updating Rust ($have_rust < $MIN_RUST)"
        rustup update stable
    else
        die "Rust $have_rust is older than $MIN_RUST and was not installed by rustup; upgrade it"
    fi
fi

# ── Build and install ────────────────────────────────────────────────────────
FEATURES=()
[ "$WITH_PIPEWIRE" -eq 1 ] && FEATURES=(--features pipewire)

step "Building $BIN (release; this takes a while on a Raspberry Pi)"
cargo build --release --locked -p "$BIN" "${FEATURES[@]}"

step "Installing to $PREFIX/bin/$BIN"
install -Dm755 "target/release/$BIN" "$PREFIX/bin/$BIN"

# Debian's ~/.profile adds ~/.local/bin only if it existed at login, so on a
# fresh account the directory this script just created is not on PATH until
# the next login. Add it to ~/.bashrc so new shells find the binary.
ON_PATH=1
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *)
        ON_PATH=0
        if ! grep -qs "# ferrite-sdr: PATH" "$HOME/.bashrc"; then
            step "Adding $PREFIX/bin to PATH in ~/.bashrc"
            printf '\nexport PATH="%s/bin:$PATH"  # ferrite-sdr: PATH\n' "$PREFIX" >> "$HOME/.bashrc"
        fi
        ;;
esac

echo
echo "Done. Plug in the dongle (re-plug it if it was already in) and run: $BIN"
if [ "$ON_PATH" -eq 0 ]; then
    echo "This shell does not see $PREFIX/bin yet; first run: source ~/.bashrc"
    echo "(or run it directly: $PREFIX/bin/$BIN)"
fi
