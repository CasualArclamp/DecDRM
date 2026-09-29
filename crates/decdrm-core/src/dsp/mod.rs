//! General DSP helpers shared by the receiver and transmitter chains.

pub mod fft;
pub mod fir;
pub mod resampler;

use crate::{Cplx, Real};

/// Pole of a one-pole IIR smoother with time constant `tau` seconds updated at
/// `rate` Hz (Dream's `IIR1Lam`).
pub fn iir1_lambda(tau: Real, rate: Real) -> Real {
    (-1.0 / (tau * rate)).exp()
}

/// One-pole smoother `y ← λ·y + (1−λ)·x` (Dream's `IIR1`).
#[inline]
pub fn iir1(y: &mut Real, x: Real, lambda: Real) {
    *y = lambda * (*y - x) + x;
}

/// Complex version of [`iir1`].
#[inline]
pub fn iir1_c(y: &mut Cplx, x: Cplx, lambda: Real) {
    *y = (*y - x) * lambda + x;
}

/// Normalised sinc: sin(πx)/(πx).
pub fn sinc(x: Real) -> Real {
    if x == 0.0 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Symmetric Hamming window of length `n` (Matlab's `hamming`).
pub fn hamming(n: usize) -> Vec<Real> {
    if n == 1 {
        return vec![1.0];
    }
    (0..n)
        .map(|k| 0.54 - 0.46 * (2.0 * std::f64::consts::PI * k as Real / (n - 1) as Real).cos())
        .collect()
}

/// Kaiser window of length `n` with shape parameter `beta`.
pub fn kaiser(n: usize, beta: Real) -> Vec<Real> {
    let denom = bessel_i0(beta);
    let m = (n - 1) as Real;
    (0..n)
        .map(|k| {
            let r = 2.0 * k as Real / m - 1.0;
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / denom
        })
        .collect()
}

/// Modified Bessel function of the first kind, order 0 (power series).
pub fn bessel_i0(x: Real) -> Real {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k * k) as Real;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Solve the symmetric Toeplitz system `T(r)·x = b` (first column `r`) with the
/// Levinson recursion. Used for the Wiener interpolation filters.
pub fn levinson(r: &[Real], b: &[Real]) -> Vec<Real> {
    let n = r.len();
    assert_eq!(b.len(), n);
    if n == 0 {
        return Vec::new();
    }
    let mut x = vec![0.0; n];
    let mut f = vec![0.0; n]; // forward vector
    x[0] = b[0] / r[0];
    f[0] = 1.0 / r[0];
    for k in 1..n {
        // Error of the forward vector extended by one element.
        let ef: Real = (0..k).map(|i| r[k - i] * f[i]).sum();
        // New forward vector: (f;0) and its reversal (symmetric Toeplitz ⇒ the
        // backward vector is the reversed forward vector).
        let denom = 1.0 - ef * ef;
        let mut nf = vec![0.0; k + 1];
        for i in 0..=k {
            let fi = if i < k { f[i] } else { 0.0 };
            let bi = if i > 0 { f[k - i] } else { 0.0 };
            nf[i] = (fi - ef * bi) / denom;
        }
        f[..=k].copy_from_slice(&nf);
        // Update the solution.
        let ex: Real = (0..k).map(|i| r[k - i] * x[i]).sum();
        let delta = b[k] - ex;
        for i in 0..=k {
            x[i] += delta * f[k - i];
        }
    }
    x
}

/// Least-squares slope of `y` over `x`.
pub fn linear_regression_slope(x: &[Real], y: &[Real]) -> Real {
    let n = x.len() as Real;
    let mx = x.iter().sum::<Real>() / n;
    let my = y.iter().sum::<Real>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (a, b) in x.iter().zip(y) {
        num += (a - mx) * (b - my);
        den += (a - mx) * (a - mx);
    }
    if den == 0.0 { 0.0 } else { num / den }
}

/// Wrap an angle to (−π, π].
pub fn wrap_phase(mut a: Real) -> Real {
    use std::f64::consts::PI;
    while a > PI {
        a -= 2.0 * PI;
    }
    while a <= -PI {
        a += 2.0 * PI;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levinson_matches_direct_solution() {
        let r = [4.0, 1.0, 0.5, 0.25, 0.1];
        let b = [1.0, 2.0, -1.0, 0.5, 3.0];
        let x = levinson(&r, &b);
        for i in 0..5 {
            let lhs: f64 = (0..5).map(|j| r[(i as isize - j as isize).unsigned_abs()] * x[j]).sum();
            assert!((lhs - b[i]).abs() < 1e-10, "row {i}");
        }
    }

    #[test]
    fn regression_slope() {
        let x = [0.0, 1.0, 2.0, 3.0];
        let y = [1.0, 3.0, 5.0, 7.0];
        assert!((linear_regression_slope(&x, &y) - 2.0).abs() < 1e-12);
    }
}
