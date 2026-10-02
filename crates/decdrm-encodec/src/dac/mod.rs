//! DAC — the Descript Audio Codec, 24 kHz model — streaming on candle (CPU).
//!
//! The network is DAC as ported to Hugging Face `transformers` (`modeling_dac.py`;
//! weights `descript/dac_24khz`, MIT licence, weight norm folded in):
//!
//! * an encoder: a convolution (1 → 64 channels), four blocks of three dilated residual
//!   units (dilations 1, 3, 9; Snake activations x + sin²(αx)/α) and a strided
//!   convolution (×2 ×4 ×5 ×8, doubling the channels), then a convolution to a
//!   1024-dimensional latent at 75 frames per second;
//! * a residual vector quantiser of 32 codebooks × 1024 entries: each codebook looks up
//!   the residual in an 8-dimensional projection by cosine similarity (factorised,
//!   L2-normalised codes) and projects the entry back to 1024 dimensions;
//! * a decoder: a convolution (1024 → 1536 channels), four blocks of a transposed
//!   convolution (×8 ×5 ×4 ×2, halving the channels) and three residual units, then a
//!   convolution to the waveform and tanh.
//!
//! The frame rate and code size are EnCodec 24 kHz's: 75 frames per second of 1–32 codes
//! of 10 bits (0.75–24 kbit/s; DAC was trained with quantiser dropout, so any number of
//! codebooks works).
//!
//! ## Streaming
//!
//! DAC's convolutions are centred, not causal: an output depends on input after it.
//! Every layer here keeps the input rows it still needs and, when new rows come, emits
//! the outputs whose inputs are all there — a convolution with padding *p* lags its
//! input by *p* rows, a transposed convolution by its padding, a residual unit by its
//! dilated convolution's. Chunks of any size give exactly what one run over the whole
//! signal gives (`flush` supplies the zero padding at the end). The look-ahead adds up
//! to [`DECODER_LAG`] samples (131 ms) for the decoder and 104 ms for the encoder
//! ([`ENCODER_LEAD_IN`]).
//!
//! Activations are time-major (time × channels): a convolution's input matrix is then
//! made of contiguous rows, copied in parallel, and only the matrix products go through
//! candle (whose own convolutions gather the matrix in one thread and transpose the
//! result, which left most cores idle). Snake activations, biases and residual sums run
//! as one parallel pass each.

use crate::error::EncodecError;
use candle_core::safetensors::SliceSafetensors;
use candle_core::{DType, Device, Tensor};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod reference;

/// Sample rate of the 24 kHz model.
pub const SAMPLE_RATE: u32 = 24_000;
/// Samples per frame (the product of the strides): 75 frames per second.
pub const FRAME_SAMPLES: usize = 320;
/// Codebooks, and entries per codebook (10-bit codes).
pub const CODEBOOKS: usize = 32;
pub const CODEBOOK_SIZE: usize = 1024;
/// Dimension of the latent (the decoder's input).
pub const LATENT_DIM: usize = 1024;
/// The decoder's output lags its input by this many samples: its look-ahead (131 ms).
pub const DECODER_LAG: usize = 3 * 320 + DECODER_BLOCK_LAG + 3;
/// Zero samples to feed the encoder before the signal, so that every 320 samples of
/// signal complete one frame (its look-ahead is 2493 samples).
pub const ENCODER_LEAD_IN: usize = 8 * FRAME_SAMPLES;

const CODEBOOK_DIM: usize = 8;
const ENCODER_DIM: usize = 64;
const DECODER_DIM: usize = 1536;
const ENCODER_STRIDES: [usize; 4] = [2, 4, 5, 8];
const DECODER_STRIDES: [usize; 4] = [8, 5, 4, 2];
const DILATIONS: [usize; 3] = [1, 3, 9];
/// The decoder blocks' lag in output samples: each block's transposed convolution lags
/// by its padding and its residual units by 3 + 9 + 27 rows, at its rate.
const DECODER_BLOCK_LAG: usize = (4 + 39) * 40 + (3 + 39) * 8 + (2 + 39) * 2 + (1 + 39);
/// Rows per parallel task of the elementwise kernels.
const ROWS_PER_TASK: usize = 64;

type CResult<T> = candle_core::Result<T>;

// ---------------------------------------------------------------------------------
// Elementwise kernels on time-major rows
// ---------------------------------------------------------------------------------

/// `f(row)` on every row (`width` values) in parallel.
fn rows_mut(x: &mut [f32], width: usize, f: impl Fn(&mut [f32]) + Sync) {
    x.par_chunks_mut(width * ROWS_PER_TASK).for_each(|block| block.chunks_exact_mut(width).for_each(&f));
}

/// Snake activation per channel after adding a bias per channel (or none).
struct Snake {
    alpha: Vec<f32>,
    /// 1 / (α + 10⁻⁹).
    inv: Vec<f32>,
}

impl Snake {
    fn apply(&self, x: &mut [f32], bias: Option<&[f32]>) {
        let width = self.alpha.len();
        rows_mut(x, width, |row| {
            for (c, v) in row.iter_mut().enumerate() {
                let u = *v + bias.map_or(0.0, |b| b[c]);
                let w = (self.alpha[c] * u).sin();
                *v = u + self.inv[c] * w * w;
            }
        });
    }
}

fn add_bias(x: &mut [f32], bias: &[f32]) {
    rows_mut(x, bias.len(), |row| row.iter_mut().zip(bias).for_each(|(v, b)| *v += b));
}

/// `rows` (n × k, row-major) times `w` (k × m) on candle: (n × m).
fn matmul(rows: Vec<f32>, n: usize, k: usize, w: &Tensor) -> CResult<Vec<f32>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    Tensor::from_vec(rows, (n, k), &Device::Cpu)?.matmul(w)?.flatten_all()?.to_vec1()
}

// ---------------------------------------------------------------------------------
// Streaming layers
// ---------------------------------------------------------------------------------

/// A convolution (kernel `k`, stride, dilation, zero padding on both sides) on
/// time-major rows. Output `m` takes input rows `m·stride − padding + j·dilation`.
struct Conv {
    /// (k·cin, cout): row `j·cin + c` holds the weights of tap `j`, input channel `c`.
    w: Tensor,
    bias: Vec<f32>,
    cin: usize,
    k: usize,
    stride: usize,
    dilation: usize,
    padding: usize,
}

/// The rows a [`Conv`] still needs, and its position.
struct ConvState {
    /// Input rows from absolute row `start` on (left padding included).
    buf: Vec<f32>,
    start: i64,
    /// The next output to produce.
    next: i64,
}

impl Conv {
    fn state(&self) -> ConvState {
        ConvState { buf: vec![0.0; self.padding * self.cin], start: -(self.padding as i64), next: 0 }
    }

    /// The outputs a whole run over `n` input rows has.
    fn batch_len(&self, n: usize) -> usize {
        (n + 2 * self.padding).saturating_sub((self.k - 1) * self.dilation + 1) / self.stride + 1
    }

    /// Append input rows; the outputs now complete (bias added), at most up to output
    /// `limit` (exclusive).
    fn push(&self, st: &mut ConvState, x: &[f32], limit: Option<i64>) -> CResult<Vec<f32>> {
        st.buf.extend_from_slice(x);
        let (cin, s, p, d) = (self.cin, self.stride as i64, self.padding as i64, self.dilation as i64);
        let rows = st.start + (st.buf.len() / cin) as i64;
        // Output m needs rows up to m·s − p + (k − 1)·d.
        let mut end = (rows - 1 + p - (self.k as i64 - 1) * d).div_euclid(s) + 1;
        if let Some(l) = limit {
            end = end.min(l);
        }
        let n = (end - st.next).max(0) as usize;
        let kc = self.k * cin;
        let col = if self.k == 1 && s == 1 && p == 0 {
            let from = (st.next - st.start) as usize * cin;
            st.buf[from..from + n * cin].to_vec()
        } else {
            let (buf, start, next, k) = (&st.buf, st.start, st.next, self.k);
            let mut col = vec![0.0f32; n * kc];
            col.par_chunks_mut(kc).enumerate().for_each(|(i, row)| {
                let m = next + i as i64;
                for j in 0..k {
                    let r = (m * s - p + j as i64 * d - start) as usize;
                    row[j * cin..(j + 1) * cin].copy_from_slice(&buf[r * cin..(r + 1) * cin]);
                }
            });
            col
        };
        let mut y = matmul(col, n, kc, &self.w)?;
        add_bias(&mut y, &self.bias);
        st.next += n as i64;
        // Rows before the next output's first tap are no longer needed.
        let drop = (st.next * s - p - st.start).clamp(0, (st.buf.len() / cin) as i64) as usize;
        st.buf.drain(..drop * cin);
        st.start += drop as i64;
        Ok(y)
    }

    /// The end of the signal (`n` input rows in all): the right padding, and the
    /// outputs it completes.
    fn flush(&self, st: &mut ConvState, n: usize) -> CResult<Vec<f32>> {
        self.push(st, &vec![0.0; self.padding * self.cin], Some(self.batch_len(n) as i64))
    }
}

/// A transposed convolution, kernel 2 × stride, cutting `padding` output rows from both
/// ends. Output row r is the full output's row r + padding, which input rows ⌊(r+p)/s⌋
/// and the one before make.
struct ConvTr {
    /// (cin, 2s·cout): column `j·cout + o` is tap `j` of output channel `o`.
    w: Tensor,
    bias: Vec<f32>,
    cin: usize,
    cout: usize,
    stride: usize,
    padding: usize,
}

struct ConvTrState {
    /// The products of the last input row (2s·cout values), for the outputs it shares
    /// with the next row.
    last: Vec<f32>,
    /// Input rows received, the next output to produce.
    received: i64,
    next: i64,
}

impl ConvTr {
    fn state(&self) -> ConvTrState {
        ConvTrState { last: Vec::new(), received: 0, next: 0 }
    }

    fn batch_len(&self, n: usize) -> usize {
        (n * self.stride + self.stride).saturating_sub(2 * self.padding)
    }

    /// Append input rows (or, with `x` empty and `end` set, the end of the signal); the
    /// outputs now complete.
    fn push(&self, st: &mut ConvTrState, x: &[f32], end: Option<i64>) -> CResult<Vec<f32>> {
        let (s, p, cout) = (self.stride as i64, self.padding as i64, self.cout);
        let t = x.len() / self.cin;
        let col = matmul(x.to_vec(), t, self.cin, &self.w)?;
        let width = 2 * self.stride * cout;
        let (old, new) = (st.received, st.received + t as i64);
        // Output r is complete once input row ⌊(r + p)/s⌋ is in.
        let stop = end.unwrap_or(new * s - p);
        let n = (stop - st.next).max(0) as usize;
        let (last, next) = (&st.last, st.next);
        // The products of input row `i`: the previous call's last row, or this call's.
        let row_of = |i: i64| -> Option<&[f32]> {
            if i == old - 1 && !last.is_empty() {
                Some(last.as_slice())
            } else if i >= old && i < new {
                let at = (i - old) as usize * width;
                Some(&col[at..at + width])
            } else {
                None
            }
        };
        let mut y = vec![0.0f32; n * cout];
        y.par_chunks_mut(cout).enumerate().for_each(|(q, out)| {
            let i = next + q as i64 + p;
            let (t1, j1) = (i.div_euclid(s), i.rem_euclid(s) as usize);
            if let Some(c) = row_of(t1) {
                out.iter_mut().zip(&c[j1 * cout..(j1 + 1) * cout]).for_each(|(o, v)| *o += v);
            }
            if let Some(c) = row_of(t1 - 1) {
                let j0 = j1 + self.stride;
                out.iter_mut().zip(&c[j0 * cout..(j0 + 1) * cout]).for_each(|(o, v)| *o += v);
            }
        });
        add_bias(&mut y, &self.bias);
        if t > 0 {
            st.last = col[(t - 1) * width..t * width].to_vec();
        }
        st.received = new;
        st.next += n as i64;
        Ok(y)
    }

    fn flush(&self, st: &mut ConvTrState) -> CResult<Vec<f32>> {
        let total = self.batch_len(st.received as usize) as i64;
        self.push(st, &[], Some(total))
    }
}

/// Snake, dilated convolution (kernel 7), Snake, 1×1 convolution, plus the input (which
/// waits for the convolution's look-ahead).
struct ResUnit {
    snake1: Snake,
    conv1: Conv,
    snake2: Snake,
    conv2: Conv,
}

struct ResState {
    conv1: ConvState,
    conv2: ConvState,
    /// Input rows whose output is not out yet.
    queue: Vec<f32>,
}

impl ResUnit {
    fn state(&self) -> ResState {
        ResState { conv1: self.conv1.state(), conv2: self.conv2.state(), queue: Vec::new() }
    }

    fn tail(&self, st: &mut ResState, h: Vec<f32>) -> CResult<Vec<f32>> {
        let mut h = h;
        self.snake2.apply(&mut h, None);
        let mut y = self.conv2.push(&mut st.conv2, &h, None)?;
        let n = y.len();
        y.par_iter_mut().zip(st.queue[..n].par_iter()).for_each(|(o, x)| *o += x);
        st.queue.drain(..n);
        Ok(y)
    }

    fn push(&self, st: &mut ResState, x: &[f32]) -> CResult<Vec<f32>> {
        st.queue.extend_from_slice(x);
        let mut h = x.to_vec();
        self.snake1.apply(&mut h, None);
        let h = self.conv1.push(&mut st.conv1, &h, None)?;
        self.tail(st, h)
    }

    fn flush(&self, st: &mut ResState, n: usize) -> CResult<Vec<f32>> {
        let h = self.conv1.flush(&mut st.conv1, n)?;
        self.tail(st, h)
    }
}

// ---------------------------------------------------------------------------------
// Encoder and decoder
// ---------------------------------------------------------------------------------

struct EncoderBlock {
    res: Vec<ResUnit>,
    snake: Snake,
    conv: Conv,
}

struct Encoder {
    conv1: Conv,
    blocks: Vec<EncoderBlock>,
    snake: Snake,
    conv2: Conv,
}

/// Streaming state of the encoder network: create with [`DacModel::encoder_state`].
pub struct EncoderState {
    conv1: ConvState,
    blocks: Vec<(Vec<ResState>, ConvState)>,
    conv2: ConvState,
    /// Samples received (the flush needs every stage's length).
    samples: usize,
}

impl Encoder {
    fn state(&self) -> EncoderState {
        EncoderState {
            conv1: self.conv1.state(),
            blocks: self.blocks.iter().map(|b| (b.res.iter().map(ResUnit::state).collect(), b.conv.state())).collect(),
            conv2: self.conv2.state(),
            samples: 0,
        }
    }

    /// Samples in (one channel) → latent rows (1024 values each) now complete; with
    /// `flush` the end of the signal.
    fn push(&self, st: &mut EncoderState, pcm: &[f32], flush: bool) -> CResult<Vec<f32>> {
        st.samples += pcm.len();
        let mut x = self.conv1.push(&mut st.conv1, pcm, None)?;
        if flush {
            x.extend(self.conv1.flush(&mut st.conv1, st.samples)?);
        }
        let mut n = st.samples;
        for (b, (rs, cs)) in self.blocks.iter().zip(st.blocks.iter_mut()) {
            for (r, s) in b.res.iter().zip(rs.iter_mut()) {
                x = if flush { [r.push(s, &x)?, r.flush(s, n)?].concat() } else { r.push(s, &x)? };
            }
            b.snake.apply(&mut x, None);
            x = if flush { [b.conv.push(cs, &x, None)?, b.conv.flush(cs, n)?].concat() } else { b.conv.push(cs, &x, None)? };
            n = b.conv.batch_len(n);
        }
        self.snake.apply(&mut x, None);
        if flush { Ok([self.conv2.push(&mut st.conv2, &x, None)?, self.conv2.flush(&mut st.conv2, n)?].concat()) } else { self.conv2.push(&mut st.conv2, &x, None) }
    }
}

struct DecoderBlock {
    snake: Snake,
    conv: ConvTr,
    res: Vec<ResUnit>,
}

struct Decoder {
    conv1: Conv,
    blocks: Vec<DecoderBlock>,
    snake: Snake,
    conv2: Conv,
}

/// Streaming state of the decoder network: create with [`DacModel::decoder_state`].
pub struct DecoderState {
    conv1: ConvState,
    blocks: Vec<(ConvTrState, Vec<ResState>)>,
    conv2: ConvState,
    frames: usize,
}

impl Decoder {
    fn state(&self) -> DecoderState {
        DecoderState {
            conv1: self.conv1.state(),
            blocks: self.blocks.iter().map(|b| (b.conv.state(), b.res.iter().map(ResUnit::state).collect())).collect(),
            conv2: self.conv2.state(),
            frames: 0,
        }
    }

    /// Latent rows in → samples now complete; with `flush` the end of the signal.
    fn push(&self, st: &mut DecoderState, latents: &[f32], flush: bool) -> CResult<Vec<f32>> {
        st.frames += latents.len() / LATENT_DIM;
        let mut x = self.conv1.push(&mut st.conv1, latents, None)?;
        if flush {
            x.extend(self.conv1.flush(&mut st.conv1, st.frames)?);
        }
        let mut n = st.frames;
        for (b, (cs, rs)) in self.blocks.iter().zip(st.blocks.iter_mut()) {
            b.snake.apply(&mut x, None);
            x = b.conv.push(cs, &x, None)?;
            if flush {
                x.extend(b.conv.flush(cs)?);
            }
            n = b.conv.batch_len(n);
            for (r, s) in b.res.iter().zip(rs.iter_mut()) {
                x = if flush { [r.push(s, &x)?, r.flush(s, n)?].concat() } else { r.push(s, &x)? };
            }
        }
        self.snake.apply(&mut x, None);
        let mut y = self.conv2.push(&mut st.conv2, &x, None)?;
        if flush {
            y.extend(self.conv2.flush(&mut st.conv2, n)?);
        }
        y.par_iter_mut().for_each(|v| *v = v.tanh());
        Ok(y)
    }
}

/// One codebook of the residual vector quantiser.
struct Codebook {
    /// in_proj transposed, (1024, 8), and its bias (8).
    in_w_t: Tensor,
    in_b: Tensor,
    /// out_proj transposed, (8, 1024), and its bias (1024).
    out_w_t: Tensor,
    out_b: Tensor,
    /// The entries (1024, 8), and L2-normalised and transposed (8, 1024).
    entries: Tensor,
    unit_t: Tensor,
}

// ---------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------

struct Loader<'a> {
    st: SliceSafetensors<'a>,
    path: &'a Path,
}

impl Loader<'_> {
    fn error(&self, message: String) -> EncodecError {
        EncodecError::Weights { path: self.path.to_path_buf(), message }
    }

    fn tensor(&self, name: &str, shape: &[usize]) -> Result<Tensor, EncodecError> {
        let t = self
            .st
            .load(name, &Device::Cpu)
            .map_err(|_| self.error(format!("tensor `{name}` is missing (is this the descript/dac_24khz model?)")))?;
        if t.dims() != shape {
            return Err(self.error(format!("tensor `{name}` has shape {:?}, expected {shape:?}", t.dims())));
        }
        Ok(t.to_dtype(DType::F32)?)
    }

    #[allow(clippy::too_many_arguments)]
    fn conv(&self, p: &str, cin: usize, cout: usize, k: usize, stride: usize, padding: usize, dilation: usize) -> Result<Conv, EncodecError> {
        // (out, in, k) → (k, in, out) → (k·in, out).
        let w = self.tensor(&format!("{p}.weight"), &[cout, cin, k])?.permute((2, 1, 0))?.reshape((k * cin, cout))?.contiguous()?;
        Ok(Conv { w, bias: self.tensor(&format!("{p}.bias"), &[cout])?.to_vec1()?, cin, k, stride, dilation, padding })
    }

    fn snake(&self, p: &str, channels: usize) -> Result<Snake, EncodecError> {
        let alpha: Vec<f32> = self.tensor(&format!("{p}.alpha"), &[1, channels, 1])?.flatten_all()?.to_vec1()?;
        let inv = alpha.iter().map(|a| 1.0 / (a + 1e-9)).collect();
        Ok(Snake { alpha, inv })
    }

    fn res_units(&self, p: &str, dim: usize) -> Result<Vec<ResUnit>, EncodecError> {
        DILATIONS
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                let p = format!("{p}.res_unit{}", i + 1);
                Ok(ResUnit {
                    snake1: self.snake(&format!("{p}.snake1"), dim)?,
                    conv1: self.conv(&format!("{p}.conv1"), dim, dim, 7, 1, 3 * d, d)?,
                    snake2: self.snake(&format!("{p}.snake2"), dim)?,
                    conv2: self.conv(&format!("{p}.conv2"), dim, dim, 1, 1, 0, 1)?,
                })
            })
            .collect()
    }

    fn encoder(&self) -> Result<Encoder, EncodecError> {
        let conv1 = self.conv("encoder.conv1", 1, ENCODER_DIM, 7, 1, 3, 1)?;
        let mut dim = ENCODER_DIM;
        let mut blocks = Vec::new();
        for (i, &s) in ENCODER_STRIDES.iter().enumerate() {
            let p = format!("encoder.block.{i}");
            blocks.push(EncoderBlock {
                res: self.res_units(&p, dim)?,
                snake: self.snake(&format!("{p}.snake1"), dim)?,
                conv: self.conv(&format!("{p}.conv1"), dim, 2 * dim, 2 * s, s, s.div_ceil(2), 1)?,
            });
            dim *= 2;
        }
        Ok(Encoder {
            conv1,
            blocks,
            snake: self.snake("encoder.snake1", dim)?,
            conv2: self.conv("encoder.conv2", dim, LATENT_DIM, 3, 1, 1, 1)?,
        })
    }

    fn decoder(&self) -> Result<Decoder, EncodecError> {
        let conv1 = self.conv("decoder.conv1", LATENT_DIM, DECODER_DIM, 7, 1, 3, 1)?;
        let mut dim = DECODER_DIM;
        let mut blocks = Vec::new();
        for (i, &s) in DECODER_STRIDES.iter().enumerate() {
            let p = format!("decoder.block.{i}");
            let out = dim / 2;
            // (in, out, 2s) → (in, 2s, out) → (in, 2s·out).
            let w = self.tensor(&format!("{p}.conv_t1.weight"), &[dim, out, 2 * s])?.permute((0, 2, 1))?.reshape((dim, 2 * s * out))?.contiguous()?;
            blocks.push(DecoderBlock {
                snake: self.snake(&format!("{p}.snake1"), dim)?,
                conv: ConvTr {
                    w,
                    bias: self.tensor(&format!("{p}.conv_t1.bias"), &[out])?.to_vec1()?,
                    cin: dim,
                    cout: out,
                    stride: s,
                    padding: s.div_ceil(2),
                },
                res: self.res_units(&p, out)?,
            });
            dim = out;
        }
        Ok(Decoder {
            conv1,
            blocks,
            snake: self.snake("decoder.snake1", dim)?,
            conv2: self.conv("decoder.conv2", dim, 1, 7, 1, 3, 1)?,
        })
    }

    fn codebook(&self, k: usize) -> Result<Codebook, EncodecError> {
        let p = format!("quantizer.quantizers.{k}");
        let entries = self.tensor(&format!("{p}.codebook.weight"), &[CODEBOOK_SIZE, CODEBOOK_DIM])?;
        let norm = entries.sqr()?.sum_keepdim(1)?.sqrt()?.maximum(1e-12)?;
        let unit_t = entries.broadcast_div(&norm)?.t()?.contiguous()?;
        let in_w = self.tensor(&format!("{p}.in_proj.weight"), &[CODEBOOK_DIM, LATENT_DIM, 1])?;
        let out_w = self.tensor(&format!("{p}.out_proj.weight"), &[LATENT_DIM, CODEBOOK_DIM, 1])?;
        Ok(Codebook {
            in_w_t: in_w.squeeze(2)?.t()?.contiguous()?,
            in_b: self.tensor(&format!("{p}.in_proj.bias"), &[CODEBOOK_DIM])?,
            out_w_t: out_w.squeeze(2)?.t()?.contiguous()?,
            out_b: self.tensor(&format!("{p}.out_proj.bias"), &[LATENT_DIM])?,
            entries,
            unit_t,
        })
    }
}

// ---------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------

/// The DAC 24 kHz model (weights only; streams keep their state in [`EncoderState`] /
/// [`DecoderState`]). Immutable and `Send + Sync`: share it with `Arc`.
pub struct DacModel {
    path: PathBuf,
    encoder: Encoder,
    decoder: Decoder,
    codebooks: Vec<Codebook>,
}

impl std::fmt::Debug for DacModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DacModel").field("path", &self.path).finish_non_exhaustive()
    }
}

impl DacModel {
    /// Load the model from a `model.safetensors` of `descript/dac_24khz` (299 MB).
    pub fn load(path: &Path) -> Result<Self, EncodecError> {
        let data = std::fs::read(path).map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: e.to_string() })?;
        Self::from_bytes(&data, path)
    }

    /// Load the model from the bytes of its weights file (`path` for messages).
    pub fn from_bytes(data: &[u8], path: &Path) -> Result<Self, EncodecError> {
        let st = SliceSafetensors::new(data)
            .map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: format!("not a safetensors file: {e}") })?;
        let l = Loader { st, path };
        let codebooks = (0..CODEBOOKS).map(|k| l.codebook(k)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { path: path.to_path_buf(), encoder: l.encoder()?, decoder: l.decoder()?, codebooks })
    }

    /// The weights file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A fresh encoder stream.
    pub fn encoder_state(&self) -> EncoderState {
        self.encoder.state()
    }

    /// A fresh decoder stream.
    pub fn decoder_state(&self) -> DecoderState {
        self.decoder.state()
    }

    /// Continue an encoder stream with 24 kHz mono PCM: the latents (frame-major,
    /// [`LATENT_DIM`] values per frame) now complete. A frame needs 2493 samples after
    /// its own; see [`ENCODER_LEAD_IN`].
    pub fn encode_latents(&self, st: &mut EncoderState, pcm: &[f32]) -> Result<Vec<f32>, EncodecError> {
        Ok(self.encoder.push(st, pcm, false)?)
    }

    /// End an encoder stream: the remaining latents.
    pub fn finish_encoding(&self, st: &mut EncoderState) -> Result<Vec<f32>, EncodecError> {
        Ok(self.encoder.push(st, &[], true)?)
    }

    /// Residual vector quantisation of latents (frame-major) into `codebooks` codes per
    /// frame: each codebook takes the entry nearest in direction to the residual's
    /// projection.
    pub fn quantize(&self, latents: &[f32], codebooks: usize) -> Result<Vec<u16>, EncodecError> {
        if !(1..=CODEBOOKS).contains(&codebooks) {
            return Err(EncodecError::InvalidInput(format!("{codebooks} codebooks (1-{CODEBOOKS} possible)")));
        }
        let frames = latents.len() / LATENT_DIM;
        let mut codes = vec![0u16; frames * codebooks];
        if frames == 0 {
            return Ok(codes);
        }
        let mut residual = Tensor::from_slice(&latents[..frames * LATENT_DIM], (frames, LATENT_DIM), &Device::Cpu)?;
        for (k, cb) in self.codebooks.iter().take(codebooks).enumerate() {
            let e = residual.matmul(&cb.in_w_t)?.broadcast_add(&cb.in_b)?;
            let norm = e.sqr()?.sum_keepdim(1)?.sqrt()?.maximum(1e-12)?;
            let idx = e.broadcast_div(&norm)?.matmul(&cb.unit_t)?.argmax(1)?;
            let q = cb.entries.index_select(&idx, 0)?;
            residual = (residual - q.matmul(&cb.out_w_t)?.broadcast_add(&cb.out_b)?)?;
            for (f, v) in idx.to_vec1::<u32>()?.into_iter().enumerate() {
                codes[f * codebooks + k] = v as u16;
            }
        }
        Ok(codes)
    }

    /// The latents (frame-major) of codes (`codebooks` per frame) using the first
    /// `depth` codebooks.
    pub fn latents(&self, codes: &[u16], codebooks: usize, depth: usize) -> Result<Vec<f32>, EncodecError> {
        if codebooks == 0 || !codes.len().is_multiple_of(codebooks) {
            return Err(EncodecError::InvalidInput(format!("{} codes of {codebooks} codebooks", codes.len())));
        }
        let frames = codes.len() / codebooks;
        let mut z = Tensor::zeros((frames, LATENT_DIM), DType::F32, &Device::Cpu)?;
        for (k, cb) in self.codebooks.iter().take(depth.min(codebooks)).enumerate() {
            let idx: Vec<u32> = (0..frames).map(|f| u32::from(codes[f * codebooks + k]) % CODEBOOK_SIZE as u32).collect();
            let q = cb.entries.index_select(&Tensor::from_vec(idx, frames, &Device::Cpu)?, 0)?;
            z = (z + q.matmul(&cb.out_w_t)?.broadcast_add(&cb.out_b)?)?;
        }
        Ok(z.flatten_all()?.to_vec1()?)
    }

    /// Continue a decoder stream with latents (frame-major): the samples now complete
    /// (320 per frame, lagging by [`DECODER_LAG`]).
    pub fn decode_latents_stream(&self, st: &mut DecoderState, latents: &[f32]) -> Result<Vec<f32>, EncodecError> {
        if !latents.len().is_multiple_of(LATENT_DIM) {
            return Err(EncodecError::InvalidInput(format!("{} latent values is not a whole number of frames", latents.len())));
        }
        Ok(self.decoder.push(st, latents, false)?)
    }

    /// End a decoder stream: the remaining samples.
    pub fn finish_decoding(&self, st: &mut DecoderState) -> Result<Vec<f32>, EncodecError> {
        Ok(self.decoder.push(st, &[], true)?)
    }

    /// Encode a whole signal (24 kHz mono, padded with zeros to whole frames) into
    /// `codebooks` codes per frame, frame-major.
    pub fn encode(&self, pcm: &[f32], codebooks: usize) -> Result<Vec<u16>, EncodecError> {
        let frames = pcm.len().div_ceil(FRAME_SAMPLES);
        let mut x = pcm.to_vec();
        x.resize(frames * FRAME_SAMPLES, 0.0);
        let mut st = self.encoder_state();
        let mut z = self.encode_latents(&mut st, &x)?;
        z.extend(self.finish_encoding(&mut st)?);
        self.quantize(&z, codebooks)
    }

    /// Decode whole-signal latents: 320 samples per frame (the last 8 are zero: the
    /// stride-5 transposed convolution yields one sample less than 5× its input).
    pub fn decode_latents(&self, latents: &[f32]) -> Result<Vec<f32>, EncodecError> {
        let frames = latents.len() / LATENT_DIM;
        let mut st = self.decoder_state();
        let mut y = self.decode_latents_stream(&mut st, latents)?;
        y.extend(self.finish_decoding(&mut st)?);
        y.resize(frames * FRAME_SAMPLES, 0.0);
        Ok(y)
    }

    /// Decode whole-signal codes (`codebooks` per frame) using the first `depth`
    /// codebooks.
    pub fn decode(&self, codes: &[u16], codebooks: usize, depth: usize) -> Result<Vec<f32>, EncodecError> {
        self.decode_latents(&self.latents(codes, codebooks, depth)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace's DAC weights, if downloaded.
    fn weights() -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/dac_24khz/model.safetensors");
        p.is_file().then_some(p)
    }

    /// A test signal: a chirp with harmonics and a noise burst.
    fn signal(n: usize) -> Vec<f32> {
        let mut state = 12345u32;
        (0..n)
            .map(|i| {
                let t = i as f32 / 24_000.0;
                let tone = (std::f32::consts::TAU * (200.0 + 300.0 * t) * t).sin() * 0.3 + (std::f32::consts::TAU * 1234.0 * t).sin() * 0.1;
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                tone + if (8000..12000).contains(&i) { 0.2 * noise } else { 0.0 }
            })
            .collect()
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn decoder_lag() {
        // The blocks' lags in output samples: transposed convolutions (padding) and
        // residual units (3 + 9 + 27 rows) at 600 Hz, 3 kHz, 12 kHz and 24 kHz.
        assert_eq!(DECODER_BLOCK_LAG, 43 * 40 + 42 * 8 + 41 * 2 + 40);
        assert_eq!(DECODER_LAG, 3141);
    }

    /// Streaming in uneven chunks gives what the whole-signal reference gives.
    #[test]
    fn streaming_matches_the_reference() {
        let Some(path) = weights() else {
            eprintln!("skipped: DAC weights not downloaded");
            return;
        };
        let model = DacModel::load(&path).unwrap();
        let reference = reference::DacModel::load(&path).unwrap();
        let x = signal(40 * FRAME_SAMPLES);
        // Encoder: latents of the whole signal, then fed in odd chunks.
        let mut st = model.encoder_state();
        let mut z = Vec::new();
        for chunk in x.chunks(3001) {
            z.extend(model.encode_latents(&mut st, chunk).unwrap());
        }
        z.extend(model.finish_encoding(&mut st).unwrap());
        let codes = reference.encode(&x, 12).unwrap();
        assert_eq!(model.quantize(&z, 12).unwrap(), codes, "codes");
        // Decoder: in odd chunks of frames, against the reference.
        let latents = reference.latents(&codes, 12, 12).unwrap();
        let mut st = model.decoder_state();
        let mut y = Vec::new();
        for chunk in latents.chunks(7 * LATENT_DIM) {
            y.extend(model.decode_latents_stream(&mut st, chunk).unwrap());
        }
        y.extend(model.finish_decoding(&mut st).unwrap());
        let want = reference.decode_latents(&latents).unwrap();
        y.resize(want.len(), 0.0);
        let d = max_diff(&y, &want);
        assert!(d < 1e-4, "max difference {d}");
        // Without the flush, a stream lags by the decoder's look-ahead.
        let mut st = model.decoder_state();
        let part = model.decode_latents_stream(&mut st, &latents).unwrap();
        assert_eq!(part.len(), 40 * FRAME_SAMPLES - DECODER_LAG);
    }

    /// With the lead-in, every 320 samples of signal complete one frame.
    #[test]
    fn encoder_lead_in() {
        let Some(path) = weights() else {
            eprintln!("skipped: DAC weights not downloaded");
            return;
        };
        let model = DacModel::load(&path).unwrap();
        let mut st = model.encoder_state();
        assert!(model.encode_latents(&mut st, &vec![0.0; ENCODER_LEAD_IN]).unwrap().is_empty());
        for _ in 0..3 {
            let z = model.encode_latents(&mut st, &signal(30 * FRAME_SAMPLES)).unwrap();
            assert_eq!(z.len(), 30 * LATENT_DIM);
        }
    }
}
