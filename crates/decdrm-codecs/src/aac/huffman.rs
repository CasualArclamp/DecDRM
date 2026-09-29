//! AAC Huffman codebooks (spectral books 1–11 and the scalefactor book).
//!
//! The code tables are taken from the linked FDK-AAC encoder at run time through the
//! `decdrm-fdk-sys` shim (so no FDK table is re-typed here) and turned into binary decode
//! trees for parsing.

use std::sync::OnceLock;

use decdrm_fdk_sys as fdk;

use crate::CodecError;
use crate::bits::BitReader;

/// One prefix code: `codes[i]`/`lens[i]` is the codeword of symbol `i`.
pub(crate) struct Codebook {
    codes: Vec<u32>,
    lens: Vec<u8>,
    /// Decode tree: `tree[n] = [child0, child1]`; `>= 0` is a node index, `< 0` a leaf
    /// `-(symbol + 1)`, [`NONE`] an unused branch.
    tree: Vec<[i32; 2]>,
}

const NONE: i32 = i32::MIN;

impl Codebook {
    fn new(codes: Vec<u32>, lens: Vec<u8>) -> Self {
        let mut tree: Vec<[i32; 2]> = vec![[NONE, NONE]];
        for (sym, (&code, &len)) in codes.iter().zip(lens.iter()).enumerate() {
            assert!(len > 0 && len <= 32, "invalid code length {len}");
            let mut node = 0usize;
            for i in (0..len).rev() {
                let b = ((code >> i) & 1) as usize;
                if i == 0 {
                    assert_eq!(tree[node][b], NONE, "Huffman table is not prefix-free");
                    tree[node][b] = -(sym as i32) - 1;
                } else {
                    let next = tree[node][b];
                    if next == NONE {
                        tree.push([NONE, NONE]);
                        let idx = (tree.len() - 1) as i32;
                        tree[node][b] = idx;
                        node = idx as usize;
                    } else {
                        assert!(next >= 0, "Huffman table is not prefix-free");
                        node = next as usize;
                    }
                }
            }
        }
        Self { codes, lens, tree }
    }

    /// Decodes one symbol.
    pub(crate) fn decode(&self, r: &mut BitReader<'_>) -> Result<usize, CodecError> {
        let mut node = 0usize;
        loop {
            let b = r.bit()? as usize;
            let next = self.tree[node][b];
            if next == NONE {
                return Err(CodecError::Bitstream("invalid Huffman codeword"));
            }
            if next < 0 {
                return Ok((-(next + 1)) as usize);
            }
            node = next as usize;
        }
    }

    /// `(codeword, length)` of `symbol`.
    pub(crate) fn code(&self, symbol: usize) -> (u32, u32) {
        (self.codes[symbol], u32::from(self.lens[symbol]))
    }
}

/// All AAC Huffman codebooks.
pub(crate) struct HuffTables {
    /// Index 1..=11 (index 0 is an empty placeholder).
    spectral: Vec<Codebook>,
    /// Scalefactor book; symbol = delta + 60.
    pub(crate) scf: Codebook,
}

impl HuffTables {
    /// Spectral codebook 1..=11 (virtual codebooks 16..31 use book 11).
    pub(crate) fn spectral(&self, cb: u8) -> &Codebook {
        let book = if cb >= 16 { 11 } else { cb };
        &self.spectral[usize::from(book)]
    }
}

/// The process-wide tables (built on first use).
pub(crate) fn tables() -> &'static HuffTables {
    static TABLES: OnceLock<HuffTables> = OnceLock::new();
    TABLES.get_or_init(load)
}

fn load() -> HuffTables {
    let mut spectral = vec![Codebook::new(vec![1], vec![1])];
    for cb in 1..=11 {
        let mut codes = vec![0u16; 289];
        let mut lens = vec![0u8; 289];
        // SAFETY: both buffers hold 289 entries, the capacity passed; the shim only
        // copies static FDK tables into them.
        let n = unsafe {
            fdk::decdrm_fdk_huffman_spectral(cb, codes.as_mut_ptr(), lens.as_mut_ptr(), 289)
        };
        assert!(n > 0, "FDK shim returned no table for codebook {cb}");
        let n = n as usize;
        spectral.push(Codebook::new(
            codes[..n].iter().map(|&c| u32::from(c)).collect(),
            lens[..n].to_vec(),
        ));
    }
    let mut codes = vec![0u32; 121];
    let mut lens = vec![0u8; 121];
    // SAFETY: both buffers hold 121 entries, the capacity passed.
    let n =
        unsafe { fdk::decdrm_fdk_huffman_scalefactor(codes.as_mut_ptr(), lens.as_mut_ptr(), 121) };
    assert_eq!(n, 121, "FDK shim returned no scalefactor table");
    HuffTables {
        spectral,
        scf: Codebook::new(codes, lens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitBuf;

    #[test]
    fn every_symbol_roundtrips() {
        let t = tables();
        for cb in 1..=11u8 {
            let book = t.spectral(cb);
            let mut w = BitBuf::new();
            for s in 0..book.codes.len() {
                let (c, l) = book.code(s);
                w.push(u64::from(c), l);
            }
            let mut r = BitReader::new(w.as_bytes());
            for s in 0..book.codes.len() {
                assert_eq!(book.decode(&mut r).unwrap(), s, "codebook {cb}");
            }
        }
        let mut w = BitBuf::new();
        for s in 0..121 {
            let (c, l) = t.scf.code(s);
            w.push(u64::from(c), l);
        }
        let mut r = BitReader::new(w.as_bytes());
        for s in 0..121 {
            assert_eq!(t.scf.decode(&mut r).unwrap(), s);
        }
    }
}
