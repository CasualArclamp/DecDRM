# This fork: where it came from, what it adds, how it is versioned

[DecDRM](https://github.com/CasualArclamp/DecDRM) is developed upstream by Arclamp.
This repository is a fork that adds a **HackRF One transmitter output**. The upstream
project stays the main one, and the fork follows it rather than leaving it behind.

## Where it forked

| | |
|---|---|
| Upstream | `CasualArclamp/DecDRM` |
| Base release | `v0.7.0` — "DecDRM 0.7.0: RSS feeds in Journaline" |
| Base commit | `f387499` |
| First fork commit | `ad380c7` — "HackRF One transmitter output: stream the signal over USB, no libhackrf needed (#1)" |

The receiver, the decoder, the codecs and the rest of the upstream base are kept as
they are. Upstream fixes are merged, not forked away, and the fork's own changes are
kept apart from them (see *Versions* below).

## What the fork adds

Compared with the base (`git diff f387499..HEAD`):

- **The HackRF One output** (`[output.hackrf]`, `crates/decdrm-station/src/hackrf.rs`):
  the radio's USB protocol with no libhackrf, the TX gain and the amplifier, the
  frequency plan (an LO plus an IF offset), a prebuffer queue with a drain at the end
  of the signal, underrun counting, and restarting the transmitter after a wedged bulk
  write (shown as *recoveries* in the status).
- **The resampler setting** (`resampler = "fast" | "balanced" | "high"`): the 48 kHz
  baseband has to be resampled to the radio's sample rate, which is the most expensive
  step on a small machine. The setting trades quality for CPU, and the transmit plan
  warns about the combinations that cannot keep real time.
- **Station and CLI integration**: the HackRF output in the station file, the plan
  check, the status line, the error messages (`decdrm tx`, `--check`) and the on-air
  frequency axis of the spectrum in the GUI.
- **GUI**: the HackRF fields on the transmit page (frequency, gain, amplifier, the
  resampler override) and the output card showing the resampler and the recoveries.
- **`scripts/pi/stationctl`**: running a station from a `screen` session with a log,
  rotation, a restart after an error and a one-line status.
- **`docs/BUILD.md`** (new): the build guide, including a self-contained Raspberry Pi 4
  walkthrough (packages, swap, the kernel module, udev, systemd) and the measured
  numbers for the radio path.
- **Docs, tests and CI** for the above: `docs/USER_GUIDE.md`, `README.md`,
  `.gitignore`, `.github/workflows/ci.yml`, the station's config tests and the
  `hackrf_finish`/`hackrf_drop` tests, and the example
  `crates/decdrm-station/examples/station-tx-hack-rf-radio-example.toml`.

## Versions

The fork numbers its releases **on its own line**, starting at **1.0.0**. It does not
reuse upstream numbers, so a fork build says `decdrm 1.0.0` and cannot be taken for an
upstream one. Each fork release records the upstream release it is based on:

| Fork | Upstream base | What it adds |
|---|---|---|
| 1.0.0 | `v0.7.0` (`f387499`) | the HackRF One output with the resampler setting and the write recovery, the station and GUI integration, `stationctl`, the Pi 4 guide |

The rules:

- The version lives in one place, `[workspace.package] version` in the root
  `Cargo.toml`; the crates and the applications inherit it, and `Cargo.lock` follows.
- Bump the **minor** version when an upstream release is merged (1.1.0 for upstream
  0.8.0), the **patch** version for fixes in the fork's own code. The fork's major
  version does not follow upstream's.
- A release is a "Version X.Y.Z" commit with an annotated tag `vX.Y.Z` on it. The
  release workflow (`.github/workflows/release.yml`) builds the portable Windows
  executables from the tag and refuses to publish when `decdrm --version` does not
  match the tag.
- Merge upstream rather than rebase:

  ```bash
  git remote add upstream https://github.com/CasualArclamp/DecDRM
  git fetch upstream --tags
  git merge upstream/main
  ```

  Then bump the version and add a row to the table above.
- The licence and the authorship do not change: the fork is GPL-2.0-or-later like the
  upstream, and the work of the upstream authors is credited in `README.md`.
