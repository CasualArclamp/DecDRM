//! The MOT header: 7-byte header core plus header extension parameters
//! (EN 301 234; Dream `CMOTObject::AddHeader` and `CMOTDABEnc::SetMOTObject`).
//!
//! ```text
//! header core      : BodySize 28 | HeaderSize 13 | ContentType 6 | ContentSubType 9
//! header extension : parameters, each  PLI 2 | ParamId 6 | [DataField]
//!                    PLI 00: no data, 01: 1 byte, 10: 4 bytes,
//!                    11: Ext 1 | DataFieldLength 7 or 15 | DataField
//! ```
//!
//! Parameters are kept raw ([`MotParam`]) so that unknown and application-specific ones
//! survive a decode/encode round trip; typed accessors decode the common ones.

use crate::charset;
use crate::error::{DataError, Result};
use crate::time::{Expiration, MotTime, TriggerTime};

/// MOT content types (EN 301 234 / TS 101 756 table 17) and common sub-types.
pub mod content_type {
    /// General data (sub-types: 0 object transfer, 1 MIME/HTTP).
    pub const GENERAL_DATA: u8 = 0;
    /// Text (sub-types: 0 ASCII, 1 ISO-Latin-1, 2 HTML).
    pub const TEXT: u8 = 1;
    /// Image (sub-types below).
    pub const IMAGE: u8 = 2;
    /// Audio.
    pub const AUDIO: u8 = 3;
    /// Video.
    pub const VIDEO: u8 = 4;
    /// MOT transport (header update / header only).
    pub const MOT_TRANSPORT: u8 = 5;
    /// System (MHEG, Java).
    pub const SYSTEM: u8 = 6;
    /// EPG / SPI objects (TS 102 371; sub-types below).
    pub const EPG: u8 = 7;
    /// Proprietary.
    pub const PROPRIETARY: u8 = 0x3F;

    /// Image sub-type GIF.
    pub const IMAGE_GIF: u16 = 0;
    /// Image sub-type JFIF (JPEG).
    pub const IMAGE_JFIF: u16 = 1;
    /// Image sub-type BMP.
    pub const IMAGE_BMP: u16 = 2;
    /// Image sub-type PNG.
    pub const IMAGE_PNG: u16 = 3;

    /// EPG sub-type: service information.
    pub const EPG_SERVICE_INFORMATION: u16 = 0;
    /// EPG sub-type: programme information (schedule).
    pub const EPG_PROGRAMME_INFORMATION: u16 = 1;
    /// EPG sub-type: group information.
    pub const EPG_GROUP_INFORMATION: u16 = 2;
}

/// MOT parameter ids (EN 301 234 table "MOT parameters"; application-specific ids from
/// TS 102 371 (EPG) and TS 101 499 V3 (SlideShow)).
pub mod param {
    /// PermitOutdatedVersions (1 byte).
    pub const PERMIT_OUTDATED_VERSIONS: u8 = 0x01;
    /// TriggerTime (time).
    pub const TRIGGER_TIME: u8 = 0x05;
    /// VersionNumber (1 byte).
    pub const VERSION_NUMBER: u8 = 0x06;
    /// RetransmissionDistance.
    pub const RETRANSMISSION_DISTANCE: u8 = 0x07;
    /// GroupReference.
    pub const GROUP_REFERENCE: u8 = 0x08;
    /// Expiration (relative 1 byte, or absolute time).
    pub const EXPIRATION: u8 = 0x09;
    /// Priority (1 byte).
    pub const PRIORITY: u8 = 0x0A;
    /// Label (charset + text).
    pub const LABEL: u8 = 0x0B;
    /// ContentName (charset + text).
    pub const CONTENT_NAME: u8 = 0x0C;
    /// UniqueBodyVersion (4 bytes).
    pub const UNIQUE_BODY_VERSION: u8 = 0x0D;
    /// ContentDescription (charset + text).
    pub const CONTENT_DESCRIPTION: u8 = 0x0F;
    /// MimeType (text).
    pub const MIME_TYPE: u8 = 0x10;
    /// CompressionType (1 byte, 1 = gzip).
    pub const COMPRESSION_TYPE: u8 = 0x11;
    /// AdditionalHeader.
    pub const ADDITIONAL_HEADER: u8 = 0x20;
    /// ProfileSubset.
    pub const PROFILE_SUBSET: u8 = 0x21;
    /// CAInfo.
    pub const CA_INFO: u8 = 0x23;
    /// CAReplacementObject.
    pub const CA_REPLACEMENT_OBJECT: u8 = 0x24;

    /// EPG: ScopeStart (time).
    pub const EPG_SCOPE_START: u8 = 0x25;
    /// EPG: ScopeEnd (time).
    pub const EPG_SCOPE_END: u8 = 0x26;
    /// EPG: ScopeId.
    pub const EPG_SCOPE_ID: u8 = 0x27;

    /// SlideShow: CategoryID/SlideID (2 bytes).
    pub const SLS_CATEGORY_SLIDE_ID: u8 = 0x25;
    /// SlideShow: CategoryTitle (text).
    pub const SLS_CATEGORY_TITLE: u8 = 0x26;
    /// SlideShow: ClickThroughURL (text).
    pub const SLS_CLICK_THROUGH_URL: u8 = 0x27;
    /// SlideShow: AlternativeLocationURL (text).
    pub const SLS_ALTERNATIVE_LOCATION_URL: u8 = 0x28;
    /// SlideShow: Alert (1 byte).
    pub const SLS_ALERT: u8 = 0x29;

    /// Directory extension: SortedHeaderInformation (no data).
    pub const SORTED_HEADER_INFORMATION: u8 = 0x00;
    /// Directory extension: DefaultPermitOutdatedVersions.
    pub const DEFAULT_PERMIT_OUTDATED_VERSIONS: u8 = 0x01;
    /// Directory extension: DefaultExpiration.
    pub const DEFAULT_EXPIRATION: u8 = 0x09;
    /// Directory extension (Broadcast Website, TS 101 498): DirectoryIndex (profile + name).
    pub const DIRECTORY_INDEX: u8 = 0x22;
}

/// One raw header-extension parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MotParam {
    /// ParamId 0..=63.
    pub id: u8,
    /// DataField bytes.
    pub data: Vec<u8>,
}

/// Parse a sequence of header extension parameters.
pub(crate) fn parse_params(data: &[u8]) -> Result<Vec<MotParam>> {
    let mut params = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        let id = b & 0x3F;
        let len = match b >> 6 {
            0 => 0,
            1 => 1,
            2 => 4,
            _ => {
                let e = *data.get(pos).ok_or(DataError::Truncated)?;
                if e & 0x80 == 0 {
                    pos += 1;
                    usize::from(e & 0x7F)
                } else {
                    let lo = *data.get(pos + 1).ok_or(DataError::Truncated)?;
                    pos += 2;
                    (usize::from(e & 0x7F) << 8) | usize::from(lo)
                }
            }
        };
        let field = data.get(pos..pos + len).ok_or(DataError::Truncated)?;
        params.push(MotParam {
            id,
            data: field.to_vec(),
        });
        pos += len;
    }
    Ok(params)
}

/// Serialise parameters, choosing the shortest PLI coding for each.
pub(crate) fn write_params(params: &[MotParam], out: &mut Vec<u8>) -> Result<()> {
    for p in params {
        let id = p.id & 0x3F;
        let len = p.data.len();
        match len {
            0 => out.push(id),
            1 => out.push(0x40 | id),
            4 => out.push(0x80 | id),
            _ if len <= 0x7F => out.extend_from_slice(&[0xC0 | id, len as u8]),
            _ if len <= 0x7FFF => {
                out.extend_from_slice(&[0xC0 | id, 0x80 | (len >> 8) as u8, len as u8])
            }
            _ => return Err(DataError::OutOfRange("MOT parameter length")),
        }
        out.extend_from_slice(&p.data);
    }
    Ok(())
}

/// Look up the first parameter with `id`.
pub(crate) fn find_param(params: &[MotParam], id: u8) -> Option<&[u8]> {
    params
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.data.as_slice())
}

/// Replace (or append) parameter `id`.
pub(crate) fn set_param(params: &mut Vec<MotParam>, id: u8, data: Vec<u8>) {
    match params.iter_mut().find(|p| p.id == id) {
        Some(p) => p.data = data,
        None => params.push(MotParam { id, data }),
    }
}

/// Decode a "charset + text" parameter (ContentName, Label, ContentDescription).
fn charset_text(data: &[u8]) -> Option<String> {
    let (&first, text) = data.split_first()?;
    Some(charset::decode(first >> 4, text))
}

fn encode_charset_text(text: &str) -> Vec<u8> {
    let (cs, bytes) = charset::encode(text);
    let mut data = Vec::with_capacity(bytes.len() + 1);
    data.push(cs << 4);
    data.extend_from_slice(&bytes);
    data
}

fn be_uint(data: &[u8]) -> Option<u32> {
    if data.is_empty() || data.len() > 4 {
        return None;
    }
    Some(data.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b)))
}

/// A decoded MOT header.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct MotHeader {
    /// BodySize in bytes (28 bits).
    pub body_size: u32,
    /// ContentType (6 bits), see [`content_type`].
    pub content_type: u8,
    /// ContentSubType (9 bits).
    pub content_subtype: u16,
    /// Header extension parameters in transmission order.
    pub params: Vec<MotParam>,
}

impl MotHeader {
    /// Size of the header core in bytes.
    pub const CORE_LEN: usize = 7;

    /// A header with no parameters.
    pub fn new(content_type: u8, content_subtype: u16, body_size: u32) -> Self {
        Self {
            body_size,
            content_type,
            content_subtype,
            params: Vec::new(),
        }
    }

    /// A header for a file, typed from its name's extension, with ContentName and
    /// MimeType parameters (what a Broadcast Website object needs).
    pub fn for_file(name: &str, body_size: u32) -> Self {
        let (ct, cst, mime) = type_from_name(name);
        let mut h = Self::new(ct, cst, body_size);
        h.set_content_name(name);
        h.set_mime_type(mime);
        h
    }

    /// Parse a header from the start of `bytes`; returns the header and its size
    /// (HeaderSize) so that concatenated headers (MOT directory) can be walked.
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < Self::CORE_LEN {
            return Err(DataError::Truncated);
        }
        let core = u64::from_be_bytes([
            0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
        ]);
        let body_size = (core >> 28) as u32;
        let header_size = ((core >> 15) & 0x1FFF) as usize;
        let content_type = ((core >> 9) & 0x3F) as u8;
        let content_subtype = (core & 0x1FF) as u16;
        if header_size < Self::CORE_LEN {
            return Err(DataError::Malformed("MOT header size"));
        }
        let ext = bytes
            .get(Self::CORE_LEN..header_size)
            .ok_or(DataError::Truncated)?;
        let params = parse_params(ext)?;
        Ok((
            Self {
                body_size,
                content_type,
                content_subtype,
                params,
            },
            header_size,
        ))
    }

    /// Serialise (HeaderSize is computed).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut ext = Vec::new();
        write_params(&self.params, &mut ext)?;
        let header_size = Self::CORE_LEN + ext.len();
        if header_size > 0x1FFF {
            return Err(DataError::OutOfRange("MOT header size"));
        }
        if self.body_size > 0x0FFF_FFFF {
            return Err(DataError::OutOfRange("MOT body size"));
        }
        let core = (u64::from(self.body_size) << 28)
            | ((header_size as u64) << 15)
            | (u64::from(self.content_type & 0x3F) << 9)
            | u64::from(self.content_subtype & 0x1FF);
        let mut out = Vec::with_capacity(header_size);
        out.extend_from_slice(&core.to_be_bytes()[1..]);
        out.extend_from_slice(&ext);
        Ok(out)
    }

    /// Raw data of the first parameter with `id`.
    pub fn param(&self, id: u8) -> Option<&[u8]> {
        find_param(&self.params, id)
    }

    /// Set (replace or append) parameter `id`.
    pub fn set_param(&mut self, id: u8, data: Vec<u8>) {
        set_param(&mut self.params, id, data);
    }

    /// Remove every parameter with `id`.
    pub fn remove_param(&mut self, id: u8) {
        self.params.retain(|p| p.id != id);
    }

    /// ContentName (decoded according to its character-set indicator).
    pub fn content_name(&self) -> Option<String> {
        self.param(param::CONTENT_NAME).and_then(charset_text)
    }

    /// Set ContentName (EBU Latin for plain ASCII, UTF-8 otherwise).
    pub fn set_content_name(&mut self, name: &str) {
        self.set_param(param::CONTENT_NAME, encode_charset_text(name));
    }

    /// MimeType parameter.
    pub fn mime_type(&self) -> Option<String> {
        self.param(param::MIME_TYPE)
            .map(|d| String::from_utf8_lossy(d).trim_end_matches('\0').to_owned())
    }

    /// Set MimeType.
    pub fn set_mime_type(&mut self, mime: &str) {
        self.set_param(param::MIME_TYPE, mime.as_bytes().to_vec());
    }

    /// TriggerTime.
    pub fn trigger_time(&self) -> Option<TriggerTime> {
        self.param(param::TRIGGER_TIME)
            .and_then(|d| TriggerTime::decode(d).ok())
    }

    /// Set TriggerTime.
    pub fn set_trigger_time(&mut self, t: TriggerTime) {
        self.set_param(param::TRIGGER_TIME, t.encode());
    }

    /// VersionNumber.
    pub fn version_number(&self) -> Option<u8> {
        self.param(param::VERSION_NUMBER)
            .and_then(|d| d.first().copied())
    }

    /// Expiration.
    pub fn expiration(&self) -> Option<Expiration> {
        self.param(param::EXPIRATION)
            .and_then(|d| Expiration::decode(d).ok())
    }

    /// Set Expiration.
    pub fn set_expiration(&mut self, e: Expiration) {
        self.set_param(param::EXPIRATION, e.encode());
    }

    /// PermitOutdatedVersions.
    pub fn permit_outdated_versions(&self) -> Option<bool> {
        self.param(param::PERMIT_OUTDATED_VERSIONS)
            .and_then(|d| d.first())
            .map(|&b| b != 0)
    }

    /// Priority.
    pub fn priority(&self) -> Option<u8> {
        self.param(param::PRIORITY).and_then(|d| d.first().copied())
    }

    /// Label.
    pub fn label(&self) -> Option<String> {
        self.param(param::LABEL).and_then(charset_text)
    }

    /// ContentDescription.
    pub fn content_description(&self) -> Option<String> {
        self.param(param::CONTENT_DESCRIPTION)
            .and_then(charset_text)
    }

    /// UniqueBodyVersion.
    pub fn unique_body_version(&self) -> Option<u32> {
        self.param(param::UNIQUE_BODY_VERSION).and_then(be_uint)
    }

    /// CompressionType (1 = gzip).
    pub fn compression_type(&self) -> Option<u8> {
        self.param(param::COMPRESSION_TYPE)
            .and_then(|d| d.first().copied())
    }

    /// ProfileSubset bytes.
    pub fn profile_subset(&self) -> Option<&[u8]> {
        self.param(param::PROFILE_SUBSET)
    }

    /// EPG ScopeStart.
    pub fn epg_scope_start(&self) -> Option<MotTime> {
        self.param(param::EPG_SCOPE_START)
            .and_then(|d| MotTime::decode_param(d).ok().flatten())
    }

    /// EPG ScopeEnd.
    pub fn epg_scope_end(&self) -> Option<MotTime> {
        self.param(param::EPG_SCOPE_END)
            .and_then(|d| MotTime::decode_param(d).ok().flatten())
    }

    /// EPG ScopeId (service id or ensemble id, big-endian).
    pub fn epg_scope_id(&self) -> Option<u32> {
        self.param(param::EPG_SCOPE_ID).and_then(be_uint)
    }

    /// SlideShow CategoryID/SlideID.
    pub fn sls_category_slide_id(&self) -> Option<(u8, u8)> {
        match self.param(param::SLS_CATEGORY_SLIDE_ID) {
            Some(&[c, s]) => Some((c, s)),
            _ => None,
        }
    }

    /// SlideShow CategoryTitle.
    pub fn sls_category_title(&self) -> Option<String> {
        self.param(param::SLS_CATEGORY_TITLE)
            .map(|d| String::from_utf8_lossy(d).into_owned())
    }

    /// SlideShow ClickThroughURL.
    pub fn sls_click_through_url(&self) -> Option<String> {
        self.param(param::SLS_CLICK_THROUGH_URL)
            .map(|d| String::from_utf8_lossy(d).into_owned())
    }

    /// The MIME type of the body: the MimeType parameter if present, otherwise derived
    /// from ContentType/ContentSubType, then from the ContentName extension.
    pub fn inferred_mime(&self) -> String {
        if let Some(m) = self.mime_type().filter(|m| !m.is_empty()) {
            return m;
        }
        let by_type = match (self.content_type, self.content_subtype) {
            (content_type::IMAGE, content_type::IMAGE_GIF) => Some("image/gif"),
            (content_type::IMAGE, content_type::IMAGE_JFIF) => Some("image/jpeg"),
            (content_type::IMAGE, content_type::IMAGE_BMP) => Some("image/bmp"),
            (content_type::IMAGE, content_type::IMAGE_PNG) => Some("image/png"),
            (content_type::TEXT, 0) => Some("text/plain"),
            (content_type::TEXT, 1) => Some("text/plain; charset=iso-8859-1"),
            (content_type::TEXT, 2) => Some("text/html"),
            (content_type::AUDIO, 0..=5) => Some("audio/mpeg"),
            (content_type::AUDIO, 10) => Some("audio/mp4"),
            (content_type::VIDEO, 0 | 1) => Some("video/mpeg"),
            (content_type::VIDEO, 2) => Some("video/mp4"),
            _ => None,
        };
        if let Some(m) = by_type {
            return m.to_owned();
        }
        match self.content_name() {
            Some(name) => type_from_name(&name).2.to_owned(),
            None => "application/octet-stream".to_owned(),
        }
    }
}

/// (ContentType, ContentSubType, MIME type) for a file name, by extension.
pub(crate) fn type_from_name(name: &str) -> (u8, u16, &'static str) {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    use content_type::*;
    match ext.as_str() {
        "jpg" | "jpeg" | "jfif" | "jpe" => (IMAGE, IMAGE_JFIF, "image/jpeg"),
        "png" => (IMAGE, IMAGE_PNG, "image/png"),
        "gif" => (IMAGE, IMAGE_GIF, "image/gif"),
        "bmp" => (IMAGE, IMAGE_BMP, "image/bmp"),
        "htm" | "html" => (TEXT, 2, "text/html"),
        "txt" => (TEXT, 0, "text/plain"),
        "css" => (GENERAL_DATA, 1, "text/css"),
        "js" => (GENERAL_DATA, 1, "application/javascript"),
        "xml" => (GENERAL_DATA, 1, "application/xml"),
        "svg" => (GENERAL_DATA, 1, "image/svg+xml"),
        "json" => (GENERAL_DATA, 1, "application/json"),
        "mp3" => (AUDIO, 2, "audio/mpeg"),
        "mp4" | "m4a" => (AUDIO, 10, "audio/mp4"),
        _ => (GENERAL_DATA, 0, "application/octet-stream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header Dream's `CMOTDABEnc::SetMOTObject` writes for "pic.jpg":
    /// core + TriggerTime(Now, PLI 10) + VersionNumber(0, PLI 01) + ContentName (PLI 11).
    fn dream_header() -> Vec<u8> {
        let body_size: u64 = 12345;
        let header_size: u64 = 7 + 5 + 2 + 3 + 7;
        let core = (body_size << 28) | (header_size << 15) | (2 << 9) | 1;
        let mut h = core.to_be_bytes()[1..].to_vec();
        h.extend_from_slice(&[0x85, 0, 0, 0, 0]); // TriggerTime = Now
        h.extend_from_slice(&[0x46, 0]); // VersionNumber 0
        h.extend_from_slice(&[0xCC, 8, 0x00]); // ContentName, 8 data bytes, charset 0
        h.extend_from_slice(b"pic.jpg");
        h
    }

    #[test]
    fn parses_dream_encoder_header() {
        let bytes = dream_header();
        let (h, size) = MotHeader::parse(&bytes).unwrap();
        assert_eq!(size, bytes.len());
        assert_eq!(h.body_size, 12345);
        assert_eq!((h.content_type, h.content_subtype), (2, 1));
        assert_eq!(h.trigger_time(), Some(TriggerTime::Now));
        assert_eq!(h.version_number(), Some(0));
        assert_eq!(h.content_name().as_deref(), Some("pic.jpg"));
        assert_eq!(h.inferred_mime(), "image/jpeg");
        // Re-encoding reproduces Dream's bytes exactly.
        assert_eq!(h.to_bytes().unwrap(), bytes);
    }

    #[test]
    fn long_parameters_use_15_bit_lengths() {
        let mut h = MotHeader::new(0, 1, 1);
        let name = "d/".repeat(100) + "index.html";
        h.set_content_name(&name);
        h.set_param(0x30, vec![]);
        let bytes = h.to_bytes().unwrap();
        let (back, _) = MotHeader::parse(&bytes).unwrap();
        assert_eq!(back, h);
        assert_eq!(back.content_name().unwrap(), name);
    }

    #[test]
    fn truncated_parameters_are_rejected() {
        let mut bytes = dream_header();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(MotHeader::parse(&bytes).unwrap_err(), DataError::Truncated);
        // HeaderSize smaller than the core.
        let core: u64 = 3 << 15;
        assert!(MotHeader::parse(&core.to_be_bytes()[1..]).is_err());
    }

    #[test]
    fn file_headers() {
        let h = MotHeader::for_file("img/Logo.PNG", 10);
        assert_eq!((h.content_type, h.content_subtype), (2, 3));
        assert_eq!(h.mime_type().as_deref(), Some("image/png"));
        assert_eq!(h.content_name().as_deref(), Some("img/Logo.PNG"));
        let mut h = MotHeader::new(1, 2, 0);
        assert_eq!(h.inferred_mime(), "text/html");
        h.set_mime_type("application/xhtml+xml");
        assert_eq!(h.inferred_mime(), "application/xhtml+xml");
    }
}
