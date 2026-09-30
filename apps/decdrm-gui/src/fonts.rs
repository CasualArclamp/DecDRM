//! Fallback fonts for the scripts egui's built-in fonts lack (they cover Latin, Greek
//! and Cyrillic). DRM labels, text messages, Journaline pages and programme guides
//! may be Korean, Chinese, Japanese, Arabic, Hindi, Thai, …; without a font for them
//! egui draws empty boxes.
//!
//! The system's own fonts are used — bundling CJK fonts would add tens of MB to the
//! program — and only when needed: the application passes every text it receives to
//! [`FontFallbacks::note`], and [`FontFallbacks::apply`] loads a font the first time a
//! script shows up (a few MB to ~20 MB each, read once) and adds it to egui's fallback
//! chain after the built-in fonts. Candidates: the fonts that ship with Windows; on
//! other systems what fontconfig (`fc-match :lang=…`) names, then common Noto/DejaVu
//! paths. A candidate must contain a sample character of its script.

use eframe::egui;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

/// A group of scripts that one system font covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Script {
    Hangul,
    /// Chinese characters (also Japanese kanji), CJK punctuation and full-width forms.
    Han,
    /// Japanese hiragana and katakana.
    Kana,
    /// Arabic, Hebrew, Armenian and Georgian (one broad-coverage font).
    Broad,
    /// Devanagari, Bengali, Gurmukhi, Gujarati, Oriya, Tamil, Telugu, Kannada,
    /// Malayalam, Sinhala.
    Indic,
    /// Thai and Lao.
    Thai,
    Ethiopic,
    /// Arrows, mathematical and technical symbols, shapes, dingbats.
    Symbols,
}

impl Script {
    /// A character the font for this script must contain.
    fn sample(self) -> char {
        match self {
            Self::Hangul => '한',
            Self::Han => '中',
            Self::Kana => 'か',
            Self::Broad => 'ب',
            Self::Indic => 'क',
            Self::Thai => 'ก',
            Self::Ethiopic => 'ሀ',
            Self::Symbols => '→',
        }
    }
}

/// The script of `c` that needs a fallback font; `None` for what egui's built-in fonts
/// draw (Latin, Greek, Cyrillic, emoji) and for scripts without a fallback here.
pub fn script_of(c: char) -> Option<Script> {
    Some(match u32::from(c) {
        0x0000..=0x052F => return None,
        0x0530..=0x058F | 0x10A0..=0x10FF | 0x2D00..=0x2D2F => Script::Broad, // Armenian, Georgian
        0x0590..=0x05FF | 0xFB1D..=0xFB4F => Script::Broad,                  // Hebrew
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => Script::Broad,
        0x0900..=0x0DFF | 0xA8E0..=0xA8FF => Script::Indic,
        0x0E00..=0x0EFF => Script::Thai,
        0x1200..=0x139F | 0x2D80..=0x2DDF => Script::Ethiopic,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF => Script::Hangul,
        0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF65..=0xFF9F => Script::Kana,
        0x2E80..=0x2FDF | 0x3000..=0x303F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF => {
            Script::Han
        }
        0x2_0000..=0x2_FA1F => Script::Han,
        0x2190..=0x2BFF => Script::Symbols,
        _ => return None,
    })
}

/// A font file (and face index in a collection) that may cover `script`.
type Candidate = (PathBuf, u32);

#[cfg(windows)]
fn candidates(script: Script) -> Vec<Candidate> {
    let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    let files: &[&str] = match script {
        Script::Hangul => &["malgun.ttf", "gulim.ttc"],
        Script::Han => &["msyh.ttc", "msjh.ttc", "simsun.ttc", "YuGothR.ttc", "meiryo.ttc", "msgothic.ttc"],
        Script::Kana => &["YuGothR.ttc", "meiryo.ttc", "msgothic.ttc", "msyh.ttc"],
        Script::Broad => &["segoeui.ttf", "arial.ttf", "tahoma.ttf"],
        Script::Indic => &["Nirmala.ttc", "Nirmala.ttf", "mangal.ttf"],
        Script::Thai => &["LeelawUI.ttf", "leelawad.ttf", "tahoma.ttf"],
        Script::Ethiopic => &["ebrima.ttf", "nyala.ttf"],
        Script::Symbols => &["seguisym.ttf", "segoeui.ttf"],
    };
    files.iter().map(|f| (dir.join(f), 0)).collect()
}

#[cfg(not(windows))]
fn candidates(script: Script) -> Vec<Candidate> {
    let lang = match script {
        Script::Hangul => "ko",
        Script::Han => "zh-cn",
        Script::Kana => "ja",
        Script::Broad => "ar",
        Script::Indic => "hi",
        Script::Thai => "th",
        Script::Ethiopic => "am",
        Script::Symbols => "und-zsym",
    };
    let mut out: Vec<Candidate> = fc_match(lang).into_iter().collect();
    let cjk = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    ];
    let dejavu = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    let paths: &[&str] = match script {
        Script::Hangul | Script::Han | Script::Kana => &cjk,
        Script::Indic => &["/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf"],
        Script::Thai => &["/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf"],
        Script::Ethiopic => &["/usr/share/fonts/truetype/noto/NotoSansEthiopic-Regular.ttf"],
        Script::Broad | Script::Symbols => &dejavu,
    };
    out.extend(paths.iter().map(|p| (PathBuf::from(p), 0)));
    out
}

/// The font fontconfig picks for a language, with its face index.
#[cfg(not(windows))]
fn fc_match(lang: &str) -> Option<Candidate> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}\n%{index}", &format!(":lang={lang}")]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let file = lines.next().filter(|f| !f.is_empty())?;
    let index = lines.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
    Some((PathBuf::from(file), index))
}

/// Whether the font in `data` (face `index`) has a glyph for `c`.
fn has_glyph(data: &[u8], index: u32, c: char) -> bool {
    use ab_glyph::Font as _;
    ab_glyph::FontRef::try_from_slice_and_index(data, index).is_ok_and(|f| f.glyph_id(c).0 != 0)
}

/// The fallback fonts loaded so far, and the scripts still to look after.
#[derive(Default)]
pub struct FontFallbacks {
    /// Scripts seen in the texts noted, not yet handled.
    pending: BTreeSet<Script>,
    /// Scripts handled (a font added, or none found).
    done: BTreeSet<Script>,
    /// Fonts added so far: egui name by file and face.
    loaded: BTreeMap<Candidate, String>,
    defs: Option<egui::FontDefinitions>,
    /// What was loaded, for the log.
    pub messages: Vec<String>,
}

impl FontFallbacks {
    /// Look at a text the GUI will show.
    pub fn note(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_ascii()) {
            if let Some(s) = script_of(c)
                && !self.done.contains(&s)
            {
                self.pending.insert(s);
            }
        }
    }

    /// Load fonts for the scripts noted since the last call; returns whether egui's
    /// fonts changed.
    pub fn apply(&mut self, ctx: &egui::Context) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let mut changed = false;
        for script in std::mem::take(&mut self.pending) {
            self.done.insert(script);
            match self.add(script) {
                Some(msg) => {
                    self.messages.push(msg);
                    changed = true;
                }
                None => self.messages.push(format!("no font found for {script:?} text")),
            }
        }
        if changed && let Some(defs) = &self.defs {
            ctx.set_fonts(defs.clone());
        }
        changed
    }

    /// Add the first candidate font that covers `script`; a log line, or `None`.
    fn add(&mut self, script: Script) -> Option<String> {
        for candidate in candidates(script) {
            if let Some(name) = self.loaded.get(&candidate) {
                // Already added for another script (e.g. one CJK font for all three).
                return Some(format!("{script:?} text uses the font {name}"));
            }
            let Ok(data) = std::fs::read(&candidate.0) else { continue };
            if !has_glyph(&data, candidate.1, script.sample()) {
                continue;
            }
            let name = format!(
                "{}#{}",
                candidate.0.file_name().map_or_else(|| "font".into(), |f| f.to_string_lossy().into_owned()),
                candidate.1
            );
            let defs = self.defs.get_or_insert_with(egui::FontDefinitions::default);
            let mut font = egui::FontData::from_owned(data);
            font.index = candidate.1;
            defs.font_data.insert(name.clone(), Arc::new(font));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                defs.families.entry(family).or_default().push(name.clone());
            }
            let msg = format!("{script:?} text: added the font {}", candidate.0.display());
            self.loaded.insert(candidate, name);
            return Some(msg);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_by_character() {
        for c in ['A', 'é', 'Ж', 'Ω', '1', ' '] {
            assert_eq!(script_of(c), None, "{c}");
        }
        assert_eq!(script_of('한'), Some(Script::Hangul));
        assert_eq!(script_of('中'), Some(Script::Han));
        assert_eq!(script_of('。'), Some(Script::Han), "CJK punctuation");
        assert_eq!(script_of('カ'), Some(Script::Kana));
        assert_eq!(script_of('ب'), Some(Script::Broad));
        assert_eq!(script_of('ש'), Some(Script::Broad));
        assert_eq!(script_of('क'), Some(Script::Indic));
        assert_eq!(script_of('ก'), Some(Script::Thai));
        assert_eq!(script_of('→'), Some(Script::Symbols));
        for s in [Script::Hangul, Script::Han, Script::Kana, Script::Broad, Script::Indic, Script::Thai, Script::Ethiopic, Script::Symbols] {
            assert_eq!(script_of(s.sample()), Some(s), "{s:?}");
        }
    }

    #[test]
    fn scripts_are_noted_once() {
        let mut f = FontFallbacks::default();
        f.note("DRM 조선중앙방송 · 中国 · Radio");
        assert_eq!(f.pending, [Script::Hangul, Script::Han].into());
        f.done.insert(Script::Hangul);
        f.pending.clear();
        f.note("다시 조선");
        assert!(f.pending.is_empty(), "a handled script is not noted again");
    }

    /// The fonts Windows ships cover their scripts (skipped where they are missing).
    #[test]
    fn candidates_cover_their_scripts() {
        for s in [Script::Hangul, Script::Han, Script::Kana, Script::Broad, Script::Indic, Script::Thai, Script::Symbols] {
            if let Some((path, index)) = candidates(s).into_iter().find(|(p, _)| p.is_file()) {
                let data = std::fs::read(&path).unwrap();
                if !has_glyph(&data, index, s.sample()) {
                    eprintln!("{} does not cover {s:?} (another candidate may)", path.display());
                }
            }
        }
    }
}
