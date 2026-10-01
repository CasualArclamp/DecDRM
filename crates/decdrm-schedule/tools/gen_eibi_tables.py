"""Generate crates/decdrm-schedule/src/eibi_tables.rs from Dream's
src/tables/TableStations.cpp (EiBi's language, country, target-area and
transmitter-site codes).

    python crates/decdrm-schedule/tools/gen_eibi_tables.py \
        reference/dream-mjf/src/tables/TableStations.cpp \
        crates/decdrm-schedule/src/eibi_tables.rs

Reproduces the std::map semantics of Dream's CStationData: a later duplicate key
overwrites an earlier one, and every target code is also entered with the prefixes
C/N/S/E/W ("Central "/"North "/...) in the array's order (so e.g. the base code
"NNE" overwrites the composed "N" + "NE"). The output is sorted by key (byte
order) for binary search and marked #[rustfmt::skip] to stay one entry per line.
"""
import re
import sys

src_path, out_path = sys.argv[1], sys.argv[2]
text = open(src_path, encoding="utf-8").read()

STR = r'"((?:[^"\\]|\\.)*)"'


def unescape_c(s):
    out, i = [], 0
    while i < len(s):
        c = s[i]
        if c == "\\":
            n = s[i + 1]
            out.append({"n": "\n", "t": "\t", "\\": "\\", '"': '"', "'": "'"}[n])
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def array(name, fields):
    """The rows of the C array `name[] = { { "a", "b" }, ..., { 0, 0 } }`."""
    start = text.index(name + "[] = {")
    end = text.index("{ 0, 0", start)
    rows = []
    for line in text[start:end].splitlines()[1:]:
        line = line.strip()
        if not line.startswith("{"):
            continue
        vals = [unescape_c(v) for v in re.findall(STR, line)]
        assert len(vals) == fields, (name, line)
        rows.append(vals)
    return rows


langs = {}
for code, lang in array("eibi_langs", 2):
    langs[code] = lang
countries = {}
for code, country in array("itu_r_countries", 2):
    countries[code] = country
targets = {}
for code, target in array("eibi_targets", 2):
    targets[code] = target
    for p, w in (("C", "Central "), ("N", "North "), ("S", "South "), ("E", "East "), ("W", "West ")):
        targets[p + code] = w + target
sites = {}
for country, mark, _bc, site in array("eibi_stations", 4):
    sites[(country, mark)] = site

for d in (langs, countries, targets):
    assert all(k.isascii() for k in d), "keys must be ASCII for byte-order binary search"
assert all(c.isascii() and m.isascii() for c, m in sites), "site keys must be ASCII"


def emit_pairs(name, doc, d):
    lines = [f"/// {doc}", "#[rustfmt::skip]", f"pub(crate) static {name}: &[(&str, &str)] = &["]
    for k in sorted(d, key=lambda s: s.encode()):
        lines.append(f"    ({rust_str(k)}, {rust_str(d[k])}),")
    lines.append("];")
    return "\n".join(lines)


def emit_sites(d):
    lines = [
        "/// Transmitter sites: (ITU country of the site, site code, name and coordinates),",
        "/// sorted by (country, code). An empty code is the country's main site.",
        "#[rustfmt::skip]",
        "pub(crate) static SITES: &[(&str, &str, &str)] = &[",
    ]
    for k in sorted(d, key=lambda t: (t[0].encode(), t[1].encode())):
        lines.append(f"    ({rust_str(k[0])}, {rust_str(k[1])}, {rust_str(d[k])}),")
    lines.append("];")
    return "\n".join(lines)


header = """//! EiBi code tables: languages, ITU country codes, target areas and transmitter sites.
//!
//! **Generated** by `tools/gen_eibi_tables.py` from Dream's
//! `src/tables/TableStations.cpp` (dream-mjf r1548, refreshed there from EiBi's lists —
//! "21 July 26 data" in Dream's ChangeLog; GPL-2.0-or-later like DecDRM), reproducing
//! Dream's `CStationData` maps: a later duplicate key replaces an earlier one, and each
//! target code is also entered with the prefixes C/N/S/E/W ("Central ", "North ", …).
//! Sorted by key (byte order) for binary search; do not edit by hand.

// Rust note: `&[(&str, &str)]` is a slice of string pairs stored in the executable's
// read-only data; nothing is allocated at run time.
"""

parts = [
    header,
    emit_pairs("LANGUAGES", "EiBi language codes (`Lng` column) and their names.", langs),
    "",
    emit_pairs("COUNTRIES", "ITU country codes (`ITU` column, site prefixes) and their names.", countries),
    "",
    emit_pairs("TARGETS", "EiBi target-area codes (`Target` column) and their names.", targets),
    "",
    emit_sites(sites),
    "",
]
with open(out_path, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(parts))
print(f"languages {len(langs)}, countries {len(countries)}, targets {len(targets)}, sites {len(sites)}")
