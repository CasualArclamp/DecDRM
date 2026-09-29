//! NML, the "news markup language" of Journaline objects (ETSI TS 102 979).
//!
//! Port of the Fraunhofer decoder shipped with Dream (`journaline/NML.cpp`,
//! `NMLFactory::CreateNML`), plus the matching encoder.
//!
//! ```text
//! header   : object id 16 | object type 3 | static 1 | compressed 1 | revision 3
//! [extended header: length signalled outside the object (SDC), usually 0]
//! body     : [0x08 + raw deflate stream when compressed]
//!            0x01 title-text
//!            plain text object : 0x03 body-text
//!            menu object       : { 0x02 link-id(16) item-text }
//!            list object       : { 0x04|0x05 item-text }
//! ```
//!
//! Text runs until the next byte below 0x10 (an NML code). Inside text, 0x10..=0x1F
//! are escape codes: 0x10 preferred line break, 0x11 preferred word break, 0x12/0x13
//! highlight on/off, 0x1A/0x1B data section (followed by a length byte L and L+1 data
//! bytes), 0x1C/0x1D extended code (followed by one byte). The text itself is UTF-8.
//!
//! Decoding maps 0x10 to `'\n'` and drops the other escapes (the behaviour of
//! Fraunhofer's `RemoveNMLEscapeSequences`, except that 0x11 is dropped instead of
//! being passed through as a control character). Encoding maps `'\n'` back to 0x10.
//!
//! Uncertainties (TS 102 979 not available): the meaning of 0x04 vs 0x05 in lists is
//! taken from a comment in `NML.cpp` (`newRow = (*p == 0x04)`), and 0x11 is assumed to
//! be a soft hyphen.

use crate::compress::{deflate_raw, inflate_raw};
use crate::error::{DataError, Result};

/// Object id of the root menu.
pub const ROOT_OBJECT_ID: u16 = 0x0000;
/// Maximum size of a raw NML object (`NML::NML_MAX_LEN`).
pub const NML_MAX_LEN: usize = 4092;

const CODE_TITLE: u8 = 0x01;
const CODE_MENU_ITEM: u8 = 0x02;
const CODE_PLAIN_BODY: u8 = 0x03;
const CODE_LIST_ROW: u8 = 0x04;
const CODE_LIST_CELL: u8 = 0x05;
const COMPRESSION_DEFLATE: u8 = 0x08;

/// NML object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NmlObjectType {
    /// A menu of links to other objects.
    Menu = 1,
    /// A plain text message.
    PlainText = 2,
    /// A title-only message (e.g. a headline).
    TitleOnly = 3,
    /// A list/table.
    List = 4,
}

impl NmlObjectType {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Menu),
            2 => Some(Self::PlainText),
            3 => Some(Self::TitleOnly),
            4 => Some(Self::List),
            _ => None,
        }
    }
}

/// One entry of a menu.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MenuItem {
    /// Object id the entry links to.
    pub link: u16,
    /// Entry text.
    pub text: String,
}

impl MenuItem {
    /// Convenience constructor.
    pub fn new(link: u16, text: impl Into<String>) -> Self {
        Self {
            link,
            text: text.into(),
        }
    }
}

/// One entry of a list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListItem {
    /// `true` (code 0x04) starts a new row; `false` (0x05) continues the row.
    pub new_row: bool,
    /// Entry text.
    pub text: String,
}

impl ListItem {
    /// An item starting a new row.
    pub fn row(text: impl Into<String>) -> Self {
        Self {
            new_row: true,
            text: text.into(),
        }
    }

    /// An item continuing the current row (next column).
    pub fn cell(text: impl Into<String>) -> Self {
        Self {
            new_row: false,
            text: text.into(),
        }
    }
}

/// Type-specific content of an NML object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NmlBody {
    /// Title only.
    TitleOnly,
    /// Plain text body.
    PlainText(String),
    /// Menu entries.
    Menu(Vec<MenuItem>),
    /// List entries.
    List(Vec<ListItem>),
}

/// A decoded Journaline object ("page").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NmlObject {
    /// Object id (0x0000 = root menu).
    pub object_id: u16,
    /// Static flag (content does not change over time).
    pub static_flag: bool,
    /// Revision index 0..=7, incremented when the content changes.
    pub revision: u8,
    /// Extended header bytes (unparsed).
    pub extended_header: Vec<u8>,
    /// Title with escape codes resolved.
    pub title: String,
    /// Body.
    pub body: NmlBody,
}

impl NmlObject {
    /// A menu object.
    pub fn menu(object_id: u16, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self::with_body(object_id, title, NmlBody::Menu(items))
    }

    /// A plain text object.
    pub fn plain_text(object_id: u16, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::with_body(object_id, title, NmlBody::PlainText(text.into()))
    }

    /// A list object.
    pub fn list(object_id: u16, title: impl Into<String>, items: Vec<ListItem>) -> Self {
        Self::with_body(object_id, title, NmlBody::List(items))
    }

    /// A title-only object.
    pub fn title_only(object_id: u16, title: impl Into<String>) -> Self {
        Self::with_body(object_id, title, NmlBody::TitleOnly)
    }

    fn with_body(object_id: u16, title: impl Into<String>, body: NmlBody) -> Self {
        Self {
            object_id,
            static_flag: false,
            revision: 0,
            extended_header: Vec::new(),
            title: title.into(),
            body,
        }
    }

    /// The object type.
    pub fn object_type(&self) -> NmlObjectType {
        match self.body {
            NmlBody::TitleOnly => NmlObjectType::TitleOnly,
            NmlBody::PlainText(_) => NmlObjectType::PlainText,
            NmlBody::Menu(_) => NmlObjectType::Menu,
            NmlBody::List(_) => NmlObjectType::List,
        }
    }

    /// `true` for the root menu.
    pub fn is_root(&self) -> bool {
        self.object_id == ROOT_OBJECT_ID
    }

    /// Link targets of a menu (empty for other types).
    pub fn links(&self) -> Vec<u16> {
        match &self.body {
            NmlBody::Menu(items) => items.iter().map(|i| i.link).collect(),
            _ => Vec::new(),
        }
    }

    /// Parse a raw NML object. `extended_header_len` is the length signalled for the
    /// service (normally 0).
    pub fn parse(raw: &[u8], extended_header_len: usize) -> Result<Self> {
        // `NMLFactory::CreateNML`: "at least header needs to be present".
        if raw.len() < 4 {
            return Err(DataError::Truncated);
        }
        let object_id = u16::from_be_bytes([raw[0], raw[1]]);
        let object_type =
            NmlObjectType::from_code(raw[2] >> 5).ok_or(DataError::Malformed("NML object type"))?;
        let static_flag = raw[2] & 0x10 != 0;
        let compressed = raw[2] & 0x08 != 0;
        let revision = raw[2] & 0x07;
        let rest = &raw[3..];
        if extended_header_len > rest.len() {
            return Err(DataError::Malformed("NML extended header"));
        }
        let (extended_header, body_raw) = rest.split_at(extended_header_len);
        let inflated;
        let body: &[u8] = if compressed {
            match body_raw.split_first() {
                Some((&COMPRESSION_DEFLATE, packed)) => {
                    inflated = inflate_raw(packed, NML_MAX_LEN)?;
                    &inflated
                }
                _ => return Err(DataError::Unsupported("NML compression")),
            }
        } else {
            body_raw
        };
        let mut cur = Cursor { data: body, pos: 0 };
        cur.expect(CODE_TITLE)?;
        let title = cur.text()?;
        let body = match object_type {
            NmlObjectType::TitleOnly => NmlBody::TitleOnly,
            NmlObjectType::PlainText => {
                cur.expect(CODE_PLAIN_BODY)?;
                NmlBody::PlainText(cur.text()?)
            }
            NmlObjectType::Menu => {
                let mut items = Vec::new();
                while let Some(code) = cur.peek() {
                    if code == CODE_MENU_ITEM && cur.remaining() >= 3 {
                        cur.pos += 1;
                        let link = u16::from_be_bytes([cur.data[cur.pos], cur.data[cur.pos + 1]]);
                        cur.pos += 2;
                        items.push(MenuItem {
                            link,
                            text: cur.text()?,
                        });
                    } else if cur.remaining() <= 3 {
                        // Dream only parses items while more than 3 bytes remain.
                        break;
                    } else {
                        return Err(DataError::Malformed("NML menu item"));
                    }
                }
                NmlBody::Menu(items)
            }
            NmlObjectType::List => {
                let mut items = Vec::new();
                while let Some(code) = cur.peek() {
                    if code == CODE_LIST_ROW || code == CODE_LIST_CELL {
                        cur.pos += 1;
                        items.push(ListItem {
                            new_row: code == CODE_LIST_ROW,
                            text: cur.text()?,
                        });
                    } else if cur.remaining() <= 3 {
                        break;
                    } else {
                        return Err(DataError::Malformed("NML list item"));
                    }
                }
                NmlBody::List(items)
            }
        };
        Ok(Self {
            object_id,
            static_flag,
            revision,
            extended_header: extended_header.to_vec(),
            title,
            body,
        })
    }

    /// Serialise, optionally deflate-compressing the body. Fails if the result exceeds
    /// [`NML_MAX_LEN`].
    pub fn to_bytes(&self, compress: bool) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.object_id.to_be_bytes());
        out.push(
            ((self.object_type() as u8) << 5)
                | (u8::from(self.static_flag) << 4)
                | (u8::from(compress) << 3)
                | (self.revision & 0x07),
        );
        out.extend_from_slice(&self.extended_header);
        let mut body = vec![CODE_TITLE];
        push_text(&self.title, &mut body);
        match &self.body {
            NmlBody::TitleOnly => {}
            NmlBody::PlainText(text) => {
                body.push(CODE_PLAIN_BODY);
                push_text(text, &mut body);
            }
            NmlBody::Menu(items) => {
                for item in items {
                    body.push(CODE_MENU_ITEM);
                    body.extend_from_slice(&item.link.to_be_bytes());
                    push_text(&item.text, &mut body);
                }
            }
            NmlBody::List(items) => {
                for item in items {
                    body.push(if item.new_row {
                        CODE_LIST_ROW
                    } else {
                        CODE_LIST_CELL
                    });
                    push_text(&item.text, &mut body);
                }
            }
        }
        if compress {
            out.push(COMPRESSION_DEFLATE);
            out.extend_from_slice(&deflate_raw(&body));
        } else {
            out.extend_from_slice(&body);
        }
        if out.len() > NML_MAX_LEN {
            return Err(DataError::OutOfRange("NML object size"));
        }
        Ok(out)
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn expect(&mut self, code: u8) -> Result<()> {
        match self.peek() {
            Some(c) if c == code => {
                self.pos += 1;
                Ok(())
            }
            Some(_) => Err(DataError::Malformed("NML code")),
            None => Err(DataError::Truncated),
        }
    }

    /// Read a text run (Fraunhofer `getNextSection` + `RemoveNMLEscapeSequences`).
    fn text(&mut self) -> Result<String> {
        let mut raw = Vec::new();
        while let Some(b) = self.peek() {
            match b {
                0x1A | 0x1B => {
                    // Data section: code, length byte L, then L + 1 bytes (skipped).
                    let len =
                        usize::from(*self.data.get(self.pos + 1).ok_or(DataError::Truncated)?) + 1;
                    if self.pos + 2 + len > self.data.len() {
                        return Err(DataError::Malformed("NML data section"));
                    }
                    self.pos += 2 + len;
                }
                0x1C | 0x1D => {
                    // Extended code: its parameter byte is part of the code even if it
                    // is below 0x10 (Fraunhofer's getNextSection would stop there).
                    raw.push(b);
                    if let Some(&param) = self.data.get(self.pos + 1) {
                        raw.push(param);
                    }
                    self.pos = (self.pos + 2).min(self.data.len());
                }
                0x00..=0x0F => break,
                _ => {
                    raw.push(b);
                    self.pos += 1;
                }
            }
        }
        Ok(unescape(&raw))
    }
}

/// Resolve NML escape codes into plain UTF-8 text.
fn unescape(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            0x10 => out.push(b'\n'),
            // Extended code begin/end: the following byte belongs to the code.
            0x1C | 0x1D => i += 1,
            // Word break, highlighting and reserved escape codes are dropped.
            0x11..=0x1F => {}
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Append `text` as NML text: `'\n'` becomes the preferred line break 0x10, tabs become
/// spaces, other control characters (which would read as NML codes) are dropped.
fn push_text(text: &str, out: &mut Vec<u8>) {
    for c in text.chars() {
        match c {
            '\n' => out.push(0x10),
            '\t' => out.push(b' '),
            c if u32::from(c) < 0x20 => {}
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built objects following the layout `NMLFactory::CreateNML` parses.
    fn raw_menu() -> Vec<u8> {
        let mut v = vec![0x00, 0x00, (1 << 5) | 0x10 | 3]; // root, menu, static, rev 3
        v.push(0x01);
        v.extend_from_slice(b"News");
        v.extend_from_slice(&[0x02, 0x01, 0x00]);
        v.extend_from_slice(b"World");
        v.extend_from_slice(&[0x02, 0x01, 0x01]);
        v.extend_from_slice("Sport \u{2013} live".as_bytes());
        v
    }

    #[test]
    fn menu_object() {
        let obj = NmlObject::parse(&raw_menu(), 0).unwrap();
        assert_eq!(obj.object_id, ROOT_OBJECT_ID);
        assert!(obj.static_flag && obj.is_root());
        assert_eq!(obj.revision, 3);
        assert_eq!(obj.title, "News");
        assert_eq!(
            obj.body,
            NmlBody::Menu(vec![
                MenuItem::new(0x0100, "World"),
                MenuItem::new(0x0101, "Sport – live")
            ])
        );
        assert_eq!(obj.links(), [0x0100, 0x0101]);
        assert_eq!(obj.to_bytes(false).unwrap(), raw_menu());
    }

    #[test]
    fn plain_text_with_escapes_and_data_sections() {
        let mut v = vec![0x12, 0x34, 2 << 5];
        v.push(0x01);
        v.extend_from_slice(b"Head");
        v.extend_from_slice(&[0x12]); // highlight on
        v.extend_from_slice(b"line");
        v.extend_from_slice(&[0x13]); // highlight off
        v.push(0x03);
        v.extend_from_slice(b"one");
        v.push(0x10);
        v.extend_from_slice(b"two");
        v.extend_from_slice(&[0x1A, 0x02, 0xAA, 0x03, 0xBB]); // data section, 3 bytes (incl. codes < 0x10)
        v.extend_from_slice(b" three");
        v.extend_from_slice(&[0x1C, 0x05]); // extended code
        v.extend_from_slice(b"!");
        let obj = NmlObject::parse(&v, 0).unwrap();
        assert_eq!(obj.title, "Headline");
        assert_eq!(obj.body, NmlBody::PlainText("one\ntwo three!".into()));
        assert_eq!(obj.object_type(), NmlObjectType::PlainText);
    }

    #[test]
    fn list_and_title_only() {
        let mut v = vec![0x00, 0x07, (4 << 5) | 1, 0x01];
        v.extend_from_slice(b"Table");
        v.push(0x04);
        v.extend_from_slice(b"a");
        v.push(0x05);
        v.extend_from_slice(b"b");
        v.push(0x04);
        v.extend_from_slice(b"c");
        let obj = NmlObject::parse(&v, 0).unwrap();
        assert_eq!(
            obj.body,
            NmlBody::List(vec![
                ListItem::row("a"),
                ListItem::cell("b"),
                ListItem::row("c")
            ])
        );
        let t = NmlObject::parse(&[0, 9, 3 << 5, 1, b'H', b'i'], 0).unwrap();
        assert_eq!((t.title.as_str(), &t.body), ("Hi", &NmlBody::TitleOnly));
    }

    #[test]
    fn compressed_and_extended_header() {
        let mut obj = NmlObject::plain_text(0x0200, "Weather", "Sunny\nwarm ".repeat(40));
        obj.extended_header = vec![0xE1, 0xE2];
        obj.revision = 5;
        let raw = obj.to_bytes(true).unwrap();
        assert_eq!(raw[2] & 0x08, 0x08);
        assert_eq!(&raw[3..5], &[0xE1, 0xE2]);
        assert_eq!(raw[5], 0x08);
        assert!(raw.len() < obj.to_bytes(false).unwrap().len());
        assert_eq!(NmlObject::parse(&raw, 2).unwrap(), obj);
        // Wrong extended header length breaks parsing.
        assert!(NmlObject::parse(&raw, 0).is_err());
    }

    #[test]
    fn malformed_objects() {
        assert_eq!(
            NmlObject::parse(&[0, 0, 0x20], 0),
            Err(DataError::Truncated)
        );
        assert_eq!(
            NmlObject::parse(&[0, 0, 0xE0, 1, b'x'], 0),
            Err(DataError::Malformed("NML object type"))
        );
        assert_eq!(
            NmlObject::parse(&[0, 0, 0x40, 3, b'x'], 0),
            Err(DataError::Malformed("NML code"))
        );
        assert_eq!(
            NmlObject::parse(&[0, 0, 0x48, 7, 1, 2], 0),
            Err(DataError::Unsupported("NML compression"))
        );
        let mut v = raw_menu();
        v.extend_from_slice(&[0x07, b'j', b'u', b'n', b'k']);
        assert_eq!(
            NmlObject::parse(&v, 0),
            Err(DataError::Malformed("NML menu item"))
        );
        let mut v = raw_menu();
        v.extend_from_slice(&[0x07, 0x07]); // short trailing garbage is ignored like Dream
        assert!(NmlObject::parse(&v, 0).is_ok());
    }

    #[test]
    fn encoder_sanitises_text() {
        let obj = NmlObject::title_only(1, "a\tb\r\nc\u{1}");
        let raw = obj.to_bytes(false).unwrap();
        assert_eq!(&raw[3..], &[0x01, b'a', b' ', b'b', 0x10, b'c']);
        assert_eq!(NmlObject::parse(&raw, 0).unwrap().title, "a b\nc");
        let huge = NmlObject::plain_text(2, "x", "y".repeat(5000));
        assert_eq!(
            huge.to_bytes(false),
            Err(DataError::OutOfRange("NML object size"))
        );
        assert!(huge.to_bytes(true).is_ok());
    }
}
