//! Decode a FLAC file to raw little-endian f32 (interleaved) for analysis.
use std::io::Write;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut r = claxon::FlacReader::open(&a[1]).unwrap();
    let info = r.streaminfo();
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&a[2]).unwrap());
    for s in r.samples() {
        out.write_all(&(s.unwrap() as f32 * scale).to_le_bytes()).unwrap();
    }
    eprintln!("rate {} ch {}", info.sample_rate, info.channels);
}
