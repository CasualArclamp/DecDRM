# Building DecDRM

Step-by-step build instructions for Windows, Ubuntu/Debian and a Raspberry Pi 4.
The user guide ([USER_GUIDE.md](USER_GUIDE.md)) covers running the programs; this
file is only about getting them built.

**What is verified.** CI builds the whole workspace on Linux (x86_64) and Windows,
runs the tests, the lints and the smoke tests there, and additionally builds the CLI
and runs the station's unit tests on **aarch64** — the Raspberry Pi's architecture, on
a GitHub-hosted arm64 runner (not a Pi itself). The Pi-specific advice below (powered
hub, the kernel module, throttling, systemd) is still a guide rather than a CI step;
the resampler numbers and the write-timeout recovery were measured on a Pi 4B
(Model B Rev 1.4, 8 GB, Ubuntu 24.04).

## Common requirements

- **Rust 1.88 or newer**; the GUI (`decdrm-gui`, eframe/egui 0.36) needs **1.95 or
  newer**. `rustup` is the usual way to get and update both.
- A **C/C++ compiler** and **CMake**: the AAC, xHE-AAC and Opus libraries are
  vendored as git submodules (`third_party/fdk-aac`, `third_party/libxaac`,
  `third_party/opus`) and built by their crates (FDK-AAC and libxaac with `cc`,
  Opus with CMake).
- `pkg-config` on Linux.
- The `third_party/` **submodules**: clone with `--recursive`, or run
  `git submodule update --init --recursive` in an existing clone.
- The `dac` feature (the neural audio codec) is **optional**: it pulls in the
  candle ML runtime and its weights. Nothing else here needs it; the portable
  Windows script builds it in.

```bash
git clone --recursive https://github.com/Ageres/DecDRM-fork.git
cd DecDRM-fork
cargo build --release          # target/release/decdrm and target/release/decdrm-gui
```

A quick check without a radio or a sound card: `cargo test --workspace --release`,
and on Linux `xvfb-run -a scripts/smoke-test.sh target/release /tmp/smoke` (the CLI
transmits the example station and receives it; the GUI decodes it and saves a
screenshot).

## Windows (x86_64)

1. Install Rust with [rustup](https://rustup.rs/) (a recent stable; 1.95+ if you
   want the GUI).
2. Install **Visual Studio Build Tools 2022** with the *Desktop development with
   C++* workload (MSVC compiler and the Windows SDK) and **CMake** (as a Build
   Tools component, or standalone and on `PATH`). *Git for Windows* makes the
   clone easy.
3. Clone with the submodules and build:

   ```powershell
   git clone --recursive https://github.com/Ageres/DecDRM-fork.git
   cd DecDRM-fork
   cargo build --release
   ```

4. **Disk space.** The release profile keeps line tables for backtraces and the
   build is large. On a small disk, drop them for the build — this also avoids the
   linker's `LNK1106`/`LNK1140` ("cannot seek/read") failures when the disk fills:

   ```powershell
   $env:CARGO_PROFILE_RELEASE_DEBUG = 'false'
   cargo build --release
   ```

   `cargo build --release --jobs 1` helps if the machine is short of RAM.

5. **HackRF driver.** The radio needs the WinUSB driver, installed once with
   [Zadig](https://zadig.akeo.ie/): *Options → List All Devices*, pick
   `HackRF One` (USB id `1d50:6089`), choose *WinUSB* and *Replace Driver*. Then
   check the configuration without opening the radio, and transmit:

   ```powershell
.\target\release\decdrm.exe tx station\station.toml --check
.\target\release\decdrm.exe tx station\station.toml
   ```

   The GUI's Output card lists the radios it finds; *Scan* repeats the USB scan.
   "The HackRF could not be opened" usually means another program (the GUI, SDR
   software) is using the radio, or the WinUSB driver is not installed.

6. **Portable single-file executables** (nothing to install, C runtime linked
   statically, DAC weights built in) go to `exe\`:

   ```powershell
   .\target\release\decdrm.exe models download dac
   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
   ```

   The prebuilt executables on the upstream releases come from upstream and have
   no HackRF output — build this fork's own to get it.

## Ubuntu / Debian

```bash
sudo apt update
sudo apt install -y build-essential cmake pkg-config \
    libasound2-dev libusb-1.0-0-dev libudev-dev
# GUI run-time libraries (not needed to build):
sudo apt install -y libgl1-mesa-dri libegl1 libxkbcommon-x11-0
```

`libasound2-dev` is for sound-card input/output, `libusb-1.0-0-dev` and
`libudev-dev` for the HackRF output (its `rusb` dependency links libusb), CMake
for the vendored Opus.

Install Rust with rustup, clone with the submodules and build as above.

**HackRF permissions.** A normal user needs the udev rule and the `plugdev`
group (or run as root):

```bash
sudo tee /etc/udev/rules.d/53-hackrf.rules >/dev/null <<'EOF'
SUBSYSTEM=="usb", ATTRS{idVendor}=="1d50", ATTRS{idProduct}=="6089", MODE="0660", GROUP="plugdev"
EOF
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG plugdev "$USER"     # log out and in again for the group to apply
```

Then `target/release/decdrm tx station.toml --check` and `... tx station.toml`.

## Raspberry Pi 4 (64-bit)

A headless station runs the CLI; the GUI is not needed there (it needs a desktop
session and Rust 1.95).

**OS and hardware.** Use a **64-bit** OS: Raspberry Pi OS Bookworm (64-bit) or
Ubuntu 24.04 arm64. The GUI does not build on a 32-bit userland, and the Rust
support for 32-bit ARM is thin. Use the official 5 V/3 A supply (a sagging one
shows up as `get_throttled` below), a heatsink or fan, and a card with about 16 GB
free: the release build of the vendored C/C++ codecs is large. Eight gigabytes
of RAM make the build comfortable; on 2–4 GB add swap. A 1 GB Pi runs the CLI
fine but cannot build it comfortably — take the arm64 artifact from CI (or
build on a PC), or add a large swap and be patient. The radio draws about
500 mA at peaks, so power it through a **powered USB hub**.

The order below is the one that works: OS, updates and swap, packages, Rust, the
repository, the build, the kernel module and udev, the radio, the station.

1. **Update the system and add swap** (the compiler is the memory hog):

   ```bash
   sudo apt update && sudo apt full-upgrade -y
   # Ubuntu: a 4 GB swap file
   sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile
   sudo mkswap /swapfile && sudo swapon /swapfile
   echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
   # Raspberry Pi OS instead: CONF_SWAPSIZE=4096 in /etc/dphys-swapfile, then
   #   sudo dphys-swapfile setup && sudo dphys-swapfile swapon
   ```

2. **Packages** (the Ubuntu list, repeated so this section stands alone):

   ```bash
   sudo apt install -y build-essential cmake pkg-config git \
       libasound2-dev libusb-1.0-0-dev libudev-dev
   ```

3. **Rust** with rustup (the CLI needs 1.88 or newer; the GUI 1.95):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   . "$HOME/.cargo/env"
   ```

4. **The repository**, with the vendored codecs as submodules:

   ```bash
   git clone --recursive https://github.com/Ageres/DecDRM-fork.git
   cd DecDRM-fork
   ```

5. **Build the CLI.** One job and no debug info, so the linker and the card cope;
   the first build takes tens of minutes on a Pi 4:

   ```bash
   CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_DEBUG=false \
       cargo build --release -p decdrm-cli
   ```

   A later `git pull` rebuilds the station and the CLI (minutes), not the vendored
   codecs.

   **Or cross-compile on a PC** (needs Docker there; the CI builds on a native
   arm64 runner instead, so this route is not exercised):

   ```bash
   cargo install cross
   cross build --release --target aarch64-unknown-linux-gnu -p decdrm-cli
   ```

6. **Keep the kernel's own `hackrf` module off the radio.** Ubuntu's kernel ships
   an SDR driver for the HackRF that binds the radio at boot (it shows up as
   `/dev/swradio0`); libusb then cannot claim it: `lsusb` lists `1d50:6089`, but
   the station says `no HackRF One found` or `the HackRF could not be opened`.
   Blacklist the module:

   ```bash
   echo 'blacklist hackrf' | sudo tee /etc/modprobe.d/blacklist-hackrf.conf
   sudo modprobe -r hackrf            # if it is loaded already
   # lsmod | grep hackrf and ls /dev/swradio* must come back empty
   ```

   This is the kernel module, not the `hackrf` package with the `hackrf_transfer`
   tools: the blacklist is what matters.

7. **Permissions and the radio.** The udev rule and the `plugdev` group (log out
   and in again for the group to apply), and no USB autosuspend on the radio:

   ```bash
   sudo tee /etc/udev/rules.d/53-hackrf.rules >/dev/null <<'EOF'
   SUBSYSTEM=="usb", ATTRS{idVendor}=="1d50", ATTRS{idProduct}=="6089", MODE="0660", GROUP="plugdev"
   EOF
   sudo tee /etc/udev/rules.d/53-hackrf-power.rules >/dev/null <<'EOF'
   SUBSYSTEM=="usb", ATTRS{idVendor}=="1d50", ATTRS{idProduct}=="6089", ATTR{power/control}="on"
   EOF
   sudo udevadm control --reload-rules && sudo udevadm trigger
   sudo usermod -aG plugdev "$USER"
   ```

   Plug the radio into the powered hub; `lsusb | grep 1d50:6089` must list it.

8. **Before going on air.** `--check` prints the plan without opening the radio,
   and the supply and the temperature must be sane:

   ```bash
   ./target/release/decdrm tx station.toml --check    # ..., resampler fast
   vcgencmd get_throttled                             # 0x0, not 0x50005
   vcgencmd measure_temp                              # below about 80 C
   ```

   (`vcgencmd` comes with the Pi's Ubuntu image; if it is missing,
   `sudo apt install libraspberrypi-bin`.)

   Keep `sample_rate_hz = 2_000_000`: the plan warns that above about 4 Msps the
   station may not keep real time at all. **Set `resampler = "fast"` in
   `[output.hackrf]`.** Measured on a Raspberry Pi 4 Model B Rev 1.4 (4 cores,
   8 GB, Ubuntu 24.04, kernel 7.3.0-1004-raspi, 1.8 GHz,
   `get_throttled = 0x0`): encoding alone runs about 20 times faster than real
   time (10 s of three xHE-AAC services in 0.52 s with a file output), but
   resampling the 48 kHz baseband to 2 Msps is the bottleneck. With the default
   `"balanced"` the resampler pegs one core (99.6 %), sustains only ~0.5 of real
   time, and a 20 s transmission ends at 18.4 s with four underruns and `the
   HackRF did not take the last of the signal`. With `"fast"` the same 20 s goes
   out cleanly (exit 0) at about 60 % of one core (~1.6 times real time), with a
   1 s and a 10 s buffer alike; three services and one service cost about the
   same, the resampler dominating. `"fast"` (64 taps, linear) is transparent for
   the 9/10 kHz occupancies; the 18 and 20 kHz ones reach the edge of its
   passband, keep `"balanced"` for them. `buffer_s` only delays the problem: the
   deficit is continuous.

   A short test, then the status: `underruns` must stay 0:

   ```bash
   ./target/release/decdrm tx station.toml --duration 10
   ```

9. **systemd.** A unit for a station, with the udev rule's group and the log in
   the journal (running as root is unnecessary). Change the user and the paths to
   your setup:

   ```ini
   [Unit]
   Description=DecDRM station
   After=network-online.target
   Wants=network-online.target

   [Service]
   User=pi
   SupplementaryGroups=plugdev
   WorkingDirectory=/home/pi/DecDRM-fork
   ExecStart=/home/pi/DecDRM-fork/target/release/decdrm tx /home/pi/station.toml
   Restart=on-failure
   RestartSec=5

   [Install]
   WantedBy=multi-user.target
   ```

   `sudo systemctl daemon-reload && sudo systemctl enable --now decdrm`, then
   `journalctl -u decdrm -f`.

   **Without root, or without a service:** `scripts/pi/stationctl` keeps the
   station in a detached `screen` session, so it survives the SSH session (but
   not a reboot):

   ```bash
   scripts/pi/stationctl start station-talk-3          # no output is logged
   scripts/pi/stationctl start --log station-talk-3    # append to station/logs/decdrm-<name>.log
   scripts/pi/stationctl status station-talk-3
   scripts/pi/stationctl logs station-talk-3           # tail -f; Ctrl-C stops the tail only
   scripts/pi/stationctl restart station-talk-3
   scripts/pi/stationctl stop station-talk-3           # SIGINT: the radio drains first
   ```

   Nothing is written unless `start` (or `restart`) is given `--log`; `logs`
   then tails that file. The station files come from `station/` (`STATION_DIR`
   overrides it), the binary from `target/release/decdrm` (`DECDRM_BIN`), and
   the log goes to `station/logs/decdrm-<name>.log`. It needs `screen`
   (`sudo apt install screen`). One station per radio: a second session on the
   same HackRF fails to open it, and its log (if asked for) says so. A station
   that exits with an error is restarted after 10 s (`STATIONCTL_RESTART_S`
   changes that), and `status` counts the restarts. The log is rotated to
   `decdrm-<name>.log.1` when it passes 5 MB (`STATIONCTL_LOG_MAX`), at start and
   at each restart.

   **Clock and network.** The station transmits the time and date (SDC type 8),
   so the clock must be right: `timedatectl set-ntp true`. A web-stream station
   needs a steady network; a wired link is steadier than Wi-Fi.

**Pi-specific problems**

- `vcgencmd get_throttled` is not `0x0` (e.g. `0x50005`): the supply sags. The CPU
  and the radio both suffer, the station underruns; use the official 5 V/3 A
  supply and a heatsink.
- `lsusb` lists the radio but the station says `no HackRF One found` or `the
  HackRF could not be opened`, and `/dev/swradio0` exists: the kernel's `hackrf`
  module took it — blacklist it (step 6).
- The compiler is killed or the card fills up: add swap, build with
  `CARGO_PROFILE_RELEASE_DEBUG=false` and `CARGO_BUILD_JOBS=1`.

## Troubleshooting

- **`alsa-sys`/`pkg-config` cannot find ALSA** — install `libasound2-dev`.
- **libusb build errors on Linux** — install `libusb-1.0-0-dev` (and
  `libudev-dev`).
- **`no HackRF One found`** — the radio is not connected, has no driver (Zadig on
  Windows, udev + `plugdev` on Linux), or is in use by another program.
- **`the HackRF could not be opened`** — close whatever else holds it (the GUI, SDR
  software, a second station); the message carries the driver's reason.
- **GUI: `failed to create GL context`** — install `libgl1-mesa-dri libegl1` and
  make sure `DISPLAY` is set; under a virtual display use
  `LIBGL_ALWAYS_SOFTWARE=1`.
- **The plan rejects `buffer_s`, `sample_rate_hz`, `frequency_hz` or
  `if_offset_hz`** — the message names the allowed range. `decdrm tx file.toml
  --check` prints the plan and the problems without opening the radio.
- **`the HackRF did not take the last of the signal` or `writing to the HackRF
  failed: Operation timed out`** — the station is not keeping real time, or the
  radio wedged after an underrun. The writer restarts the transmitter a few times
  after such a failure (counted as *recoveries* in the status), so one glitch may
  pass — on a Pi 4B a wedged write recovered once and the transmission finished; a
  repeating one still needs its cause fixed. On a Raspberry Pi 4 set
  `resampler = "fast"` in `[output.hackrf]`; keep `sample_rate_hz = 2_000_000`. A
  larger `buffer_s` only postpones the failure.
- **Windows linker `LNK1106`/`LNK1140`** — the disk filled up: set
  `$env:CARGO_PROFILE_RELEASE_DEBUG = 'false'`, free space, and retry with
  `--jobs 1`.
