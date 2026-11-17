#!/usr/bin/env bash
# Install the system dependencies needed to build ferrite (Debian / Raspberry Pi OS).
#
# Usage: ./install.sh [--pipewire] [--blacklist-dvb]
#   --pipewire       also install PipeWire dev packages (for `cargo build --features pipewire`)
#   --blacklist-dvb  blacklist the kernel DVB driver so it doesn't claim the RTL-SDR dongle
set -euo pipefail

WITH_PIPEWIRE=0
BLACKLIST_DVB=0

for arg in "$@"; do
    case "$arg" in
        --pipewire) WITH_PIPEWIRE=1 ;;
        --blacklist-dvb) BLACKLIST_DVB=1 ;;
        -h|--help) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; exit 1 ;;
    esac
done

if ! command -v apt-get >/dev/null 2>&1; then
    echo "apt-get not found; this script supports Debian-based systems only." >&2
    exit 1
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
fi

PACKAGES=(
    build-essential
    pkg-config
    librtlsdr-dev   # rtlsdr_sys (RTL-SDR dongle)
    libasound2-dev  # cpal ALSA backend (default)
)

if [ "$WITH_PIPEWIRE" -eq 1 ]; then
    PACKAGES+=(
        libpipewire-0.3-dev
        libspa-0.2-dev
        clang
        libclang-dev    # bindgen for libspa-sys / pipewire-sys
    )
fi

echo "Installing: ${PACKAGES[*]}"
$SUDO apt-get update
$SUDO apt-get install -y "${PACKAGES[@]}"

if [ "$BLACKLIST_DVB" -eq 1 ]; then
    CONF=/etc/modprobe.d/blacklist-rtl.conf
    echo "Blacklisting dvb_usb_rtl28xxu in $CONF"
    echo 'blacklist dvb_usb_rtl28xxu' | $SUDO tee "$CONF" >/dev/null
    $SUDO rmmod dvb_usb_rtl28xxu 2>/dev/null || true
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo
    echo "Rust toolchain not found. Install it with:"
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

echo
if [ "$WITH_PIPEWIRE" -eq 1 ]; then
    echo "Done. Build with: cargo build --release --features pipewire"
else
    echo "Done. Build with: cargo build --release"
fi
