//! Print a coarse averaged spectrum of a FLAC file (for diagnosing acquisition).
use decdrm_core::dsp::fft::Fft;
use decdrm_core::Cplx;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let start_s: f64 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(0.0);
    let mut r = claxon::FlacReader::open(&path).unwrap();
    let info = r.streaminfo();
    let ch = info.channels as usize;
    let fs = info.sample_rate as f64;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f64;
    let s: Vec<f64> = r.samples().map(|v| v.unwrap() as f64 * scale).collect();
    let n = 8192;
    let mut fft = Fft::new(n);
    let mut acc = vec![0.0; n];
    let start = (start_s * fs) as usize * ch;
    let mut blocks = 0;
    for b in 0..40 {
        let off = start + b * n * ch;
        if off + n * ch > s.len() { break; }
        let mut v: Vec<Cplx> = (0..n).map(|i| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
            if ch == 2 { Cplx::new(s[off + 2 * i], s[off + 2 * i + 1]) * w } else { Cplx::new(s[off + i] * w, 0.0) }
        }).collect();
        fft.forward(&mut v);
        for (a, x) in acc.iter_mut().zip(&v) { *a += x.norm_sqr(); }
        blocks += 1;
    }
    let bin = fs / n as f64;
    // Print in 500 Hz bands.
    let band = (500.0 / bin) as usize;
    let lo = if ch == 2 { -(n as isize) / 2 } else { 0 };
    let hi = (n / 2) as isize;
    let mut k = lo;
    println!("fs={fs} ch={ch} blocks={blocks}");
    while k < hi {
        let mut p = 0.0;
        for j in 0..band { let idx = (k + j as isize).rem_euclid(n as isize) as usize; p += acc[idx]; }
        let db = 10.0 * (p / band as f64 / blocks as f64 + 1e-30).log10();
        let bar = ((db + 120.0).max(0.0) / 2.0) as usize;
        println!("{:7.0} Hz {:7.1} dB {}", k as f64 * bin, db, "#".repeat(bar.min(80)));
        k += band as isize;
    }
}
