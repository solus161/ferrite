# ferrite-sdr

A terminal FM radio for RTL-SDR dongles. It tunes, demodulates wideband FM in
stereo, plays it through your sound card, and draws a live spectrum and
waterfall, all inside a terminal.

It is built to run well on small hardware: a Raspberry Pi with a 70×24
terminal (such as the PocketTerm35) is the reference setup, and the layout is
designed around that size. Larger terminals work too.

**Status: v0.1.0, an early release.** Wideband FM is the only mode
implemented. See [Limitations](#limitations).

## Features

- Wideband FM with stereo decoding (pilot-locked PLL) and de-emphasis
- Live spectrum and waterfall, with a fullscreen view (`f`)
- Two frequencies: **Freq** moves the dongle and the whole span; **Tuned**
  moves the channel inside the span without retuning
- Gain stepped through the tuner's real gain table, or AGC
- Squelch, volume, mute, PPM correction, tuner bandwidth
- Pause (`Space`) stops the dongle, the audio stream and the FFT, not just
  the sound
- RSSI, stereo pilot, ring lag and audio underruns in the Info panel
- Optional trace log (`--trace`) for debugging, off by default

## Requirements

- An RTL-SDR dongle (RTL2832U-based). Tested with a Fitipower FC0013 tuner;
  the common R820T/R828D tuners are supported by librtlsdr as well.
- Linux with ALSA (default) or PipeWire.
- Debian, Ubuntu or Raspberry Pi OS for the install script. Other
  distributions can build by hand, see [Manual build](#manual-build).
- Rust 1.88 or newer (the install script sets this up).

## Install

```sh
git clone https://github.com/solus161/ferrite.git
cd ferrite
./install.sh
```

Run it as your normal user; it uses `sudo` where needed. The script:

1. installs the build dependencies with `apt` (`librtlsdr-dev`,
   `libasound2-dev`, `pkg-config`, a C toolchain);
2. blacklists the kernel DVB-T driver, so it does not claim the dongle as a TV
   tuner;
3. adds a udev rule so you can open the dongle without root, and adds you to
   the `plugdev` group;
4. installs Rust through rustup if you do not have it, or updates it if it is
   older than 1.88;
5. builds the release binary and installs it to `~/.local/bin/ferrite-sdr`.

Then **unplug and replug the dongle** and run:

```sh
ferrite-sdr
```

If the script added you to `plugdev`, log out and back in first.

### Options

| Option | Effect |
|---|---|
| `--pipewire` | Build with the PipeWire audio backend instead of ALSA |
| `--deps-only` | Install system packages, the blacklist and udev rule only; do not build |
| `--no-blacklist` | Leave the DVB-T kernel driver alone |
| `--prefix DIR` | Install to `DIR/bin` instead of `~/.local/bin` |
| `--uninstall` | Remove the binary, and the udev rule and blacklist if this script created them |

To upgrade, `git pull` and run `./install.sh` again.

### Building on a Raspberry Pi

The release profile uses fat LTO and a single codegen unit, because the DSP
depends on cross-crate inlining. That makes the build slow on a Pi: expect
several minutes on a Pi 4 or 5, and much longer on a Pi Zero 2 W. On boards
with 1 GB of RAM or less, enable swap before building.

### Manual build

On other distributions, install the equivalents of `librtlsdr-dev`,
`libasound2-dev` and `pkg-config`, then:

```sh
cargo build --release                      # ALSA
cargo build --release --features pipewire  # PipeWire (needs libpipewire, libspa, clang)
./target/release/ferrite-sdr
```

## Usage

```sh
ferrite-sdr                    # start on 91.0 MHz
ferrite-sdr --trace            # also log to ./ferrite-sdr.log
ferrite-sdr --trace /tmp/f.log # log to a chosen file
```

The screen has a sidebar with two panels, **Control** and **Info**, next to
the spectrum and waterfall. Warnings and errors flash on the bottom bar. Press
`h` at any time for the list of keys.

### Keys

| Key | Action |
|---|---|
| `↑` `↓` | Select a control, or scroll the Info panel |
| `←` `→` | Adjust the selected control |
| `Tab` | Switch between the Control and Info panels |
| `Enter` | Collapse or expand the focused panel |
| `Space` | Pause or resume |
| `f` | Fullscreen spectrum and waterfall |
| `[` `]` | Lower or raise the waterfall floor |
| `v` | Change the spectrum style |
| `m` | Mute |
| `h` | Show or hide help |
| `Esc` | Close help |
| `q` | Quit |

### Controls

| Control | Meaning |
|---|---|
| Mode | Demodulator. Only WFM works in this release |
| Freq | Dongle centre frequency; moves the whole span, and the channel with it |
| Tuned | The channel you hear, moved within the span without retuning |
| Step | How far one press moves Freq or Tuned |
| Gain | Tuner gain, stepped through the tuner's own table; turns AGC off |
| AGC | Tuner automatic gain |
| BW | Tuner bandwidth |
| PPM | Frequency correction for the dongle's crystal |
| Volume, Mute | Audio output level |
| Squelch | Signal level below which audio is muted |
| De-emph | De-emphasis time constant (display only, see below) |
| Floor, Ceiling | dB range of the spectrum and waterfall colours |

The waterfall spans the dongle's sample rate (2.4 MS/s) around **Freq**. The
MHz axis marks its centre and both edges, and the orange cursor marks the
**Tuned** channel. With the default 100 kHz step, one press of Freq shifts the
picture by only about 4% of its width; use a larger Step to move faster.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Cannot open the device | Unplug and replug the dongle. Check `lsusb` shows `0bda:2838`. Check the udev rule, and that you logged in again after joining `plugdev` |
| `usb_claim_interface error -6` | The DVB-T driver has the dongle. Run `./install.sh --deps-only`, or `sudo modprobe -r dvb_usb_rtl28xxu`, then replug |
| No sound | Check ALSA's default output (`aplay -l`, `alsamixer`), and that `Mute` is off and `Squelch` is below the RSSI shown in Info |
| Build fails in `libspa-sys` | You built with `--features pipewire` without the PipeWire dev packages. Use `./install.sh --pipewire`, or build without the feature |
| Build fails in `rtlsdr_sys` | `librtlsdr-dev` is missing |
| Anything else | Run with `--trace` and read `ferrite-sdr.log`. Panics are recorded there too |

`rtl_test -t` (from the `rtl-sdr` package) checks the dongle independently of
this program and prints which tuner it has.

## Limitations

- WFM only. NFM, AM, USB, LSB and Raw are listed under Mode but not built yet.
- De-emphasis is fixed at 50 µs. The De-emph control changes only the
  display; 75 µs (the Americas, South Korea) is not applied yet.
- The first RTL-SDR device (index 0) is always used.
- Linux only.

## Development

The workspace has two crates:

- `crates/sdr-core`: DSP building blocks and the lock-free single-producer,
  multi-consumer ring. It has no device, audio or UI dependencies.
- `crates/ferrite-sdr`: the application. It has the dongle source and DSP
  chain, the cpal speaker, and the ratatui interface.

```sh
cargo test
cargo bench --bench ring_vs_mutex   # see BENCHMARK.md
```

[PLAN.md](PLAN.md) has the design notes: the threading model, the ring design,
and the roadmap.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
