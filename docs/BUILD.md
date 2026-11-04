# Building DecDRM

Step-by-step build instructions for Windows, Ubuntu/Debian and a Raspberry Pi 4.
The user guide ([USER_GUIDE.md](USER_GUIDE.md)) covers running the programs; this
file is only about getting them built.

**What is verified.** CI builds the whole workspace on Linux (x86_64) and Windows,
runs the tests, the lints and the smoke tests there, and additionally builds the CLI
and runs the station's unit tests on **aarch64** — the Raspberry Pi's architecture, on
a GitHub-hosted arm64 runner (not a Pi itself). The Pi-specific advice below (powered
hub, throttling, systemd) is still a guide rather than a CI step.

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
   .\target\release\decdrm.exe tx stations-local\station.toml --check
   .\target\release\decdrm.exe tx stations-local\station.toml
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

Use a **64-bit** OS: Raspberry Pi OS Bookworm (64-bit) or Ubuntu 24.04 arm64. The
GUI does not build on a 32-bit userland, and the Rust support for 32-bit ARM is
thin.

1. Install the packages from the Ubuntu section above (ALSA, libusb, udev, CMake,
   pkg-config) and Rust with rustup.
2. **Build on the Pi.** The C++ codecs are the memory hogs; on a 2–4 GB Pi limit
   the jobs and be patient (the first build takes a long time):

   ```bash
   CARGO_BUILD_JOBS=1 cargo build --release -p decdrm-cli
   ```

   Add swap if the compiler is killed. `decdrm-gui` additionally needs Rust 1.95
   and a desktop session to run; a headless station only needs `decdrm`.

3. **Or cross-compile on a PC** (needs Docker there; CI builds on a native arm64 runner instead, so this route is not exercised):

   ```bash
   cargo install cross
   cross build --release --target aarch64-unknown-linux-gnu -p decdrm-cli
   ```

4. **HackRF.** Power the radio through a powered USB hub (it draws about 500 mA);
   `lsusb` must list `1d50:6089`. Install the udev rule and the `plugdev` group as
   in the Ubuntu section.
5. **Run** `decdrm tx station.toml` (read the plan first with `--check`). Keep
   `sample_rate_hz = 2_000_000`: the plan warns that above about 4 Msps the station
   may not keep real time, and a Pi 4 is slower than the desktop where that was
   measured. Watch the status's `underruns` (it should stay 0) and
   `vcgencmd measure_temp` / `vcgencmd get_throttled` — a throttled Pi drops
   samples.
6. A systemd unit for a station on the Pi, as a sketch:

   ```ini
   [Unit]
   Description=DecDRM station
   After=network-online.target
   Wants=network-online.target

   [Service]
   ExecStart=/home/pi/DecDRM-fork/target/release/decdrm tx /home/pi/station.toml
   Restart=on-failure
   RestartSec=5

   [Install]
   WantedBy=multi-user.target
   ```

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
- **Windows linker `LNK1106`/`LNK1140`** — the disk filled up: set
  `$env:CARGO_PROFILE_RELEASE_DEBUG = 'false'`, free space, and retry with
  `--jobs 1`.
