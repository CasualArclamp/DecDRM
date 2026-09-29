//! Builds the vendored Fraunhofer FDK-AAC library (`third_party/fdk-aac`, tag v2.0.3)
//! as a static C++ library, plus a small DecDRM shim (`csrc/decdrm_fdk_shim.cpp`).
//!
//! The source list mirrors FDK's own `CMakeLists.txt`: every `lib*/src/*.cpp`
//! (the `arm/`, `mips/` sub-directories are `#include`d by their parents when
//! relevant and must not be compiled on their own), with every `lib*/include`
//! directory on the include path.
//!
//! # Build-time patches (DRM encoder support)
//!
//! The public FDK v2.0.3 *encoder* cannot produce DRM frames: it rejects the
//! 960-sample granule that DRM mandates and has no way to request DRM-style SBR
//! payloads. Instead of forking the submodule, a handful of files are copied to
//! `$OUT_DIR/fdk-patched/` with small, anchored text edits and compiled instead of the
//! originals (see [`PATCHES`]). Every anchor must match exactly once, so an FDK update
//! that moves the code fails the build loudly instead of silently mis-patching.
//! In the words of the FDK licence, the result is a "Third-Party Modified Version of the
//! Fraunhofer FDK AAC Codec Library"; each patched copy carries a notice saying so.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// FDK sub-libraries, as listed in FDK's `CMakeLists.txt`.
const FDK_LIBS: &[&str] = &[
    "libAACdec",
    "libAACenc",
    "libArithCoding",
    "libDRCdec",
    "libFDK",
    "libMpegTPDec",
    "libMpegTPEnc",
    "libPCMutils",
    "libSACdec",
    "libSACenc",
    "libSBRdec",
    "libSBRenc",
    "libSYS",
];

/// One anchored text substitution: `anchor` must occur exactly once in the file.
struct Edit {
    anchor: &'static str,
    replacement: &'static str,
}

struct Patch {
    /// Path relative to the FDK root.
    file: &'static str,
    /// Why the file is patched (copied into the notice at the top of the patched copy).
    why: &'static str,
    edits: &'static [Edit],
}

/// DecDRM's modifications to FDK. All of them only widen what the *encoder* accepts;
/// the decoder is compiled unmodified.
const PATCHES: &[Patch] = &[
    Patch {
        file: "libAACenc/src/aacenc_lib.cpp",
        why: "accept AACENC_GRANULE_LENGTH=960 and a private parameter (0x0F01) \
              that selects DRM SBR payload syntax",
        edits: &[
            // 1. Allow the 960-sample granule (DRM / DAB+ frame length).
            Edit {
                anchor: "        switch (value) {\n          case 1024:\n          case 512:\n",
                replacement: "        switch (value) {\n          case 1024:\n          case 960: /* DecDRM */\n          case 512:\n",
            },
            // 2. Storage for the private DRM-SBR switch (USER_PARAM is memcleared, so it
            //    defaults to 0 = off).
            Edit {
                anchor: "  UINT userDownscaleFactor;\n\n} USER_PARAM;",
                replacement: "  UINT userDownscaleFactor;\n\n  UINT userDecdrmDrmSbr; /* DecDRM: 1 = DRM SBR payload syntax */\n\n} USER_PARAM;",
            },
            // 3. Propagate the switch as AC_DRM in the syntax flags. In GA (non-ER) mode
            //    AC_DRM has no effect on the core bitstream writer; the (patched) SBR
            //    encoder picks it up in sbrEncoder_Init().
            Edit {
                anchor: "  hAacConfig->syntaxFlags = 0;\n  hAacConfig->epConfig = -1;\n",
                replacement: "  hAacConfig->syntaxFlags = 0;\n  hAacConfig->epConfig = -1;\n  if (config->userDecdrmDrmSbr) {\n    hAacConfig->syntaxFlags |= AC_DRM; /* DecDRM */\n  }\n",
            },
            // 4. The setter for the private parameter.
            Edit {
                anchor: "    default:\n      err = AACENC_UNSUPPORTED_PARAMETER;\n      break;\n  } /* switch(param) */",
                replacement: "    case (AACENC_PARAM)0x0F01: /* DecDRM: DRM SBR payload syntax */\n      if (value > 1) {\n        err = AACENC_INVALID_CONFIG;\n        break;\n      }\n      if (settings->userDecdrmDrmSbr != value) {\n        settings->userDecdrmDrmSbr = value;\n        hAacEncoder->InitFlags |=\n            AACENC_INIT_CONFIG | AACENC_INIT_STATES | AACENC_INIT_TRANSPORT;\n      }\n      break;\n    default:\n      err = AACENC_UNSUPPORTED_PARAMETER;\n      break;\n  } /* switch(param) */",
            },
            // 5. ...and its getter.
            Edit {
                anchor: "    case AACENC_PEAK_BITRATE:\n      value = (UINT)-1; /* peak bitrate parameter is meaningless */\n",
                replacement: "    case (AACENC_PARAM)0x0F01: /* DecDRM */\n      value = settings->userDecdrmDrmSbr;\n      break;\n    case AACENC_PEAK_BITRATE:\n      value = (UINT)-1; /* peak bitrate parameter is meaningless */\n",
            },
            // 6. Advertise the 960 granule in the capability flags.
            Edit {
                anchor: "  info[i].flags = 0 | CAPF_AAC_1024 | CAPF_AAC_LC | CAPF_AAC_512 |\n                  CAPF_AAC_480 | CAPF_AAC_DRC | CAPF_AAC_ELD_DOWNSCALE;",
                replacement: "  info[i].flags = 0 | CAPF_AAC_1024 | CAPF_AAC_LC | CAPF_AAC_512 |\n                  CAPF_AAC_480 | CAPF_AAC_DRC | CAPF_AAC_ELD_DOWNSCALE |\n                  CAPF_AAC_960 /* DecDRM */;",
            },
        ],
    },
    Patch {
        file: "libAACenc/src/aacenc.cpp",
        why: "accept a 960-sample frame length in FDKaacEnc_Initialize()",
        edits: &[Edit {
            anchor: "  switch (config->framelength) {\n    case 1024:\n      if (isLowDelay(config->audioObjectType)) {",
            replacement: "  switch (config->framelength) {\n    case 1024:\n    case 960: /* DecDRM */\n      if (isLowDelay(config->audioObjectType)) {",
        }],
    },
    Patch {
        file: "libAACenc/src/psy_configuration.cpp",
        why: "psychoacoustic Bark mapping and PE-per-window scaling for 960/120-line \
              transforms (the originals only handle 1024/128/512/480 and silently fall \
              back to 0)",
        edits: &[
            // Bark value of an MDCT line: center frequency in Q13 = line * fs / (2 * N).
            //   N = 960: fMult(line*fs, 1/480 in Q39) is line*fs/480 in Q8 = f in Q10 -> << 3
            //   N = 120: the same product is f/2 in Q8 = f in Q7              -> << 6
            Edit {
                anchor: "    case 480:\n      center_freq = fMult(center_freq, INV480) << 4;  // q13\n      break;\n",
                replacement: "    case 480:\n      center_freq = fMult(center_freq, INV480) << 4;  // q13\n      break;\n    case 960: /* DecDRM */\n      center_freq = fMult(center_freq, INV480) << 3;  // q13\n      break;\n    case 120: /* DecDRM */\n      center_freq = fMult(center_freq, INV480) << 6;  // q13\n      break;\n",
            },
            // Perceptual entropy per window scales with the number of lines:
            // 960 = 1024 * 15/16 and 120 = 128 * 15/16.
            Edit {
                anchor: "    case 480:\n      qperwin = qperwin - 9;\n      pePerWindow = fMult(pePerWindow, FL2FXCONST_DBL(480.f / 512.f));\n      break;\n",
                replacement: "    case 480:\n      qperwin = qperwin - 9;\n      pePerWindow = fMult(pePerWindow, FL2FXCONST_DBL(480.f / 512.f));\n      break;\n    case 960: /* DecDRM */\n      qperwin = qperwin - 10;\n      pePerWindow = fMult(pePerWindow, FL2FXCONST_DBL(960.f / 1024.f));\n      break;\n    case 120: /* DecDRM */\n      qperwin = qperwin - 7;\n      pePerWindow = fMult(pePerWindow, FL2FXCONST_DBL(120.f / 128.f));\n      break;\n",
            },
        ],
    },
    Patch {
        file: "libSBRenc/src/sbr_encoder.cpp",
        why: "when the AAC encoder signals AC_DRM, emit SBR payloads in DRM syntax \
              (scalable mono syntax plus the 8-bit DRM SBR CRC that FDK already implements \
              but never enables)",
        edits: &[
            Edit {
                anchor: "      /* initialize SBR element, and get core bandwidth */\n      error = FDKsbrEnc_EnvInit(",
                replacement: "      if (syntaxFlags & AC_DRM) {\n        sbrConfig[el].crcSbr = 2; /* DecDRM: DRM SBR syntax + DRM CRC */\n      }\n\n      /* initialize SBR element, and get core bandwidth */\n      error = FDKsbrEnc_EnvInit(",
            },
            Edit {
                anchor: "  if (params->crcSbr) {\n    hSbrElement->sbrConfigData.sbrSyntaxFlags |= SBR_SYNTAX_CRC;\n  }\n",
                replacement: "  if (params->crcSbr) {\n    hSbrElement->sbrConfigData.sbrSyntaxFlags |= SBR_SYNTAX_CRC;\n  }\n  if (params->crcSbr == 2) { /* DecDRM */\n    hSbrElement->sbrConfigData.sbrSyntaxFlags |=\n        SBR_SYNTAX_SCALABLE | SBR_SYNTAX_DRM_CRC;\n  }\n",
            },
        ],
    },
    Patch {
        file: "libSBRenc/src/env_bit.cpp",
        why: "byte-align DRM-syntax SBR payloads so that the GA fill element that carries \
              them out of the encoder stays well formed (the padding follows the CRC \
              region, so the DRM SBR CRC is unaffected)",
        edits: &[Edit {
            anchor: "    FDKwriteBits(&hCmonData->tmpWriteBitbuf, FDKcrcGetCRC(hCrcInfo) ^ 0xFF,\n                 SI_SBR_DRM_CRC_BITS);\n  } else {",
            replacement: "    FDKwriteBits(&hCmonData->tmpWriteBitbuf, FDKcrcGetCRC(hCrcInfo) ^ 0xFF,\n                 SI_SBR_DRM_CRC_BITS);\n    { /* DecDRM: 4 = extension_type nibble of the fill element */\n      int sbrLoad = SI_SBR_DRM_CRC_BITS + hCmonData->sbrHdrBits +\n                    hCmonData->sbrDataBits + 4;\n      hCmonData->sbrFillBits = (8 - (sbrLoad % 8)) % 8;\n      FDKwriteBits(&hCmonData->sbrBitbuf, 0, hCmonData->sbrFillBits);\n    }\n  } else {",
        }],
    },
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let fdk_root = manifest_dir.join("../../third_party/fdk-aac");
    let fdk_root = strip_verbatim(
        fdk_root
            .canonicalize()
            .unwrap_or_else(|_| panic!("FDK-AAC sources not found at {}", fdk_root.display())),
    );
    if !fdk_root
        .join("libAACdec/include/aacdecoder_lib.h")
        .is_file()
    {
        panic!(
            "FDK-AAC submodule at {} is empty; run `git submodule update --init third_party/fdk-aac`",
            fdk_root.display()
        );
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // Directory-level change tracking (one line per sub-library instead of ~170 files).
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    for lib in FDK_LIBS {
        println!(
            "cargo:rerun-if-changed={}",
            fdk_root.join(lib).join("src").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            fdk_root.join(lib).join("include").display()
        );
    }

    let patched = apply_patches(&fdk_root, &out_dir.join("fdk-patched"));

    let mut sources: Vec<PathBuf> = Vec::new();
    for lib in FDK_LIBS {
        let src_dir = fdk_root.join(lib).join("src");
        let mut files: Vec<PathBuf> = fs::read_dir(&src_dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", src_dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "cpp"))
            .collect();
        files.sort();
        for f in files {
            let rel = f.strip_prefix(&fdk_root).expect("path under FDK root");
            let rel = rel.to_string_lossy().replace('\\', "/");
            match patched.iter().find(|(orig, _)| *orig == rel) {
                Some((_, copy)) => sources.push(copy.clone()),
                None => sources.push(f),
            }
        }
    }
    assert!(
        sources.len() >= 170,
        "unexpectedly few FDK sources ({})",
        sources.len()
    );

    let mut build = cc::Build::new();
    build.cpp(true).warnings(false).cargo_warnings(false);
    for lib in FDK_LIBS {
        let inc = fdk_root.join(lib).join("include");
        if inc.is_dir() {
            build.include(inc);
        }
    }
    // Only needed by the DecDRM shim (encoder Huffman tables in aacEnc_rom.h). Added
    // last so it can never shadow a header that FDK itself resolves elsewhere.
    build.include(fdk_root.join("libAACenc/src"));

    // FDK asserts are plain `assert()` on x86; a corrupt bitstream must never abort the
    // process, so build like CMake's Release configuration does.
    build.define("NDEBUG", None);

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        // Same as FDK's CMakeLists; /utf-8 because the licence headers contain "©".
        build.flag("/EHsc").flag("/utf-8");
    } else {
        build.flag("-fno-exceptions").flag("-fno-rtti");
    }

    // The codec is unusably slow unoptimised; always build it optimised, but keep the
    // profile's setting when it is already higher.
    let opt: u32 = env::var("OPT_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    build.opt_level(opt.max(2));

    build.files(&sources);
    build.file(manifest_dir.join("csrc/decdrm_fdk_shim.cpp"));
    build.compile("fdk-aac");
}

/// Writes the patched copies and returns `(relative original path, patched copy)` pairs.
fn apply_patches(fdk_root: &Path, patch_root: &Path) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    for patch in PATCHES {
        let orig = fdk_root.join(patch.file);
        let text = fs::read_to_string(&orig)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", orig.display()))
            // The submodule may be checked out with CRLF (core.autocrlf); anchors use LF.
            .replace("\r\n", "\n");
        let mut text = text;
        for (i, edit) in patch.edits.iter().enumerate() {
            let count = text.matches(edit.anchor).count();
            if count != 1 {
                panic!(
                    "DecDRM FDK patch #{i} for {} matched {count} times (expected exactly once); \
                     the FDK sources changed — review crates/decdrm-fdk-sys/build.rs",
                    patch.file
                );
            }
            text = text.replacen(edit.anchor, edit.replacement, 1);
        }
        let notice = format!(
            "/* DecDRM: Third-Party Modified Version of the Fraunhofer FDK AAC Codec Library.\n \
             * This file was changed by crates/decdrm-fdk-sys/build.rs (2026-09-29) to\n \
             * {}.\n * Changed lines are marked \"DecDRM\". */\n",
            patch.why
        );
        // Mirror the original directory's headers next to the copy so that quoted
        // includes (`#include "foo.h"`) still resolve relative to the source file.
        let rel_dir = Path::new(patch.file)
            .parent()
            .expect("patched file has a directory");
        let dst_dir = patch_root.join(rel_dir);
        fs::create_dir_all(&dst_dir).expect("create patch dir");
        let src_dir = fdk_root.join(rel_dir);
        for entry in fs::read_dir(&src_dir).expect("read src dir").flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|x| x == "h") {
                fs::copy(&p, dst_dir.join(p.file_name().expect("file name"))).expect("copy header");
            }
        }
        let dst = dst_dir.join(Path::new(patch.file).file_name().expect("file name"));
        write_if_changed(&dst, &(notice + &text));
        result.push((patch.file.to_string(), dst));
    }
    result
}

/// `canonicalize()` yields `\\?\C:\...` on Windows, which `cl.exe` cannot open; drop the
/// verbatim prefix (the paths involved are short, ordinary drive paths).
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// Avoids touching the patched file (and so forcing a recompile) when nothing changed.
fn write_if_changed(path: &Path, contents: &str) {
    if fs::read_to_string(path)
        .map(|old| old == contents)
        .unwrap_or(false)
    {
        return;
    }
    fs::write(path, contents).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
}
