//! Builds the *encoder* half of the vendored libxaac library (`third_party/libxaac`, tag
//! v0.1.13, Apache-2.0) as a static C library, plus a small DecDRM shim
//! (`csrc/decdrm_xaac_shim.c`).
//!
//! The source list mirrors libxaac's own CMake files: `encoder/libxaacenc.cmake` (every
//! `encoder/*.c`), `encoder/drc_src/libxaacenc_drc.cmake` (every `encoder/drc_src/*.c`)
//! and the three shared files of `common/common.cmake`. The decoder, the test benches and
//! the fuzzers are not compiled. `LOUDNESS_LEVELING_SUPPORT` is defined for every file,
//! as libxaac's top-level CMake does globally (it changes the layout of the DRC structs,
//! so the shim must see the same definition).
//!
//! # Build-time patches
//!
//! libxaac v0.1.13 cannot be used for DRM as released (see [`PATCHES`] for the details):
//! its USAC configuration check replaces every bit rate other than 64 and 96 kbit/s by
//! 96 kbit/s and refuses stereo below 16 kbit/s; its USAC core always codes the whole core
//! band and places its threshold in quiet below the 16-bit noise floor, which wrecks low
//! rates; it rejects the DRM core rates 9.6/19.2 kHz; and it offers no way to learn the
//! exact bit length of a frame or to request an independent frame. Instead of forking the
//! submodule, the affected files are copied to `$OUT_DIR/xaac-patched/` with small,
//! anchored text edits and compiled instead of the originals. Every anchor must match
//! exactly once, so a libxaac update that moves the code fails the build loudly instead
//! of silently mis-patching. As the Apache License 2.0 (§4 b) requires, each patched copy
//! carries a prominent notice saying that it was changed; changed lines are marked
//! "DecDRM".

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// One anchored text substitution: `anchor` must occur exactly once in the file.
struct Edit {
    anchor: &'static str,
    replacement: &'static str,
}

struct Patch {
    /// Path relative to the libxaac root.
    file: &'static str,
    /// Why the file is patched (copied into the notice at the top of the patched copy).
    why: &'static str,
    edits: &'static [Edit],
}

/// DecDRM's modifications to libxaac. Edits 1–3, 5 and 7 widen what the USAC encoder
/// accepts or expose state; 4 (with 6) and 8 change coding decisions (core bandwidth,
/// threshold in quiet) and were validated with the round-trip sweep in
/// `crates/decdrm-codecs/tests/xhe_roundtrip.rs` (FDK-AAC decoding every DRM rate, SBR
/// ratio and 8–64 kbit/s).
const PATCHES: &[Patch] = &[
    Patch {
        file: "encoder/ixheaace_api.c",
        why: "accept every USAC bit rate, disable the encoder's own USAC fill element, preset \
              the core bandwidth (limited to the SBR crossover), and add accessor functions \
              (frame bit length, core bandwidth, independency control) used by the DRM \
              access-unit converter of decdrm-codecs",
        edits: &[
            // 1. v0.1.13 silently replaces every USAC bit rate except 64000/96000 by the
            //    default (96000), which makes the encoder useless for DRM's 8..64 kbit/s
            //    streams. Keep the default only when no rate is given; the range clamps that
            //    follow still apply.
            Edit {
                anchor: "    if (pstr_input_config->i_bitrate != 64000 && pstr_input_config->i_bitrate != 96000) {\n      pstr_input_config->i_bitrate = USAC_BITRATE_DEFAULT_VALUE;\n    }\n",
                replacement: "    if (pstr_input_config->i_bitrate <= 0) { /* DecDRM: accept every rate */\n      pstr_input_config->i_bitrate = USAC_BITRATE_DEFAULT_VALUE;\n    }\n",
            },
            // 2. The USAC branch clamps the rate to at least 8000 bit/s *per channel*, which
            //    rules out stereo DRM streams below ~16.5 kbit/s (and mono below ~8.4 kbit/s
            //    once the DRM framing overhead is subtracted). Lower the floor for USAC only
            //    (the anchor includes the next line to skip the identical AAC clamp); DecDRM
            //    applies its own, tested limits.
            Edit {
                anchor: "      if (pstr_input_config->i_bitrate < MINIMUM_BITRATE * pstr_input_config->i_channels) {\n        pstr_input_config->i_bitrate = MINIMUM_BITRATE * pstr_input_config->i_channels;\n      }\n      if (pstr_input_config->ccfl_idx == NO_SBR_CCFL_768 ||\n",
                replacement: "      if (pstr_input_config->i_bitrate < 4000) { /* DecDRM: was 8000 per channel */\n        pstr_input_config->i_bitrate = 4000;\n      }\n      if (pstr_input_config->ccfl_idx == NO_SBR_CCFL_768 ||\n",
            },
            // 3. No USAC fill element from libxaac: DecDRM strips the leading AudioPreRoll
            //    element from every frame and appends its own fill element (sized for the
            //    DRM channel), which requires the frame to end with the channel element.
            Edit {
                anchor: "    pstr_usac_config->use_fill_element = USAC_FILL_ELEMENT_DEFAULT_VALUE;\n",
                replacement: "    pstr_usac_config->use_fill_element = 0; /* DecDRM: own fill element */\n",
            },
            // 4. Core bandwidth. In USAC mode libxaac codes the core up to min(20 kHz, core
            //    rate / 2) whatever the bit rate, and ignores the SBR crossover frequency
            //    that ixheaace_env_open() stores in aac_config.band_width. The lines above
            //    the crossover are discarded by every SBR decoder, and at low rates the
            //    upper bands eat the bits of the loud ones: below ≈0.6 bit per coded line
            //    and channel the decoded audio collapses (tones 10-50 dB too low, broadband
            //    noise). So (a) the otherwise unused input field aac_config.bandwidth
            //    presets the USAC bandwidth (0 = automatic) and (b) with SBR the core
            //    bandwidth never exceeds the crossover. Patch 6 (iusace_enc_main.c) makes
            //    iusace_bw_init() respect the preset.
            Edit {
                anchor: "    pstr_usac_config->sbr_inter_tes_active = pstr_input_config->inter_tes_active;\n",
                replacement: r#"    pstr_usac_config->sbr_inter_tes_active = pstr_input_config->inter_tes_active;
    { /* DecDRM: caller's core bandwidth preset (0 = automatic) */
      WORD32 i_bw;
      for (i_bw = 0; i_bw < USAC_MAX_ELEMENTS; i_bw++) {
        pstr_usac_config->bw_limit[i_bw] = pstr_input_config->aac_config.bandwidth;
      }
    }
"#,
            },
            Edit {
                anchor: "  error = iusace_enc_init(pstr_usac_config, &pstr_api_struct->pstr_state->audio_specific_config,\n                          &pstr_api_struct->pstr_state->str_usac_enc_data);\n",
                replacement: r#"  if (pstr_usac_config->sbr_enable && pstr_api_struct->config[0].aac_config.band_width > 0) {
    /* DecDRM: the core never codes above the SBR crossover */
    WORD32 i_bw, xover = pstr_api_struct->config[0].aac_config.band_width;
    for (i_bw = 0; i_bw < USAC_MAX_ELEMENTS; i_bw++) {
      if (pstr_usac_config->bw_limit[i_bw] <= 0 || pstr_usac_config->bw_limit[i_bw] > xover) {
        pstr_usac_config->bw_limit[i_bw] = xover;
      }
    }
  }
  error = iusace_enc_init(pstr_usac_config, &pstr_api_struct->pstr_state->audio_specific_config,
                          &pstr_api_struct->pstr_state->str_usac_enc_data);
"#,
            },
            // 5. Accessors appended after the last API function.
            Edit {
                anchor: "IA_ERRORCODE ixheaace_delete(pVOID pv_output) {\n  IXHEAACE_MEM_FREE(pv_output);\n  return IA_NO_ERROR;\n}\n",
                replacement: r#"IA_ERRORCODE ixheaace_delete(pVOID pv_output) {
  IXHEAACE_MEM_FREE(pv_output);
  return IA_NO_ERROR;
}

/* ---- DecDRM additions ------------------------------------------------------------ */

/* Exact length in bits of the UsacFrame() written by the last ixheaace_process() call,
 * before byte alignment and before ixheaace_write_audio_preroll_data() wrapped it into
 * an AudioPreRoll() frame. Also valid for the start-up frames that the library withholds
 * (i_out_bytes == 0): their bytes are still at the start of the output buffer. */
WORD32 decdrm_ixheaace_frame_bits(pVOID pv_api_obj) {
  ixheaace_api_struct *pstr_api = (ixheaace_api_struct *)pv_api_obj;
  return pstr_api->pstr_state->i_out_bits;
}

/* Number of pre-roll frames: the frame with this 0-based index is the first one the
 * library outputs, wrapped into an AudioPreRoll() that carries the frames before it. */
WORD32 decdrm_ixheaace_num_preroll_frames(pVOID pv_api_obj) {
  ixheaace_api_struct *pstr_api = (ixheaace_api_struct *)pv_api_obj;
  return pstr_api->config[0].usac_config.num_preroll_frames;
}

/* Core coder bandwidth (Hz) of the channel element after initialisation. */
WORD32 decdrm_ixheaace_core_bandwidth(pVOID pv_api_obj) {
  ixheaace_api_struct *pstr_api = (ixheaace_api_struct *)pv_api_obj;
  return pstr_api->config[0].usac_config.bw_limit[0];
}

/* Number of USAC frames encoded so far. */
WORD32 decdrm_ixheaace_frame_count(pVOID pv_api_obj) {
  ixheaace_api_struct *pstr_api = (ixheaace_api_struct *)pv_api_obj;
  return pstr_api->pstr_state->str_usac_enc_data.frame_count;
}

/* Selects usacIndependencyFlag for the next ixheaace_process() call without starting an
 * AudioPreRoll() (IPF) sequence: ixheaace_process() sets the flag when
 * iframes_interval <= num_preroll_frames and starts an IPF only when the two are equal.
 * Call it only after the start-up frames (frame_count > num_preroll_frames). */
VOID decdrm_ixheaace_set_next_independency(pVOID pv_api_obj, WORD32 independent) {
  ixheaace_api_struct *pstr_api = (ixheaace_api_struct *)pv_api_obj;
  ia_usac_encoder_config_struct *pstr_cfg = &pstr_api->config[0].usac_config;
  if (independent) {
    pstr_cfg->iframes_interval = (pstr_cfg->num_preroll_frames > 0) ? 0 : -1;
  } else {
    pstr_cfg->iframes_interval = pstr_cfg->num_preroll_frames + 1;
  }
}
"#,
            },
        ],
    },
    Patch {
        file: "encoder/iusace_enc_main.c",
        why: "let iusace_bw_init() keep a core bandwidth preset by ixheaace_api.c (see patch 4 \
              in crates/decdrm-xaac-sys/build.rs)",
        edits: &[
            // 6. Respect a preset bandwidth instead of always starting from 20 kHz.
            Edit {
                anchor: "  ptr_usac_config->bw_limit[ele_idx] = 20000;\n",
                replacement: "  if (ptr_usac_config->bw_limit[ele_idx] <= 0 || ptr_usac_config->bw_limit[ele_idx] > 20000) {\n    ptr_usac_config->bw_limit[ele_idx] = 20000; /* DecDRM: keep a preset bandwidth */\n  }\n",
            },
        ],
    },
    Patch {
        file: "encoder/iusace_tns_usac.c",
        why: "select the TNS tables of a non-standard core sampling rate (9.6, 14.4, 19.2 kHz) \
              through the standard rate mapping, as libxaac's scale factor band tables and \
              every decoder already do",
        edits: &[
            // 7. iusace_tns_init() looked the core rate up in the list of the twelve MPEG-4
            //    rates verbatim and failed with IA_EXHEAACE_INIT_FATAL_USAC_INVALID_CORE_
            //    SAMPLE_RATE for the DRM rates 9.6 and 19.2 kHz (and the 9.6 kHz core of
            //    38.4 kHz 4:1 SBR). The mapping (ISO/IEC 14496-3 table 4.82) is only used
            //    for the table index; the frequency computations keep the real rate.
            Edit {
                anchor: "  while (sampling_rate != iusace_tns_supported_sampling_rates[fs_index]) {\n",
                replacement: "  WORD32 iusace_map_sample_rate(WORD32 sample_rate); /* DecDRM */\n  while (iusace_map_sample_rate(sampling_rate) != iusace_tns_supported_sampling_rates[fs_index]) {\n",
            },
        ],
    },
    Patch {
        file: "encoder/iusace_psy_utils.c",
        why: "raise the USAC threshold in quiet by 15 dB, above the quantisation noise of the \
              16-bit PCM input",
        edits: &[
            // 8. In libxaac's MDCT scaling the USAC threshold in quiet lies about 4-5 dB
            //    *below* the quantisation noise of the 16-bit input, so (nearly) empty bands
            //    - the input's own quantisation noise, distant window leakage - are coded
            //    with ±1 values, and at low rates the rate loop coarsens the loud bands
            //    instead. With 24-32 kHz cores (48 kHz with SBR, 24/32 kHz without) the
            //    decoded audio collapsed; in the xhe_roundtrip sweep 219 of 259 DRM
            //    configurations were usable before and 243 after (the rest are invalid or
            //    extreme). +15 dB puts the floor ~10 dB above the 16-bit noise; the shape
            //    of the (flat, bark-based) curve is unchanged.
            Edit {
                anchor: "    ptr_thr_quiet[i] = (FLOAT32)pow(10.0f, (bark_thr_quiet - 20.0f) * (FLOAT32)0.1f) * 16887.8f *\n                       (FLOAT32)(ptr_sfb_offset[i + 1] - ptr_sfb_offset[i]);\n",
                replacement: "    ptr_thr_quiet[i] = (FLOAT32)pow(10.0f, (bark_thr_quiet - 20.0f) * (FLOAT32)0.1f) * 16887.8f *\n                       (FLOAT32)(ptr_sfb_offset[i + 1] - ptr_sfb_offset[i]);\n    ptr_thr_quiet[i] *= 31.6227766f; /* DecDRM: +15 dB, above the 16-bit PCM noise */\n",
            },
        ],
    },
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.join("../../third_party/libxaac");
    let root = strip_verbatim(
        root.canonicalize()
            .unwrap_or_else(|_| panic!("libxaac sources not found at {}", root.display())),
    );
    if !root.join("encoder/ixheaace_api.h").is_file() {
        panic!(
            "libxaac submodule at {} is empty; run `git submodule update --init third_party/libxaac`",
            root.display()
        );
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // Directory-level change tracking (a handful of lines instead of ~140 files).
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc");
    for dir in ["encoder", "encoder/drc_src", "common"] {
        println!("cargo:rerun-if-changed={}", root.join(dir).display());
    }

    let patched = apply_patches(&root, &out_dir.join("xaac-patched"));

    let mut sources: Vec<PathBuf> = Vec::new();
    for dir in ["encoder", "encoder/drc_src", "common"] {
        let src_dir = root.join(dir);
        let mut files: Vec<PathBuf> = fs::read_dir(&src_dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", src_dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "c"))
            .collect();
        files.sort();
        for f in files {
            let rel = f.strip_prefix(&root).expect("path under libxaac root");
            let rel = rel.to_string_lossy().replace('\\', "/");
            match patched.iter().find(|(orig, _)| *orig == rel) {
                Some((_, copy)) => sources.push(copy.clone()),
                None => sources.push(f),
            }
        }
    }
    // 129 encoder + 8 DRC + 3 common files in v0.1.13.
    assert!(
        sources.len() >= 135,
        "unexpectedly few libxaac encoder sources ({})",
        sources.len()
    );

    let mut build = cc::Build::new();
    build.warnings(false).cargo_warnings(false);
    // Quoted includes of the patched copies resolve through these paths (libxaac never
    // uses relative include paths).
    build.include(root.join("encoder"));
    build.include(root.join("encoder/drc_src"));
    build.include(root.join("common"));
    build.define("LOUDNESS_LEVELING_SUPPORT", None);

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "msvc" {
        // As libxaac's CMake does for x86/x86_64 GCC builds: its fixed-point helpers rely
        // on wrapping signed arithmetic.
        build.flag("-fwrapv");
    }

    // The codec is unusably slow unoptimised; always build it optimised, but keep the
    // profile's setting when it is already higher.
    let opt: u32 = env::var("OPT_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    build.opt_level(opt.max(2));

    build.files(&sources);
    build.file(manifest_dir.join("csrc/decdrm_xaac_shim.c"));
    build.compile("xaacenc");
}

/// Writes the patched copies and returns `(relative original path, patched copy)` pairs.
fn apply_patches(root: &Path, patch_root: &Path) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    for patch in PATCHES {
        let orig = root.join(patch.file);
        let mut text = fs::read_to_string(&orig)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", orig.display()))
            // The submodule may be checked out with CRLF (core.autocrlf); anchors use LF.
            .replace("\r\n", "\n");
        for (i, edit) in patch.edits.iter().enumerate() {
            let count = text.matches(edit.anchor).count();
            if count != 1 {
                panic!(
                    "DecDRM libxaac edit #{i} for {} matched {count} times (expected exactly \
                     once); the libxaac sources changed — review crates/decdrm-xaac-sys/build.rs",
                    patch.file
                );
            }
            text = text.replacen(edit.anchor, edit.replacement, 1);
        }
        let notice = format!(
            "/* DecDRM: this file was changed by crates/decdrm-xaac-sys/build.rs (2026-09-29)\n \
             * to {}.\n * It is a modified version of the libxaac original (Apache License 2.0).\n \
             * Changed lines are marked \"DecDRM\". */\n",
            patch.why
        );
        let rel_dir = Path::new(patch.file)
            .parent()
            .expect("patched file has a directory");
        let dst_dir = patch_root.join(rel_dir);
        fs::create_dir_all(&dst_dir).expect("create patch dir");
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
