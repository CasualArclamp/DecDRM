//! Electronic Programme Guide / Service & Programme Information in the binary encoding
//! of ETSI TS 102 371, decoded to (and encoded from) the XML vocabulary of TS 102 818.
//!
//! The binary form is a tree of tag-length-value items:
//!
//! ```text
//! tag 8 | length 8 (0xFE: next 16 bits, 0xFF: next 24 bits) | value
//! tag 0x01 CDATA, 0x02 epg, 0x03 serviceInformation, 0x04 string token table,
//!     0x05 default content id, 0x06 default language,
//!     0x10..0x7F child elements, 0x80..0xFF attributes of the enclosing element
//! ```
//!
//! Strings may contain tokens (bytes 0x01..=0x13 except TAB, LF, CR) that expand to
//! entries of the token table. Port of Dream's `CEPGDecoder` (`util-QT/epgdec.cpp`),
//! producing an [`EpgElement`] tree instead of a Qt `QDomDocument`; the encoder is new.
//!
//! EPG objects travel as MOT objects with ContentType 7 (sub-types 0 service
//! information, 1 programme information, 2 group information), usually gzip
//! compressed, with ScopeStart/ScopeEnd/ScopeId header parameters.

mod tables;

use crate::error::{DataError, Result};
use crate::mot::{MotHeader, content_type, param};
use crate::time::MotTime;
use std::fmt::Write as _;
use tables::{AttrDef, AttrKind, ElementDef};

/// A node of the EPG tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EpgNode {
    /// Child element.
    Element(EpgElement),
    /// Character data.
    Text(String),
}

/// A typed attribute value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EpgValue {
    /// Enumerated value, by name.
    Enum(String),
    /// Text.
    Text(String),
    /// 16-bit number.
    U16(u16),
    /// 24-bit number.
    U24(u32),
    /// Time point.
    Time(MotTime),
    /// Duration in seconds.
    Duration(u16),
    /// 3-byte identifier.
    Sid([u8; 3]),
    /// Genre reference bytes (scheme, term levels).
    Genre(Vec<u8>),
    /// Bitrate in 0.1 kbit/s.
    Bitrate(u16),
    /// Undecodable value, kept verbatim (shown as hex).
    Raw(Vec<u8>),
}

impl EpgValue {
    /// The value as it appears in the XML attribute.
    pub fn to_xml_string(&self) -> String {
        match self {
            Self::Enum(s) | Self::Text(s) => s.clone(),
            Self::U16(n) => n.to_string(),
            Self::U24(n) => n.to_string(),
            Self::Time(t) => t.to_iso8601(),
            Self::Duration(secs) => {
                let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
                let mut out = String::from("PT");
                if h > 0 {
                    let _ = write!(out, "{h}H");
                }
                if m > 0 {
                    let _ = write!(out, "{m}M");
                }
                if s > 0 || (h == 0 && m == 0) {
                    let _ = write!(out, "{s}S");
                }
                out
            }
            Self::Sid(b) => format!("{:x}.{:x}.{:x}", b[0], b[1], b[2]),
            Self::Genre(bytes) => {
                let scheme = bytes.first().copied().unwrap_or(0);
                let name = match scheme {
                    1..=8 => tables::GENRE_SCHEMES[usize::from(scheme - 1)].to_owned(),
                    _ => format!("CS{scheme}"),
                };
                let terms: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
                format!("urn:tva:metadata:cs:{name}:2005:{}", terms.join("."))
            }
            Self::Bitrate(n) => {
                if n % 10 == 0 {
                    (n / 10).to_string()
                } else {
                    format!("{}.{}", n / 10, n % 10)
                }
            }
            Self::Raw(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// Parse attribute text into the value type `kind` expects (the inverse of
    /// [`Self::to_xml_string`]).
    fn parse_for(kind: AttrKind, text: &str) -> Option<Self> {
        Some(match kind {
            AttrKind::Enum(names) => {
                names.iter().find(|n| !n.is_empty() && **n == text)?;
                Self::Enum(text.to_owned())
            }
            AttrKind::String => Self::Text(text.to_owned()),
            AttrKind::U16 => Self::U16(text.trim().parse().ok()?),
            AttrKind::U24 => Self::U24(text.trim().parse().ok().filter(|n| *n <= 0xFF_FFFF)?),
            AttrKind::Time => Self::Time(MotTime::parse_iso8601(text)?),
            AttrKind::Duration => Self::Duration(parse_duration(text)?),
            AttrKind::Sid => {
                let parts: Vec<u8> = text
                    .split('.')
                    .map(|p| u8::from_str_radix(p, 16).ok())
                    .collect::<Option<_>>()?;
                Self::Sid(parts.try_into().ok()?)
            }
            AttrKind::Genre => {
                let terms = text.rsplit(':').next()?;
                let bytes: Vec<u8> = terms
                    .split('.')
                    .map(|p| p.parse().ok())
                    .collect::<Option<_>>()?;
                if bytes.is_empty() || bytes.len() > 4 {
                    return None;
                }
                Self::Genre(bytes)
            }
            AttrKind::Bitrate => {
                let (int, frac) = text.trim().split_once('.').unwrap_or((text.trim(), "0"));
                let tenths = int.parse::<u32>().ok()?.checked_mul(10)?;
                let v = tenths.checked_add(frac.chars().next()?.to_digit(10)?)?;
                Self::Bitrate(u16::try_from(v).ok()?)
            }
            AttrKind::Unused => return None,
        })
    }
}

/// Parse an ISO 8601 duration of the form `PT[nH][nM][nS]`.
fn parse_duration(text: &str) -> Option<u16> {
    let rest = text.trim().strip_prefix("PT")?;
    let mut total: u32 = 0;
    let mut num = String::new();
    for c in rest.chars() {
        match c {
            '0'..='9' => num.push(c),
            'H' | 'M' | 'S' => {
                let n: u32 = num.parse().ok()?;
                num.clear();
                let unit = match c {
                    'H' => 3600,
                    'M' => 60,
                    _ => 1,
                };
                total = total.checked_add(n.checked_mul(unit)?)?;
            }
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    u16::try_from(total).ok()
}

/// An EPG element with attributes and children.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EpgElement {
    /// Element name (e.g. `programme`).
    pub name: String,
    /// Attributes in order.
    pub attributes: Vec<(String, EpgValue)>,
    /// Child nodes in order.
    pub children: Vec<EpgNode>,
}

impl EpgElement {
    /// An empty element.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Builder: add an attribute.
    pub fn attr(mut self, name: impl Into<String>, value: EpgValue) -> Self {
        self.attributes.push((name.into(), value));
        self
    }

    /// Builder: add a text attribute; for non-text attributes the text is parsed into
    /// the binary type when encoding (e.g. `"PT30M"` for a duration).
    pub fn attr_str(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.attr(name, EpgValue::Text(value.into()))
    }

    /// Builder: add a child element.
    pub fn child(mut self, child: EpgElement) -> Self {
        self.children.push(EpgNode::Element(child));
        self
    }

    /// Builder: add character data.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.children.push(EpgNode::Text(text.into()));
        self
    }

    /// Attribute value by name.
    pub fn attribute(&self, name: &str) -> Option<&EpgValue> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// Child elements (skipping text).
    pub fn elements(&self) -> impl Iterator<Item = &EpgElement> {
        // Rust note: `impl Iterator` in return position hides the concrete (and
        // unnameable) iterator type while still being zero-cost.
        self.children.iter().filter_map(|c| match c {
            EpgNode::Element(e) => Some(e),
            EpgNode::Text(_) => None,
        })
    }

    /// First child element called `name`.
    pub fn find(&self, name: &str) -> Option<&EpgElement> {
        self.elements().find(|e| e.name == name)
    }

    /// Concatenated character data of this element (not descendants).
    pub fn text_content(&self) -> String {
        self.children
            .iter()
            .filter_map(|c| match c {
                EpgNode::Text(t) => Some(t.as_str()),
                EpgNode::Element(_) => None,
            })
            .collect()
    }

    /// Serialise as an XML document (UTF-8, with declaration, two-space indentation).
    pub fn to_xml(&self) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        self.write_xml(&mut out, 0);
        out
    }

    fn write_xml(&self, out: &mut String, depth: usize) {
        let indent = "  ".repeat(depth);
        out.push_str(&indent);
        out.push('<');
        out.push_str(&self.name);
        for (name, value) in &self.attributes {
            let _ = write!(out, " {name}=\"{}\"", xml_escape(&value.to_xml_string()));
        }
        if self.children.is_empty() {
            out.push_str("/>\n");
            return;
        }
        if self.children.iter().all(|c| matches!(c, EpgNode::Text(_))) {
            let _ = writeln!(out, ">{}</{}>", xml_escape(&self.text_content()), self.name);
            return;
        }
        out.push_str(">\n");
        for child in &self.children {
            match child {
                EpgNode::Element(e) => e.write_xml(out, depth + 1),
                EpgNode::Text(t) => {
                    let _ = writeln!(out, "{indent}  {}", xml_escape(t));
                }
            }
        }
        let _ = writeln!(out, "{indent}</{}>", self.name);
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters XML 1.0 does not allow.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------------
// Binary decoding

/// Decoding context: the string token table applies to the whole object.
#[derive(Default)]
struct Ctx {
    tokens: [Option<Vec<u8>>; 20],
}

impl Ctx {
    fn load_tokens(&mut self, v: &[u8]) {
        self.tokens = Default::default();
        let mut i = 0;
        while i + 2 <= v.len() {
            let (tok, len) = (usize::from(v[i]), usize::from(v[i + 1]));
            let Some(value) = v.get(i + 2..i + 2 + len) else {
                break;
            };
            if tok < self.tokens.len() {
                self.tokens[tok] = Some(value.to_vec());
            }
            i += 2 + len;
        }
    }

    /// Expand string tokens and decode UTF-8.
    fn string(&self, v: &[u8]) -> String {
        let mut out = Vec::with_capacity(v.len());
        for &b in v {
            match b {
                0x01..=0x13 if !matches!(b, 0x09 | 0x0A | 0x0D) => {
                    if let Some(t) = &self.tokens[usize::from(b)] {
                        out.extend_from_slice(t);
                    }
                }
                _ => out.push(b),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

/// Read one TLV at `pos`: (tag, value, position after it).
fn read_tlv(data: &[u8], pos: usize) -> Result<(u8, &[u8], usize)> {
    let tag = *data.get(pos).ok_or(DataError::Truncated)?;
    let l0 = *data.get(pos + 1).ok_or(DataError::Truncated)?;
    let (len, hdr) = match l0 {
        0xFE => {
            let b = data.get(pos + 2..pos + 4).ok_or(DataError::Truncated)?;
            (usize::from(u16::from_be_bytes([b[0], b[1]])), 4)
        }
        0xFF => {
            let b = data.get(pos + 2..pos + 5).ok_or(DataError::Truncated)?;
            (
                (usize::from(b[0]) << 16) | (usize::from(b[1]) << 8) | usize::from(b[2]),
                5,
            )
        }
        n => (usize::from(n), 2),
    };
    let start = pos + hdr;
    let value = data.get(start..start + len).ok_or(DataError::Truncated)?;
    Ok((tag, value, start + len))
}

const MAX_DEPTH: usize = 32;

fn decode_element(
    def: &ElementDef,
    value: &[u8],
    ctx: &mut Ctx,
    depth: usize,
) -> Result<EpgElement> {
    if depth > MAX_DEPTH {
        return Err(DataError::Malformed("EPG nesting depth"));
    }
    let mut el = EpgElement::new(def.name);
    let mut pos = 0;
    while pos < value.len() {
        let (tag, v, next) = read_tlv(value, pos)?;
        pos = next;
        match tag {
            tables::TAG_CDATA => el.children.push(EpgNode::Text(ctx.string(v))),
            tables::TAG_TOKEN_TABLE => ctx.load_tokens(v),
            // Default content id / default language: not represented in the XML.
            tables::TAG_DEFAULT_CONTENT_ID | tables::TAG_DEFAULT_LANGUAGE => {}
            0x80..=0xFF => {
                // Unknown attributes are skipped.
                if let Some(attr) = def.attr(tag) {
                    el.attributes
                        .push((attr.name.to_owned(), decode_value(attr, v, ctx)));
                }
            }
            _ => {
                // Unknown elements are skipped with their subtree.
                if let Some(child) = tables::element_by_tag(tag) {
                    el.children
                        .push(EpgNode::Element(decode_element(child, v, ctx, depth + 1)?));
                }
            }
        }
    }
    Ok(el)
}

fn decode_value(attr: &AttrDef, v: &[u8], ctx: &Ctx) -> EpgValue {
    let raw = || EpgValue::Raw(v.to_vec());
    match attr.kind {
        AttrKind::Enum(names) => match v {
            [n] if *n >= 1
                && usize::from(*n) <= names.len()
                && !names[usize::from(*n) - 1].is_empty() =>
            {
                EpgValue::Enum(names[usize::from(*n) - 1].to_owned())
            }
            _ => raw(),
        },
        AttrKind::String => EpgValue::Text(ctx.string(v)),
        AttrKind::U16 => match v {
            [a, b] => EpgValue::U16(u16::from_be_bytes([*a, *b])),
            _ => raw(),
        },
        AttrKind::U24 => match v {
            [a, b, c] => EpgValue::U24(u32::from_be_bytes([0, *a, *b, *c])),
            _ => raw(),
        },
        AttrKind::Time => {
            let mut r = crate::bits::BitReader::new(v);
            match MotTime::read(&mut r) {
                Ok((_, t)) => EpgValue::Time(t),
                Err(_) => raw(),
            }
        }
        AttrKind::Duration => match v {
            [a, b] => EpgValue::Duration(u16::from_be_bytes([*a, *b])),
            _ => raw(),
        },
        AttrKind::Sid => match v {
            [a, b, c] => EpgValue::Sid([*a, *b, *c]),
            _ => raw(),
        },
        AttrKind::Genre if (1..=4).contains(&v.len()) => EpgValue::Genre(v.to_vec()),
        AttrKind::Bitrate => match v {
            [a, b] => EpgValue::Bitrate(u16::from_be_bytes([*a, *b])),
            _ => raw(),
        },
        AttrKind::Genre | AttrKind::Unused => raw(),
    }
}

/// Decode a binary EPG object (already decompressed) into an element tree.
pub fn decode(bytes: &[u8]) -> Result<EpgElement> {
    let (tag, value, _) = read_tlv(bytes, 0)?;
    let def = match tag {
        0x02 | 0x03 => tables::element_by_tag(tag).expect("root elements are in the table"),
        _ => return Err(DataError::Malformed("EPG root element")),
    };
    decode_element(def, value, &mut Ctx::default(), 0)
}

/// Decode a binary EPG object straight to XML text.
pub fn decode_to_xml(bytes: &[u8]) -> Result<String> {
    decode(bytes).map(|root| root.to_xml())
}

// ---------------------------------------------------------------------------------
// Binary encoding

fn write_tlv(tag: u8, value: &[u8], out: &mut Vec<u8>) -> Result<()> {
    out.push(tag);
    let n = value.len();
    if n < 0xFE {
        out.push(n as u8);
    } else if n <= 0xFFFF {
        out.push(0xFE);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xFF_FFFF {
        out.push(0xFF);
        out.extend_from_slice(&(n as u32).to_be_bytes()[1..]);
    } else {
        return Err(DataError::OutOfRange("EPG element length"));
    }
    out.extend_from_slice(value);
    Ok(())
}

/// UTF-8 bytes without the control characters that would read as string tokens.
fn string_bytes(s: &str) -> Vec<u8> {
    s.bytes()
        .filter(|&b| !(0x01..=0x13).contains(&b) || matches!(b, 0x09 | 0x0A | 0x0D))
        .collect()
}

fn encode_value(attr: &AttrDef, value: &EpgValue) -> Result<Vec<u8>> {
    let bad = || DataError::EpgValue {
        attribute: attr.name.to_owned(),
        value: value.to_xml_string(),
    };
    Ok(match (attr.kind, value) {
        (_, EpgValue::Raw(bytes)) => bytes.clone(),
        (AttrKind::String, EpgValue::Text(s)) => string_bytes(s),
        (kind, EpgValue::Text(s)) => {
            let typed = EpgValue::parse_for(kind, s).ok_or_else(bad)?;
            return encode_value(attr, &typed);
        }
        (AttrKind::Enum(names), EpgValue::Enum(s)) => {
            let idx = names
                .iter()
                .position(|n| !n.is_empty() && n == s)
                .ok_or_else(bad)?;
            vec![idx as u8 + 1]
        }
        (AttrKind::U16, EpgValue::U16(n)) => n.to_be_bytes().to_vec(),
        (AttrKind::U24, EpgValue::U24(n)) if *n <= 0xFF_FFFF => n.to_be_bytes()[1..].to_vec(),
        (AttrKind::Time, EpgValue::Time(t)) => {
            let mut w = crate::bits::BitWriter::new();
            t.write(&mut w, false);
            w.into_bytes()
        }
        (AttrKind::Duration, EpgValue::Duration(s)) => s.to_be_bytes().to_vec(),
        (AttrKind::Sid, EpgValue::Sid(b)) => b.to_vec(),
        (AttrKind::Genre, EpgValue::Genre(b)) if (1..=4).contains(&b.len()) => b.clone(),
        (AttrKind::Bitrate, EpgValue::Bitrate(n)) => n.to_be_bytes().to_vec(),
        _ => return Err(bad()),
    })
}

fn encode_element(el: &EpgElement, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(DataError::OutOfRange("EPG nesting depth"));
    }
    let def = tables::element_by_name(&el.name).ok_or_else(|| DataError::EpgValue {
        attribute: "<element>".into(),
        value: el.name.clone(),
    })?;
    let mut body = Vec::new();
    for (name, value) in &el.attributes {
        let (tag, attr) = def.attr_by_name(name).ok_or_else(|| DataError::EpgValue {
            attribute: name.clone(),
            value: format!("not an attribute of {}", el.name),
        })?;
        write_tlv(tag, &encode_value(attr, value)?, &mut body)?;
    }
    for child in &el.children {
        match child {
            EpgNode::Text(t) => write_tlv(tables::TAG_CDATA, &string_bytes(t), &mut body)?,
            EpgNode::Element(e) => encode_element(e, &mut body, depth + 1)?,
        }
    }
    write_tlv(def.tag, &body, out)
}

/// Encode an element tree (root `epg` or `serviceInformation`) in binary form.
pub fn encode(root: &EpgElement) -> Result<Vec<u8>> {
    if root.name != "epg" && root.name != "serviceInformation" {
        return Err(DataError::Malformed("EPG root element"));
    }
    let mut out = Vec::new();
    encode_element(root, &mut out, 0)?;
    Ok(out)
}

// ---------------------------------------------------------------------------------
// MOT helpers

/// A MOT header for an EPG object (ContentType 7).
pub fn mot_header(
    content_subtype: u16,
    name: &str,
    scope_start: Option<MotTime>,
    scope_end: Option<MotTime>,
    scope_id: Option<u32>,
) -> MotHeader {
    let mut h = MotHeader::new(content_type::EPG, content_subtype, 0);
    if !name.is_empty() {
        h.set_content_name(name);
    }
    if let Some(t) = scope_start {
        h.set_param(param::EPG_SCOPE_START, t.encode_param());
    }
    if let Some(t) = scope_end {
        h.set_param(param::EPG_SCOPE_END, t.encode_param());
    }
    if let Some(id) = scope_id {
        let bytes = id.to_be_bytes();
        let skip = if id <= 0xFFFF {
            2
        } else if id <= 0xFF_FFFF {
            1
        } else {
            0
        };
        h.set_param(param::EPG_SCOPE_ID, bytes[skip..].to_vec());
    }
    h
}

/// A file name for an EPG object: its ContentName, or Dream's `epgFilename` scheme
/// (`YYYYMMDD` + scope id in hex + S/P/G + `.EHB`) when it has none.
pub fn object_name(header: &MotHeader) -> String {
    if let Some(name) = header.content_name().filter(|n| !n.is_empty()) {
        return name;
    }
    let (y, m, d) = header
        .epg_scope_start()
        .map(|t| t.date())
        .unwrap_or((0, 0, 0));
    let kind = match header.content_subtype {
        0 => "S",
        1 => "P",
        2 => "G",
        _ => "",
    };
    let advanced = header.profile_subset().is_some_and(|p| p.contains(&2));
    let ext = if advanced { "EHA" } else { "EHB" };
    format!(
        "{y:04}{m:02}{d:02}{:04x}{kind}.{ext}",
        header.epg_scope_id().unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built schedule, byte by byte, as the TS 102 371 encoder of a broadcaster
    /// would produce it: token table, schedule with a scope and one programme.
    fn sample_binary() -> Vec<u8> {
        let mut prog = Vec::new();
        prog.extend_from_slice(&[0x81, 3, 0x00, 0x12, 0x34]); // shortId = 4660
        prog.extend_from_slice(&[0x82, 2, 0x00, 0x02]); // version = 2
        prog.extend_from_slice(&[0x84, 1, 0x02]); // broadcast = off-air
        prog.extend_from_slice(&[0x11, 8, 0x01, 6]); // mediumName { CDATA "\x01 News" }
        prog.extend_from_slice(&[0x01]);
        prog.extend_from_slice(b" News");
        // location { time { time, duration } }
        let mut time = vec![0x80, 4];
        let t = MotTime {
            long_form: false,
            ..MotTime::from_ymd_hms(2026, 9, 29, 18, 0, 0)
        };
        let mut w = crate::bits::BitWriter::new();
        t.write(&mut w, false);
        time.extend_from_slice(&w.into_bytes());
        time.extend_from_slice(&[0x81, 2, 0x07, 0x08]); // duration 1800 s
        let mut loc = vec![0x2C, time.len() as u8];
        loc.extend_from_slice(&time);
        prog.extend_from_slice(&[0x19, loc.len() as u8]);
        prog.extend_from_slice(&loc);
        prog.extend_from_slice(&[0x14, 5, 0x80, 3, 3, 6, 1]); // genre href ContentCS 3.6.1
        let mut sched = vec![0x80, 2, 0x00, 0x01]; // version 1
        sched.extend_from_slice(&[0x82, 3]);
        sched.extend_from_slice(b"BBC"); // originator
        sched.extend_from_slice(&[0x1C, prog.len() as u8]);
        sched.extend_from_slice(&prog);
        let mut epg = vec![0x04, 7, 0x01, 5]; // token table: token 1 = "World"
        epg.extend_from_slice(b"World");
        epg.extend_from_slice(&[0x80, 1, 0x02]); // system = DRM
        epg.extend_from_slice(&[0x21, sched.len() as u8]);
        epg.extend_from_slice(&sched);
        let mut out = vec![0x02, epg.len() as u8];
        out.extend_from_slice(&epg);
        out
    }

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<epg system="DRM">
  <schedule version="1" originator="BBC">
    <programme shortId="4660" version="2" broadcast="off-air">
      <mediumName>World News</mediumName>
      <location>
        <time time="2026-09-29T18:00:00Z" duration="PT30M"/>
      </location>
      <genre href="urn:tva:metadata:cs:ContentCS:2005:3.6.1"/>
    </programme>
  </schedule>
</epg>
"#;

    #[test]
    fn hand_built_schedule_to_xml() {
        assert_eq!(decode_to_xml(&sample_binary()).unwrap(), SAMPLE_XML);
    }

    #[test]
    fn tree_round_trip() {
        let tree = decode(&sample_binary()).unwrap();
        let bytes = encode(&tree).unwrap();
        assert_eq!(decode(&bytes).unwrap(), tree);
        let prog = tree.find("schedule").unwrap().find("programme").unwrap();
        assert_eq!(
            prog.find("mediumName").unwrap().text_content(),
            "World News"
        );
        assert_eq!(prog.attribute("shortId"), Some(&EpgValue::U24(4660)));
    }

    #[test]
    fn built_from_strings() {
        let si = EpgElement::new("serviceInformation")
            .attr_str("version", "3")
            .attr_str("originator", "DecDRM & friends")
            .child(
                EpgElement::new("ensemble").attr_str("id", "e1.ce15").child(
                    EpgElement::new("service")
                        .attr_str("format", "audio")
                        .attr_str("bitrate", "12.5")
                        .child(
                            EpgElement::new("serviceID")
                                .attr_str("id", "e1.c221")
                                .attr_str("type", "primary"),
                        )
                        .child(EpgElement::new("shortName").text("Test <1>"))
                        .child(
                            EpgElement::new("multimedia")
                                .attr_str("type", "logo_colour_square")
                                .attr_str("width", "32")
                                .attr_str("url", "logo.png"),
                        ),
                ),
            );
        let bytes = encode(&si).unwrap();
        let xml = decode_to_xml(&bytes).unwrap();
        assert!(
            xml.contains(r#"<serviceInformation version="3" originator="DecDRM &amp; friends">"#),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<service format="audio" bitrate="12.5">"#),
            "{xml}"
        );
        assert!(
            xml.contains("<shortName>Test &lt;1&gt;</shortName>"),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<multimedia type="logo_colour_square" width="32" url="logo.png"/>"#),
            "{xml}"
        );
        // Values that do not fit their binary type are rejected.
        let bad =
            EpgElement::new("epg").child(EpgElement::new("schedule").attr_str("version", "x"));
        assert!(matches!(encode(&bad), Err(DataError::EpgValue { .. })));
        assert!(encode(&EpgElement::new("programme")).is_err());
    }

    #[test]
    fn long_lengths_and_errors() {
        let text = "x".repeat(70_000);
        let tree = EpgElement::new("epg").child(
            EpgElement::new("schedule").child(
                EpgElement::new("programme")
                    .child(EpgElement::new("longDescription").text(text.clone())),
            ),
        );
        let bytes = encode(&tree).unwrap();
        assert_eq!(bytes[1], 0xFF); // 24-bit length
        assert_eq!(decode(&bytes).unwrap(), tree);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        assert_eq!(
            decode(&[0x21, 0]),
            Err(DataError::Malformed("EPG root element"))
        );
    }

    #[test]
    fn durations_and_values() {
        assert_eq!(EpgValue::Duration(0).to_xml_string(), "PT0S");
        assert_eq!(EpgValue::Duration(3725).to_xml_string(), "PT1H2M5S");
        assert_eq!(parse_duration("PT1H2M5S"), Some(3725));
        assert_eq!(parse_duration("PT5"), None);
        assert_eq!(
            EpgValue::Sid([0xe1, 0xc2, 0x21]).to_xml_string(),
            "e1.c2.21"
        );
        assert_eq!(EpgValue::Bitrate(1280).to_xml_string(), "128");
    }

    #[test]
    fn mot_naming() {
        let start = MotTime::from_ymd_hms(2026, 9, 29, 0, 0, 0);
        let h = mot_header(1, "", Some(start), None, Some(0xE1C221));
        assert_eq!(h.epg_scope_id(), Some(0xE1C221));
        assert_eq!(object_name(&h), "20260929e1c221P.EHB");
        let named = mot_header(0, "SI.xml.gz", None, None, None);
        assert_eq!(object_name(&named), "SI.xml.gz");
    }
}
