//! Assembly of the SDC block (ES 201 980 §6.4.2; Dream's `CSDCTransmit`).
//!
//! On air, one SDC block per transmission super frame carries
//!
//! ```text
//! AFS index (4 bits) ‖ data field (⌊(L − 20)/8⌋ bytes) ‖ CRC-16 ‖ padding (0…7 zero bits)
//! ```
//!
//! where L is the number of SDC information bits per super frame (from the MLC
//! parameters of the SDC). The CRC covers the AFS index as a byte (four leading
//! zero bits) followed by the data field bytes. Unused data-field bytes are zero,
//! which is the padding an SDC generator uses after the last data entity.

use crate::bits::BitWriter;
use crate::fec::crc::Crc;

/// Bits of the SDC block that are not data field: AFS index (4) + CRC (16).
pub const SDC_OVERHEAD_BITS: usize = 20;

/// Data-field capacity in bytes of an SDC block of `block_bits` bits (L).
pub const fn sdc_data_bytes(block_bits: usize) -> usize {
    block_bits.saturating_sub(SDC_OVERHEAD_BITS) / 8
}

/// Build the `block_bits`-bit SDC block (one bit per byte) for `afs_index`
/// (0..=15) and `data` (at most [`sdc_data_bytes`] bytes; shorter data is padded
/// with zero bytes).
///
/// # Panics
/// If `data` is longer than the capacity or `afs_index` does not fit in 4 bits
/// (the [`Transmitter`](super::Transmitter) validates both first).
pub fn build_sdc_block(afs_index: u8, data: &[u8], block_bits: usize) -> Vec<u8> {
    let capacity = sdc_data_bytes(block_bits);
    assert!(data.len() <= capacity, "SDC data field of {} bytes exceeds {capacity}", data.len());
    assert!(afs_index < 16, "AFS index {afs_index} does not fit in 4 bits");
    let mut field = data.to_vec();
    field.resize(capacity, 0);

    let mut crc = Crc::crc16();
    crc.add_byte(afs_index);
    crc.add_bytes(&field);

    let mut w = BitWriter::new();
    w.write(u32::from(afs_index), 4);
    w.write_bytes(&field);
    w.write(crc.value(), 16);
    let mut bits = w.into_bits();
    bits.resize(block_bits, 0);
    bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitReader;

    /// Parse the block the way the receiver does (`rx::chain::decode_sdc`).
    fn parse(bits: &[u8]) -> (u8, Vec<u8>, bool) {
        let data_bytes = sdc_data_bytes(bits.len());
        let mut r = BitReader::from_bits(bits);
        let afs = r.read(4) as u8;
        let data: Vec<u8> = (0..data_bytes).map(|_| r.read_byte()).collect();
        let rx_crc = r.read(16);
        let mut crc = Crc::crc16();
        crc.add_byte(afs);
        crc.add_bytes(&data);
        (afs, data, crc.value() == rx_crc)
    }

    #[test]
    fn block_layout_and_crc() {
        for block_bits in [20, 27, 100, 333, 1000] {
            let cap = sdc_data_bytes(block_bits);
            let data: Vec<u8> = (0..cap.min(5)).map(|i| (i * 37 + 1) as u8).collect();
            let bits = build_sdc_block(9, &data, block_bits);
            assert_eq!(bits.len(), block_bits);
            let (afs, field, ok) = parse(&bits);
            assert!(ok, "L = {block_bits}");
            assert_eq!(afs, 9);
            assert_eq!(&field[..data.len()], &data[..]);
            assert!(field[data.len()..].iter().all(|&b| b == 0));
            // Padding bits after the CRC are zero.
            assert!(bits[SDC_OVERHEAD_BITS + 8 * cap..].iter().all(|&b| b == 0));
            // Any single bit error is detected.
            let mut bad = bits.clone();
            bad[block_bits / 2 % (SDC_OVERHEAD_BITS + 8 * cap)] ^= 1;
            assert!(!parse(&bad).2);
        }
    }
}
