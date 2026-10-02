//! Reed–Solomon over GF(2⁸) for the PFT layer's error correction (TS 102 821 §7.3):
//! RS(255, 207), shortened as needed, field polynomial x⁸ + x⁴ + x³ + x² + 1 (11D₁₆),
//! generator roots α⁰ … α⁴⁷ with α = 2. The 48 parity bytes of a codeword correct `e`
//! byte errors and `f` erasures (bytes known to be missing: a lost fragment) as long
//! as 2e + f ≤ 48.
//!
//! The decoder is Phil Karn's Berlekamp–Massey/Chien/Forney decoder with erasures
//! (`decode_rs.h` of his libfec, LGPL; the same algorithm in Rust), for codewords
//! given in transmission order: data bytes first, the parity bytes last. A shortened
//! codeword is the tail of a 255-byte one whose leading bytes are zero.

use std::sync::OnceLock;

/// Parity bytes per PFT chunk (TS 102 821: RS(255, 207)).
pub const PFT_PARITY: usize = 48;
/// Most data bytes per PFT chunk.
pub const PFT_MAX_K: usize = 207;

/// Symbols per (unshortened) codeword.
const NN: usize = 255;
/// Index-form (logarithm) value standing for zero.
const A0: usize = NN;
/// First consecutive root of the generator, α^FCR (TS 102 821: α⁰).
const FCR: usize = 0;

struct Field {
    /// α^i (poly form) for i = 0 … 254; [255] = 0.
    alpha_to: [u8; 256],
    /// log_α(x) for x ≠ 0; [0] = A0.
    index_of: [usize; 256],
}

fn field() -> &'static Field {
    // Rust note: `OnceLock` builds the tables on first use, thread-safely, then hands
    // out the same `&'static` reference.
    static FIELD: OnceLock<Field> = OnceLock::new();
    FIELD.get_or_init(|| {
        let mut f = Field { alpha_to: [0; 256], index_of: [A0; 256] };
        let mut sr: u16 = 1;
        for i in 0..NN {
            f.alpha_to[i] = sr as u8;
            f.index_of[sr as usize] = i;
            sr <<= 1;
            if sr & 0x100 != 0 {
                sr ^= 0x11D;
            }
        }
        f
    })
}

fn modnn(x: usize) -> usize {
    x % NN
}

/// Why a codeword could not be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RsError {
    #[error("codeword length {0} is outside {1}–255")]
    Length(usize, usize),
    #[error("more erasures ({0}) than parity bytes")]
    TooManyErasures(usize),
    #[error("uncorrectable")]
    Uncorrectable,
}

/// A Reed–Solomon code with `nroots` parity bytes (48 for PFT).
#[derive(Debug, Clone)]
pub struct ReedSolomon {
    nroots: usize,
    /// Generator polynomial in index form, `genpoly[i]` the coefficient of xⁱ.
    genpoly: Vec<usize>,
}

impl ReedSolomon {
    pub fn new(nroots: usize) -> Self {
        assert!((1..NN).contains(&nroots), "nroots out of range");
        let f = field();
        // Poly form while building: g(x) = Π (x − α^(FCR+i)).
        let mut g = vec![0u8; nroots + 1];
        g[0] = 1;
        for i in 0..nroots {
            let root = FCR + i;
            g[i + 1] = 1;
            for j in (1..=i).rev() {
                g[j] = if g[j] != 0 { g[j - 1] ^ f.alpha_to[modnn(f.index_of[g[j] as usize] + root)] } else { g[j - 1] };
            }
            g[0] = f.alpha_to[modnn(f.index_of[g[0] as usize] + root)];
        }
        Self { nroots, genpoly: g.iter().map(|&c| f.index_of[c as usize]).collect() }
    }

    /// The PFT code, RS(255, 207).
    pub fn pft() -> Self {
        Self::new(PFT_PARITY)
    }

    /// Parity bytes per codeword.
    pub fn parity_len(&self) -> usize {
        self.nroots
    }

    /// The parity bytes of `data` (at most 255 − parity bytes long).
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        assert!(data.len() + self.nroots <= NN, "codeword longer than 255 bytes");
        let f = field();
        let n = self.nroots;
        let mut bb = vec![0u8; n];
        for &d in data {
            let feedback = f.index_of[(d ^ bb[0]) as usize];
            if feedback != A0 {
                for (j, b) in bb.iter_mut().enumerate().skip(1) {
                    *b ^= f.alpha_to[modnn(feedback + self.genpoly[n - j])];
                }
            }
            bb.rotate_left(1);
            bb[n - 1] = if feedback != A0 { f.alpha_to[modnn(feedback + self.genpoly[0])] } else { 0 };
        }
        bb
    }

    /// Correct `codeword` (data then parity, at most 255 bytes) in place. `erasures`
    /// are indices of bytes known to be wrong (their content does not matter).
    /// Returns the number of bytes corrected.
    pub fn decode(&self, codeword: &mut [u8], erasures: &[usize]) -> Result<usize, RsError> {
        let n = codeword.len();
        if n <= self.nroots || n > NN {
            return Err(RsError::Length(n, self.nroots + 1));
        }
        let nroots = self.nroots;
        if erasures.len() > nroots {
            return Err(RsError::TooManyErasures(erasures.len()));
        }
        let f = field();
        let pad = NN - n;

        let s = self.syndromes(codeword);
        if s.iter().all(|&x| x == 0) {
            return Ok(0);
        }
        let s: Vec<usize> = s.iter().map(|&x| f.index_of[x as usize]).collect();

        // Λ(x) starts as the erasure locator Π (1 − X_k x), X_k = α^(n−1−position).
        let mut lambda = vec![0u8; nroots + 1];
        lambda[0] = 1;
        for (count, &pos) in erasures.iter().enumerate() {
            if pos >= n {
                return Err(RsError::Uncorrectable);
            }
            let u = modnn(n - 1 - pos);
            for j in (1..=count + 1).rev() {
                let t = f.index_of[lambda[j - 1] as usize];
                if t != A0 {
                    lambda[j] ^= f.alpha_to[modnn(u + t)];
                }
            }
        }

        // Berlekamp–Massey for the errors.
        let mut b: Vec<usize> = lambda.iter().map(|&x| f.index_of[x as usize]).collect();
        let mut t = vec![0u8; nroots + 1];
        let no_eras = erasures.len();
        let mut el = no_eras;
        for r in (no_eras + 1)..=nroots {
            let mut discr = 0u8;
            for i in 0..r {
                if lambda[i] != 0 && s[r - i - 1] != A0 {
                    discr ^= f.alpha_to[modnn(f.index_of[lambda[i] as usize] + s[r - i - 1])];
                }
            }
            let discr = f.index_of[discr as usize];
            if discr == A0 {
                b.rotate_right(1);
                b[0] = A0;
                continue;
            }
            t[0] = lambda[0];
            for i in 0..nroots {
                t[i + 1] = if b[i] != A0 { lambda[i + 1] ^ f.alpha_to[modnn(discr + b[i])] } else { lambda[i + 1] };
            }
            if 2 * el < r + no_eras {
                el = r + no_eras - el;
                for i in 0..=nroots {
                    b[i] = if lambda[i] == 0 { A0 } else { modnn(f.index_of[lambda[i] as usize] + NN - discr) };
                }
            } else {
                b.rotate_right(1);
                b[0] = A0;
            }
            lambda.copy_from_slice(&t);
        }

        // Λ in index form, its degree.
        let lambda: Vec<usize> = lambda.iter().map(|&x| f.index_of[x as usize]).collect();
        let deg_lambda = lambda.iter().rposition(|&x| x != A0).unwrap_or(0);

        // Chien search: the roots of Λ give the error locations.
        let mut reg = lambda.clone();
        let mut roots = Vec::with_capacity(deg_lambda);
        let mut locs = Vec::with_capacity(deg_lambda);
        for i in 1..=NN {
            let mut q = 1u8;
            for j in (1..=deg_lambda).rev() {
                if reg[j] != A0 {
                    reg[j] = modnn(reg[j] + j);
                    q ^= f.alpha_to[reg[j]];
                }
            }
            if q != 0 {
                continue;
            }
            roots.push(i);
            // Position in the unshortened codeword (0 = first byte).
            locs.push(i - 1);
            if roots.len() == deg_lambda {
                break;
            }
        }
        if roots.len() != deg_lambda {
            return Err(RsError::Uncorrectable);
        }

        // Ω(x) = S(x)·Λ(x) mod x^nroots, index form.
        let deg_omega = deg_lambda.saturating_sub(1);
        let mut omega = vec![A0; deg_omega + 1];
        for (i, o) in omega.iter_mut().enumerate() {
            let mut tmp = 0u8;
            for j in (0..=i).rev() {
                if s[i - j] != A0 && lambda[j] != A0 {
                    tmp ^= f.alpha_to[modnn(s[i - j] + lambda[j])];
                }
            }
            *o = f.index_of[tmp as usize];
        }

        // Forney: error values; an error in the virtual padding means a wrong guess.
        let mut fixes = Vec::with_capacity(roots.len());
        for (&root, &loc) in roots.iter().zip(&locs) {
            let mut num1 = 0u8;
            for (i, &o) in omega.iter().enumerate() {
                if o != A0 {
                    num1 ^= f.alpha_to[modnn(o + i * root)];
                }
            }
            let num2 = f.alpha_to[modnn(root * (NN + FCR - 1))];
            let mut den = 0u8;
            let mut i = deg_lambda.min(nroots - 1) & !1;
            loop {
                if lambda[i + 1] != A0 {
                    den ^= f.alpha_to[modnn(lambda[i + 1] + i * root)];
                }
                if i < 2 {
                    break;
                }
                i -= 2;
            }
            if den == 0 {
                return Err(RsError::Uncorrectable);
            }
            if num1 != 0 {
                if loc < pad {
                    return Err(RsError::Uncorrectable);
                }
                let value = f.alpha_to
                    [modnn(f.index_of[num1 as usize] + f.index_of[num2 as usize] + NN - f.index_of[den as usize])];
                fixes.push((loc - pad, value));
            }
        }
        for &(pos, value) in &fixes {
            codeword[pos] ^= value;
        }
        // The result must be a codeword (a locator without roots, e.g., leaves it
        // unchanged); otherwise undo and give up.
        if self.syndromes(codeword).iter().any(|&x| x != 0) {
            for &(pos, value) in &fixes {
                codeword[pos] ^= value;
            }
            return Err(RsError::Uncorrectable);
        }
        Ok(fixes.len())
    }

    /// The codeword polynomial at the generator's roots (poly form): all zero for a
    /// valid codeword.
    fn syndromes(&self, codeword: &[u8]) -> Vec<u8> {
        let f = field();
        let mut s = vec![codeword[0]; self.nroots];
        for &c in &codeword[1..] {
            for (i, si) in s.iter_mut().enumerate() {
                *si = if *si == 0 { c } else { c ^ f.alpha_to[modnn(f.index_of[*si as usize] + FCR + i)] };
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift32: deterministic test data.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            self.next() as usize % n
        }
    }

    fn codeword(rs: &ReedSolomon, k: usize, rng: &mut Rng) -> Vec<u8> {
        let mut c: Vec<u8> = (0..k).map(|_| rng.next() as u8).collect();
        let parity = rs.encode(&c);
        c.extend(parity);
        c
    }

    /// Distinct positions below `n`.
    fn positions(rng: &mut Rng, n: usize, count: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        while out.len() < count {
            let p = rng.below(n);
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn field_and_generator() {
        let f = field();
        assert_eq!(f.alpha_to[8], 0x1D, "x⁸ = x⁴ + x³ + x² + 1");
        assert_eq!(f.index_of[f.alpha_to[200] as usize], 200);
        let rs = ReedSolomon::pft();
        assert_eq!(rs.genpoly.len(), 49);
        assert_eq!(rs.genpoly[48], 0, "monic: leading coefficient α⁰ = 1");
        // Every codeword evaluates to zero at the roots: a clean decode changes nothing.
        let mut rng = Rng(7);
        let mut c = codeword(&rs, PFT_MAX_K, &mut rng);
        let orig = c.clone();
        assert_eq!(rs.decode(&mut c, &[]), Ok(0));
        assert_eq!(c, orig);
    }

    /// Errors and erasures up to the limit 2e + f ≤ 48, full-length and shortened.
    #[test]
    fn corrects_errors_and_erasures() {
        let rs = ReedSolomon::pft();
        let mut rng = Rng(0x1234_5678);
        for &k in &[PFT_MAX_K, 120, 20, 1] {
            for &(errors, erasures) in &[(0, 0), (1, 0), (24, 0), (0, 48), (10, 28), (23, 2), (5, 7)] {
                let orig = codeword(&rs, k, &mut rng);
                let n = orig.len();
                let pos = positions(&mut rng, n, (errors + erasures).min(n));
                let mut c = orig.clone();
                for &p in &pos {
                    c[p] ^= (rng.below(255) + 1) as u8;
                }
                let erased: Vec<usize> = pos.iter().copied().take(erasures.min(pos.len())).collect();
                let fixed = rs.decode(&mut c, &erased);
                assert!(fixed.is_ok(), "k={k} e={errors} f={erasures}: {fixed:?}");
                assert_eq!(c, orig, "k={k} e={errors} f={erasures}");
            }
        }
    }

    /// An erased byte that happens to be right is not counted as a correction.
    #[test]
    fn erasures_may_be_right() {
        let rs = ReedSolomon::pft();
        let mut rng = Rng(99);
        let orig = codeword(&rs, 100, &mut rng);
        let mut c = orig.clone();
        c[3] ^= 0x55;
        assert_eq!(rs.decode(&mut c, &[3, 10, 50]), Ok(1));
        assert_eq!(c, orig);
    }

    /// Beyond the limit decoding fails (rather than "correcting" to another codeword,
    /// which a 48-parity code almost never does).
    #[test]
    fn detects_too_many_errors() {
        let rs = ReedSolomon::pft();
        let mut rng = Rng(4242);
        let mut failures = 0;
        for _ in 0..20 {
            let orig = codeword(&rs, PFT_MAX_K, &mut rng);
            let mut c = orig.clone();
            for p in positions(&mut rng, c.len(), 30) {
                c[p] ^= (rng.below(255) + 1) as u8;
            }
            if rs.decode(&mut c, &[]).is_err() {
                failures += 1;
            }
        }
        assert_eq!(failures, 20);
        assert_eq!(rs.decode(&mut [0u8; 48], &[]), Err(RsError::Length(48, 49)));
        assert_eq!(rs.decode(&mut [0u8; 60], &(0..49).collect::<Vec<_>>()), Err(RsError::TooManyErasures(49)));
    }
}
