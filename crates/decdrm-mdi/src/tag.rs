//! TAG items and TAG packets (TS 102 821 §5): an AF packet's payload is a sequence of
//! TAG items, each `name (4 ASCII bytes) │ length in bits (4 bytes) │ value`, the value
//! taking whole bytes. An item's meaning comes from its name ([`crate::mdi`],
//! [`crate::rsci`], [`crate::rci`]); receivers skip the names they do not know.

use crate::DcpError;

/// One TAG item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagItem {
    pub name: [u8; 4],
    /// Length of the value in bits (the value field takes whole bytes).
    pub bits: u32,
    pub value: Vec<u8>,
}

impl TagItem {
    /// An item of whole bytes.
    pub fn new(name: &[u8; 4], value: Vec<u8>) -> Self {
        Self { name: *name, bits: value.len() as u32 * 8, value }
    }

    /// An item whose value is `bits` long (the last byte padded with zeros).
    pub fn with_bits(name: &[u8; 4], bits: u32, mut value: Vec<u8>) -> Self {
        value.resize(bits.div_ceil(8) as usize, 0);
        Self { name: *name, bits, value }
    }

    /// The name as text (`fac_`, `str0`, …).
    pub fn name_str(&self) -> String {
        self.name.iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
    }

    /// Bytes of the item as sent.
    pub fn encoded_len(&self) -> usize {
        8 + self.value.len()
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.extend_from_slice(&self.bits.to_be_bytes());
        out.extend_from_slice(&self.value);
    }
}

/// Decode the TAG items of a TAG packet. Fewer than 8 bytes left at the end are
/// padding.
pub fn parse_tag_packet(payload: &[u8]) -> Result<Vec<TagItem>, DcpError> {
    let mut items = Vec::new();
    let mut pos = 0;
    while payload.len() - pos >= 8 {
        let name = [payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3]];
        let bits = u32::from_be_bytes([payload[pos + 4], payload[pos + 5], payload[pos + 6], payload[pos + 7]]);
        let len = bits.div_ceil(8) as usize;
        let start = pos + 8;
        let value = payload.get(start..start + len).ok_or(DcpError::Truncated)?;
        items.push(TagItem { name, bits, value: value.to_vec() });
        pos = start + len;
    }
    Ok(items)
}

/// A TAG packet of `items`, in order.
pub fn build_tag_packet(items: &[TagItem]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(TagItem::encoded_len).sum());
    for item in items {
        item.write(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_round_trip() {
        let items = vec![
            TagItem::new(b"*ptr", b"DMDI\0\0\0\0".to_vec()),
            TagItem::with_bits(b"odd_", 12, vec![0xAB, 0xCF]),
            TagItem::new(b"str0", Vec::new()),
        ];
        assert_eq!(items[1].value, [0xAB, 0xCF]);
        assert_eq!(TagItem::with_bits(b"pad_", 9, vec![1]).value, [1, 0]);
        let packet = build_tag_packet(&items);
        assert_eq!(packet.len(), 16 + 10 + 8);
        assert_eq!(&packet[20..24], &12u32.to_be_bytes());
        let mut padded = packet.clone();
        padded.extend_from_slice(&[0; 7]);
        assert_eq!(parse_tag_packet(&padded).unwrap(), items);
        assert_eq!(items[0].name_str(), "*ptr");
        assert_eq!(parse_tag_packet(&packet[..packet.len() - 9]), Err(DcpError::Truncated));
    }
}
