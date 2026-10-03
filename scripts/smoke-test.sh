#!/usr/bin/env bash
# Smoke test of the DecDRM executables, for CI and the release workflow (Linux, or Git
# Bash on Windows):
#
#   scripts/smoke-test.sh BIN_DIR OUT_DIR [--dac]
#
# BIN_DIR holds decdrm and decdrm-gui. The CLI transmits the example station (HE-AAC
# audio, Journaline, EPG) and receives it, and the GUI decodes the same file and saves
# a screenshot: headless under xvfb-run on Linux; on Windows it needs an OpenGL driver
# (Mesa on GitHub's runners). --dac: a DAC station too, received in an empty directory
# without DECDRM_MODELS, so the executables need the weights built in (feature
# embed-weights) and no models directory may lie up to four levels above BIN_DIR (see
# crates/decdrm-dac/src/weights.rs). OUT_DIR receives the receiver's output, the data
# objects and the screenshot.
set -euo pipefail

if [ $# -lt 2 ]; then
    echo "usage: $0 BIN_DIR OUT_DIR [--dac]" >&2
    exit 2
fi
bin=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
shift 2
dac=false
for arg in "$@"; do
    case $arg in
        --dac) dac=true ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
cd "$work"
unset DECDRM_MODELS

"$bin/decdrm" --version
"$bin/decdrm-gui" --version
"$bin/decdrm" devices

echo "== HE-AAC, Journaline and EPG: transmit, receive"
"$bin/decdrm" tx "$repo/crates/decdrm-station/examples/station.toml" --duration 20 --status-every 0 --output aac.wav
"$bin/decdrm" rx aac.wav --data-dir "$out/data" | tee "$out/rx_aac.txt"
grep -E "audio .*: [1-9][0-9]* frames ok, 0 concealed" "$out/rx_aac.txt"
ls -R "$out/data"

if $dac; then
    echo "== DAC from the built-in weights: transmit, receive"
    # The 1557 kHz station's layout: mode B, 9 kHz, 16-QAM MSC, 4-QAM SDC, short
    # interleaving, I/Q.
    cat > dac.toml <<'TOML'
[channel]
mode = "B"
occupancy = 2
msc_mode = "16-QAM"
sdc_mode = "4-QAM"
interleaving = "short"
protection_b = 1

[output]
file = "dac.wav"
format = "iq"

[[service]]
label = "Smoke DAC"
id = 0xD0D0A2
language = "English"

[service.audio]
codec = "dac"

[service.audio.input]
tone_hz = 440.0
level_dbfs = -12
TOML
    "$bin/decdrm" tx dac.toml --duration 12 --status-every 0 --output dac.wav
    "$bin/decdrm" rx dac.wav --format iq | tee "$out/rx_dac.txt"
    grep -E "audio DAC.*: [1-9][0-9]* frames ok, 0 concealed" "$out/rx_dac.txt"
fi

# A standard transmitter never makes the receiver distrust the FAC identity.
if grep -h "FAC identity" "$out"/rx_*.txt; then
    echo "the receiver distrusted a standard FAC identity" >&2
    exit 1
fi

echo "== GUI: decode the HE-AAC file, screenshot"
gui=("$bin/decdrm-gui")
if [ "$(uname -s)" = Linux ]; then
    gui=(xvfb-run -a -s "-screen 0 1280x800x24" "$bin/decdrm-gui")
fi
"${gui[@]}" aac.wav --start --no-audio --config gui.toml --exit-after 8 --screenshot "$out/gui.png"
test -s "$out/gui.png"
echo "smoke test passed"
