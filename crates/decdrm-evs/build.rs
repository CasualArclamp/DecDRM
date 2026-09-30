//! With the `decoder` feature: build the 3GPP EVS floating-point reference decoder
//! (TS 26.443) from the source archive the user supplies. It is not part of DecDRM
//! (3GPP/ETSI copyright; the codec is patent-licensed), so it is never committed.
//!
//! Source: `$DECDRM_EVS_SRC` (a zip from 3GPP or ETSI, or a directory holding one or
//! the extracted `c-code` tree), else `reference/evs/` at the workspace root. The ETSI
//! zip (`ts_126443v…p0.zip`) holds the 3GPP zip (`26443-…-ANSI-C_source_code.zip`),
//! which holds `c-code/`; nested zips are opened here.
//!
//! Compiled: `lib_com` and `lib_dec` without the command-line `main()` (decoder.c),
//! plus `csrc/evs_shim.c` (frame-by-frame API). `lib_com` includes headers of
//! `lib_enc`, so all three directories are on the include path.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc/evs_shim.c");
    println!("cargo:rerun-if-env-changed=DECDRM_EVS_SRC");
    #[cfg(feature = "decoder")]
    decoder::build();
}

#[cfg(feature = "decoder")]
mod decoder {
    use std::env;
    use std::io::{Cursor, Read};
    use std::path::{Path, PathBuf};

    pub fn build() {
        let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let src = env::var_os("DECDRM_EVS_SRC")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest.join("../../reference/evs"));
        println!("cargo:rerun-if-changed={}", src.display());
        let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
        let code = locate(&src, &out).unwrap_or_else(|| {
            panic!(
                "the `decoder` feature (EVS) needs the 3GPP EVS floating-point source (TS 26.443): put \
                 26443-j00.zip from https://www.3gpp.org/ftp/Specs/archive/26_series/26.443/ or \
                 ts_126443v190000p0.zip from https://www.etsi.org/deliver/etsi_ts/126400_126499/126443/19.00.00_60/ \
                 into {} (or set DECDRM_EVS_SRC to the zip or the extracted c-code directory)",
                src.display()
            )
        });
        compile(&code, &manifest);
    }

    /// Whether `dir` is the reference code's `c-code` directory.
    fn is_code_dir(dir: &Path) -> bool {
        dir.join("lib_dec").join("evs_dec.c").is_file() && dir.join("lib_com").is_dir()
    }

    /// The `c-code` directory: `src` or `src/c-code` itself, else extracted from a zip
    /// (`src`, or the zips in the directory `src`).
    fn locate(src: &Path, out: &Path) -> Option<PathBuf> {
        if src.is_file() {
            return extract(src, out);
        }
        if !src.is_dir() {
            return None;
        }
        for dir in [src.to_path_buf(), src.join("c-code")] {
            if is_code_dir(&dir) {
                return Some(dir);
            }
        }
        let mut zips: Vec<PathBuf> = std::fs::read_dir(src)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")))
            .collect();
        zips.sort();
        // Newest version first (the names carry the version).
        zips.iter().rev().find_map(|z| extract(z, out))
    }

    fn extract(zip_path: &Path, out: &Path) -> Option<PathBuf> {
        println!("cargo:rerun-if-changed={}", zip_path.display());
        let bytes = std::fs::read(zip_path).ok()?;
        let dest = out.join("evs-src");
        let _ = std::fs::remove_dir_all(&dest);
        let code = dest.join("c-code");
        (unpack(&bytes, &dest, 0) && is_code_dir(&code)).then_some(code)
    }

    /// Unpack the `.c`/`.h` files of the `c-code` tree of an archive into `dest`,
    /// looking into nested zips (two levels). Whether any were found.
    fn unpack(bytes: &[u8], dest: &Path, depth: u32) -> bool {
        let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else { return false };
        let mut found = false;
        for i in 0..archive.len() {
            let Ok(mut file) = archive.by_index(i) else { continue };
            let name = file.name().replace('\\', "/");
            if let Some(pos) = name.find("c-code/") {
                let rel = &name[pos..];
                let wanted = rel.ends_with(".c") || rel.ends_with(".h");
                // No path components that could leave `dest`.
                if !wanted || file.is_dir() || rel.split('/').any(|c| c == ".." || c.contains(':')) {
                    continue;
                }
                let path = dest.join(rel);
                let mut data = Vec::new();
                if file.read_to_end(&mut data).is_ok()
                    && std::fs::create_dir_all(path.parent().expect("has a parent")).is_ok()
                    && std::fs::write(&path, data).is_ok()
                {
                    found = true;
                }
            } else if depth < 2 && name.to_ascii_lowercase().ends_with(".zip") {
                let mut inner = Vec::new();
                if file.read_to_end(&mut inner).is_ok() {
                    found |= unpack(&inner, dest, depth + 1);
                }
            }
        }
        found
    }

    fn c_files(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("reference code directory")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "c"))
            .collect();
        v.sort();
        v
    }

    fn compile(code: &Path, manifest: &Path) {
        let (lib_com, lib_dec, lib_enc) = (code.join("lib_com"), code.join("lib_dec"), code.join("lib_enc"));
        let msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "msvc");
        let configure = |b: &mut cc::Build| {
            b.include(&lib_com)
                .include(&lib_dec)
                .include(&lib_enc)
                .define("RELEASE", None)
                .define("_CRT_SECURE_NO_WARNINGS", None)
                // Reference code: its warnings are not ours to fix.
                .warnings(false)
                .flag_if_supported(if msvc { "/w" } else { "-w" });
        };
        let mut main = cc::Build::new();
        configure(&mut main);
        // Always optimised, like the other codecs (far too slow otherwise).
        main.opt_level(2);
        // MSVC's optimiser crashes on avq_dec.c (C1001): that file alone unoptimised.
        // (Only there: with GNU ld two libraries referring to each other would need
        // a link group.)
        let mut unoptimised = cc::Build::new();
        configure(&mut unoptimised);
        unoptimised.opt_level(0);
        let mut split = false;
        for f in c_files(&lib_com) {
            main.file(f);
        }
        for f in c_files(&lib_dec) {
            match f.file_name().and_then(|n| n.to_str()) {
                Some("decoder.c") => {} // the command-line main()
                Some("avq_dec.c") if msvc => {
                    unoptimised.file(f);
                    split = true;
                }
                _ => {
                    main.file(f);
                }
            }
        }
        main.file(manifest.join("csrc").join("evs_shim.c"));
        if split {
            unoptimised.compile("evs_avq");
        }
        main.compile("evs");
        if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
            // ntohl/ntohs in the VoIP bitstream readers.
            println!("cargo:rustc-link-lib=ws2_32");
        } else {
            println!("cargo:rustc-link-lib=m");
        }
    }
}
