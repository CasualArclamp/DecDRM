//! Decode an ITU-T G.192 EVS bitstream (as written by the 3GPP encoder) to raw 16-bit
//! little-endian PCM, through DecDRM's frame-by-frame interface — for comparing with
//! the reference decoder's command-line program.
//!
//! `cargo run --release -p decdrm-evs --features decoder --example g192dec -- IN.192 OUT.pcm [RATE]`

use decdrm_evs::EvsDecoder;
use std::io::Write as _;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (input, output) = (&args[1], &args[2]);
    let rate = args.get(3).map_or(Ok(48_000), |r| r.parse())?;
    let data = std::fs::read(input)?;
    let words: Vec<u16> = data.as_chunks::<2>().0.iter().map(|w| u16::from_le_bytes(*w)).collect();
    let mut dec = EvsDecoder::new(rate)?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(output)?);
    let (mut i, mut frames) = (0, 0);
    while i + 2 <= words.len() {
        let (sync, len) = (words[i], usize::from(words[i + 1]));
        let bits = &words[i + 2..(i + 2 + len).min(words.len())];
        i += 2 + len;
        // 0x6B21 good frame, 0x6B20 bad frame; bits 0x0081 = 1, 0x007F = 0.
        let packed: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().enumerate().fold(0u8, |b, (k, &w)| b | (u8::from(w == 0x0081) << (7 - k))))
            .collect();
        let frame = (sync == 0x6B21 && len > 0).then_some(packed.as_slice());
        for s in dec.decode(frame)? {
            out.write_all(&((s * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes())?;
        }
        frames += 1;
    }
    eprintln!("{frames} frames decoded to {output}");
    Ok(())
}
