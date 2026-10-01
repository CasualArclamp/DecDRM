//! The public KiwiSDR list as the community mirror publishes it
//! (`rx.linkfanel.net/kiwisdr_com.js`, regenerated from kiwisdr.com/public every few
//! minutes): which Kiwis are up, where, how busy, and whether their owners let apps
//! other than the web page connect (`ext_api`: the number of channels apps may use;
//! many owners allow none). The file is JavaScript, `var kiwisdr_com = [ {…}, … ];`,
//! whose array is JSON apart from trailing commas.

use crate::address::KiwiAddress;

/// Where the list is. DecDRM downloads it only when asked to.
pub const DIRECTORY_URL: &str = "http://rx.linkfanel.net/kiwisdr_com.js";

/// One KiwiSDR of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct KiwiEntry {
    pub name: String,
    pub url: String,
    pub address: KiwiAddress,
    /// The owner's description of the location.
    pub location: String,
    /// Latitude, longitude (degrees).
    pub gps: Option<(f64, f64)>,
    pub users: u32,
    pub users_max: u32,
    /// Channels the owner lets apps (other than the web page) use.
    pub apps: u32,
    pub antenna: String,
    /// The Kiwi's own SNR measurement, dB: whole band and HF.
    pub snr: Option<(i32, i32)>,
    /// The Kiwi has the DRM extension (its web page decodes DRM itself).
    pub drm: bool,
    /// Firmware, e.g. "1.902".
    pub version: String,
    /// Frequencies it receives, kHz.
    pub bands_khz: Option<(f64, f64)>,
    /// Listed as active and not offline.
    pub online: bool,
}

impl KiwiEntry {
    /// Free channels.
    pub fn free(&self) -> u32 {
        self.users_max.saturating_sub(self.users)
    }

    /// The owner lets apps such as DecDRM connect.
    pub fn allows_apps(&self) -> bool {
        self.apps > 0
    }

    /// Whether `khz` lies within its bands (unknown bands: yes).
    pub fn covers(&self, khz: f64) -> bool {
        self.bands_khz.is_none_or(|(lo, hi)| khz >= lo && khz <= hi)
    }

    /// Great-circle distance to `(lat, lon)` in km.
    pub fn distance_km(&self, lat: f64, lon: f64) -> Option<f64> {
        let (a, b) = self.gps?;
        let (p1, p2) = (a.to_radians(), lat.to_radians());
        let dp = p2 - p1;
        let dl = (lon - b).to_radians();
        let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
        Some(2.0 * 6371.0 * h.sqrt().min(1.0).asin())
    }
}

/// Why the list could not be read.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error("not a KiwiSDR list (no JSON array)")]
    NoArray,
    #[error("the KiwiSDR list is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Parse the list. Entries without a usable address are left out.
pub fn parse_directory(text: &str) -> Result<Vec<KiwiEntry>, DirectoryError> {
    let start = text.find('[').ok_or(DirectoryError::NoArray)?;
    let end = text.rfind(']').ok_or(DirectoryError::NoArray)?;
    if end <= start {
        return Err(DirectoryError::NoArray);
    }
    let list: Vec<serde_json::Value> = serde_json::from_str(&strip_trailing_commas(&text[start..=end]))?;
    Ok(list.iter().filter_map(entry).collect())
}

fn entry(v: &serde_json::Value) -> Option<KiwiEntry> {
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("").trim().to_string();
    let n = |k: &str| s(k).parse::<u32>().unwrap_or(0);
    let url = s("url");
    let address = KiwiAddress::parse(&url).ok()?;
    let gps = {
        let g = s("gps");
        let mut it = g.trim_matches(|c| c == '(' || c == ')').split(',').map(|x| x.trim().parse::<f64>());
        match (it.next(), it.next()) {
            (Some(Ok(lat)), Some(Ok(lon))) if (lat, lon) != (0.0, 0.0) => Some((lat, lon)),
            _ => None,
        }
    };
    let snr = {
        let t = s("snr");
        let mut it = t.split(',').map(|x| x.trim().parse::<i32>());
        match (it.next(), it.next()) {
            (Some(Ok(a)), Some(Ok(b))) => Some((a, b)),
            _ => None,
        }
    };
    let bands_khz = s("bands").split_once('-').and_then(|(lo, hi)| Some((lo.trim().parse::<f64>().ok()? / 1e3, hi.trim().parse::<f64>().ok()? / 1e3)));
    let version = s("sw_version").trim_start_matches("KiwiSDR_v").to_string();
    Some(KiwiEntry {
        name: s("name"),
        url,
        address,
        location: s("loc"),
        gps,
        users: n("users"),
        users_max: n("users_max"),
        apps: n("ext_api"),
        antenna: s("antenna"),
        snr,
        drm: s("sdr_hw").contains("DRM"),
        version,
        bands_khz,
        online: s("status") == "active" && s("offline") != "yes",
    })
}

/// Remove commas that directly precede `]` or `}` (JavaScript allows them, JSON does
/// not), outside string literals.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let (mut in_string, mut escaped) = (false, false);
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                // Look past whitespace for a closing bracket.
                let rest: String = chars.clone().take_while(|c| c.is_whitespace()).collect();
                let next = chars.clone().nth(rest.chars().count());
                if !matches!(next, Some(']' | '}')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"// KiwiSDR.com receiver list for dyatlov map maker
var kiwisdr_com =
[
	{
		"name":"0-30 MHz SDR, \"quoted\", ]",
		"url":"http://ja2jvc.ddns.net:8073",
		"status":"active",
		"offline":"no",
		"users":"2",
		"users_max":"8",
		"ext_api":"4",
		"gps":"(35.12, 138.91)",
		"loc":"Mishima, Japan",
		"antenna":"Loop",
		"snr":"23,19",
		"sdr_hw":"KiwiSDR 2 v1.902 ⁣ 📻 DRM ⁣",
		"sw_version":"KiwiSDR_v1.902",
		"bands":"10000-30000000",
	},
	{
		"name":"No apps here",
		"url":"http://example.proxy.kiwisdr.com",
		"status":"active",
		"offline":"yes",
		"users":"8",
		"users_max":"8",
		"ext_api":"0",
		"gps":"(0, 0)",
	},
	{ "name":"no address", "url":"" },
];
"#;

    #[test]
    fn parses_the_community_list() {
        let list = parse_directory(SAMPLE).unwrap();
        assert_eq!(list.len(), 2, "the entry without an address is left out");
        let a = &list[0];
        assert_eq!(a.name, "0-30 MHz SDR, \"quoted\", ]");
        assert_eq!((a.address.host.as_str(), a.address.port), ("ja2jvc.ddns.net", 8073));
        assert_eq!((a.users, a.users_max, a.free(), a.apps), (2, 8, 6, 4));
        assert!(a.allows_apps() && a.drm && a.online);
        assert_eq!(a.gps, Some((35.12, 138.91)));
        assert_eq!(a.snr, Some((23, 19)));
        assert_eq!(a.version, "1.902");
        assert_eq!(a.bands_khz, Some((10.0, 30_000.0)));
        assert!(a.covers(6140.0) && !a.covers(95_200.0));
        let b = &list[1];
        assert!(!b.allows_apps() && !b.online && !b.drm);
        assert_eq!((b.free(), b.gps, b.bands_khz), (0, None, None));
        assert!(b.covers(95_200.0), "unknown bands: assume yes");
    }

    #[test]
    fn distances() {
        let a = &parse_directory(SAMPLE).unwrap()[0];
        // Mishima to Pyongyang is about 1270 km.
        let d = a.distance_km(39.03, 125.75).unwrap();
        assert!((d - 1270.0).abs() < 60.0, "{d}");
    }

    #[test]
    fn not_a_list() {
        assert!(matches!(parse_directory("<html>captcha</html>"), Err(DirectoryError::NoArray)));
        assert!(matches!(parse_directory("var x = [ {"), Err(DirectoryError::NoArray)));
    }

    #[test]
    fn trailing_commas_outside_strings_only() {
        assert_eq!(strip_trailing_commas(r#"[{"a":"x,]",},]"#), r#"[{"a":"x,]"}]"#);
        assert_eq!(strip_trailing_commas("[1, 2 ,\n ]"), "[1, 2 \n ]");
    }
}
