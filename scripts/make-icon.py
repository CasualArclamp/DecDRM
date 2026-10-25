"""Renders the DecDRM app icon: assets/icon/*.svg -> assets/icon/png/*.png and
assets/icon/decdrm.ico (every size, PNG-compressed entries, read by Windows Vista and
later).

    python scripts/make-icon.py [RESVG]

Needs resvg's command-line renderer (`cargo install resvg`); pass its path if it is not
on PATH. 16 and 24 px come from decdrm-small.svg (drawn on a 16-pixel grid), the larger
sizes from decdrm.svg.
"""

import pathlib
import struct
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
ICON = ROOT / "assets" / "icon"
SIZES = [16, 24, 32, 48, 64, 128, 256]


def render(resvg, size):
    svg = ICON / ("decdrm-small.svg" if size < 32 else "decdrm.svg")
    png = ICON / "png" / f"decdrm-{size}.png"
    subprocess.run([resvg, "-w", str(size), "-h", str(size), str(svg), str(png)], check=True)
    return png.read_bytes()


def ico(images):
    # ICONDIR, then one ICONDIRENTRY per image (width/height 0 means 256), then the data.
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in images:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(data for _, data in images)


def main():
    resvg = sys.argv[1] if len(sys.argv) > 1 else "resvg"
    (ICON / "png").mkdir(exist_ok=True)
    images = [(size, render(resvg, size)) for size in SIZES]
    (ICON / "decdrm.ico").write_bytes(ico(images))
    print("wrote", len(images), "sizes to", ICON)


if __name__ == "__main__":
    main()
