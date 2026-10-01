//! KiwiSDR addresses as people write them: `host`, `host:port`, or a URL copied from the
//! browser (`http://kiwi.example:8073/?f=6140iqz10`, whose `f=` also gives a frequency).

use std::fmt;

/// The port KiwiSDRs listen on unless their owner changed it.
pub const DEFAULT_PORT: u16 = 8073;

/// Where a KiwiSDR is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KiwiAddress {
    pub host: String,
    pub port: u16,
}

/// Why an address was not understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("no KiwiSDR address given")]
    Empty,
    #[error("\"{0}\" is not a valid port number")]
    BadPort(String),
    #[error("\"{0}\" is not a host name or IP address")]
    BadHost(String),
}

impl KiwiAddress {
    /// Parse an address; the scheme (`http`, `https`, `ws`, `wss`) and anything after
    /// the host and port are ignored, and the port defaults to [`DEFAULT_PORT`].
    pub fn parse(s: &str) -> Result<Self, AddressError> {
        let s = s.trim();
        let rest = strip_scheme(s);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.is_empty() {
            return Err(AddressError::Empty);
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or_else(|| AddressError::BadHost(authority.to_string()))?;
            let port = match after.strip_prefix(':') {
                Some(p) => parse_port(p)?,
                None if after.is_empty() => DEFAULT_PORT,
                None => return Err(AddressError::BadHost(authority.to_string())),
            };
            (h.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), parse_port(p)?),
                None => (authority.to_string(), DEFAULT_PORT),
            }
        };
        let valid = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'));
        if !valid {
            return Err(AddressError::BadHost(host));
        }
        Ok(Self { host: host.to_ascii_lowercase(), port })
    }

    /// Whether the address goes through the kiwisdr.com proxy service.
    pub fn is_proxied(&self) -> bool {
        self.host.ends_with(".proxy.kiwisdr.com")
    }
}

impl fmt::Display for KiwiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

fn strip_scheme(s: &str) -> &str {
    for scheme in ["http://", "https://", "ws://", "wss://"] {
        if s.len() >= scheme.len() && s[..scheme.len()].eq_ignore_ascii_case(scheme) {
            return &s[scheme.len()..];
        }
    }
    s
}

fn parse_port(p: &str) -> Result<u16, AddressError> {
    match p.parse::<u16>() {
        Ok(0) | Err(_) => Err(AddressError::BadPort(p.to_string())),
        Ok(port) => Ok(port),
    }
}

/// The frequency (kHz) in a KiwiSDR URL's `f=` parameter, e.g. `?f=6140iqz10` or
/// `?f=6140.00iq`: the number in front of the mode letters.
pub fn frequency_from_url(s: &str) -> Option<f64> {
    let query = s.split_once('?')?.1;
    let value = query.split(['&', '#']).find_map(|kv| kv.strip_prefix("f="))?;
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    digits.parse::<f64>().ok().filter(|f| *f > 0.0 && f.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> (String, u16) {
        let a = KiwiAddress::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        (a.host, a.port)
    }

    #[test]
    fn forms_people_paste() {
        assert_eq!(addr("kiwisdr.areg.org.au"), ("kiwisdr.areg.org.au".into(), 8073));
        assert_eq!(addr(" Kiwi.Example:8074 "), ("kiwi.example".into(), 8074));
        assert_eq!(addr("http://jj8ntm.proxy.kiwisdr.com/"), ("jj8ntm.proxy.kiwisdr.com".into(), 8073));
        assert_eq!(addr("HTTPS://kiwi.example:80/?f=6140iqz10"), ("kiwi.example".into(), 80));
        assert_eq!(addr("ws://192.168.1.20:8073/12345/SND"), ("192.168.1.20".into(), 8073));
        assert_eq!(addr("[2001:db8::1]:8075"), ("2001:db8::1".into(), 8075));
        assert_eq!(addr("[::1]"), ("::1".into(), 8073));
        assert!(KiwiAddress::parse("jj8ntm.proxy.kiwisdr.com").unwrap().is_proxied());
        assert_eq!(KiwiAddress::parse("[::1]:9").unwrap().to_string(), "[::1]:9");
        assert_eq!(KiwiAddress::parse("kiwi.example").unwrap().to_string(), "kiwi.example:8073");
    }

    #[test]
    fn bad_addresses() {
        assert_eq!(KiwiAddress::parse("  "), Err(AddressError::Empty));
        assert_eq!(KiwiAddress::parse("http:///x"), Err(AddressError::Empty));
        assert_eq!(KiwiAddress::parse("kiwi.example:80x"), Err(AddressError::BadPort("80x".into())));
        assert_eq!(KiwiAddress::parse("kiwi.example:0"), Err(AddressError::BadPort("0".into())));
        assert!(matches!(KiwiAddress::parse("kiwi example"), Err(AddressError::BadHost(_))));
    }

    #[test]
    fn frequency_in_a_url() {
        assert_eq!(frequency_from_url("http://kiwi.example:8073/?f=6140iqz10"), Some(6140.0));
        assert_eq!(frequency_from_url("http://kiwi.example:8073/?ext=drm&f=13840.00iq"), Some(13840.0));
        assert_eq!(frequency_from_url("http://kiwi.example:8073/"), None);
        assert_eq!(frequency_from_url("http://kiwi.example:8073/?f=iq"), None);
    }
}
