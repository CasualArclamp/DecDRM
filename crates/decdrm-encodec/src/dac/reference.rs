//! The whole-signal reference the streaming DAC is tested against: a direct port of
//! `transformers`' DAC with candle's own (channel-major) convolutions, run over long
//! signals in chunks with context.
//!
//! The network is DAC as ported to Hugging Face `transformers` (`modeling_dac.py`;
//! weights `descript/dac_24khz`, MIT licence, weight norm folded in):
//!
//! * an encoder: a convolution (1 → 64 channels), four blocks of three dilated residual
//!   units (dilations 1, 3, 9; Snake activations) and a strided convolution (×2 ×4 ×5 ×8,
//!   doubling the channels), then a convolution to a 1024-dimensional latent at 75
//!   frames per second;
//! * a residual vector quantiser of 32 codebooks × 1024 entries: each codebook looks up
//!   the residual in an 8-dimensional projection by cosine similarity (factorised,
//!   L2-normalised codes) and projects the entry back to 1024 dimensions;
//! * a decoder: a convolution (1024 → 1536 channels), four blocks of a transposed
//!   convolution (×8 ×5 ×4 ×2, halving the channels) and three residual units, then a
//!   convolution to the waveform and tanh.
//!
//! The same frame rate and code size as EnCodec 24 kHz: 75 frames per second of 1–32
//! codes of 10 bits (0.75–24 kbit/s; DAC was trained with quantiser dropout, so any
//! number of codebooks works). Unlike EnCodec its convolutions are centred, not causal:
//! an output sample depends on about ten frames before and nine after it. Long signals
//! are therefore processed in chunks with [`CONTEXT_FRAMES`] of context on each side,
//! which gives the same result as one run over the whole signal.

#![allow(dead_code)]

use crate::error::EncodecError;
use candle_core::safetensors::SliceSafetensors;
use candle_core::{DType, Device, Tensor};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

/// Sample rate of the 24 kHz model.
pub const SAMPLE_RATE: u32 = 24_000;
/// Samples per frame (the product of the strides): 75 frames per second.
pub const FRAME_SAMPLES: usize = 320;
/// Codebooks, and entries per codebook (10-bit codes).
pub const CODEBOOKS: usize = 32;
pub const CODEBOOK_SIZE: usize = 1024;
/// Dimension of the latent (the decoder's input).
pub const LATENT_DIM: usize = 1024;
/// Frames of context on each side of a chunk: more than the receptive field (about ten
/// frames back and nine ahead for the decoder, fewer for the encoder).
pub const CONTEXT_FRAMES: usize = 16;

const CODEBOOK_DIM: usize = 8;
const ENCODER_DIM: usize = 64;
const DECODER_DIM: usize = 1536;
const ENCODER_STRIDES: [usize; 4] = [2, 4, 5, 8];
const DECODER_STRIDES: [usize; 4] = [8, 5, 4, 2];
const DILATIONS: [usize; 3] = [1, 3, 9];
/// Frames per chunk (4 s).
const CHUNK_FRAMES: usize = 300;

type CResult<T> = candle_core::Result<T>;

// ---------------------------------------------------------------------------------
// Elementwise kernels
// ---------------------------------------------------------------------------------

/// `f(channel, row)` on every channel row of a (1, C, T) tensor in parallel, into a new
/// tensor. candle's elementwise operations run on one thread and allocate a tensor each;
/// the decoder's last stages have 96–192 channels at 12–24 kHz, where a Snake
/// activation made of five of them cost more than the convolutions.
fn map_rows(x: &Tensor, f: impl Fn(usize, &mut [f32]) + Sync) -> CResult<Tensor> {
    let (b, c, t) = x.dims3()?;
    let mut v: Vec<f32> = x.flatten_all()?.to_vec1()?;
    v.par_chunks_mut(t.max(1)).enumerate().for_each(|(i, row)| f(i % c, row));
    Tensor::from_vec(v, (b, c, t), x.device())
}

/// Snake of `x` plus a bias per channel: u + sin²(αu)/α with u = x + bias.
fn snake(x: &Tensor, s: &Snake, bias: Option<&[f32]>) -> CResult<Tensor> {
    map_rows(x, |c, row| {
        let (a, k, b) = (s.alpha[c], s.inv[c], bias.map_or(0.0, |b| b[c]));
        for v in row {
            let u = *v + b;
            let w = (a * u).sin();
            *v = u + k * w * w;
        }
    })
}

/// `x` plus a bias per channel.
fn add_bias(x: &Tensor, bias: &[f32]) -> CResult<Tensor> {
    map_rows(x, |c, row| row.iter_mut().for_each(|v| *v += bias[c]))
}

/// `y` plus a bias per channel plus `x` (a residual connection).
fn residual(y: &Tensor, bias: &[f32], x: &Tensor) -> CResult<Tensor> {
    let (b, c, t) = y.dims3()?;
    let mut v: Vec<f32> = y.flatten_all()?.to_vec1()?;
    let xv: Vec<f32> = x.flatten_all()?.to_vec1()?;
    v.par_chunks_mut(t.max(1)).zip(xv.par_chunks(t.max(1))).enumerate().for_each(|(i, (row, xr))| {
        let bc = bias[i % c];
        for (o, xi) in row.iter_mut().zip(xr) {
            *o += bc + xi;
        }
    });
    Tensor::from_vec(v, (b, c, t), y.device())
}

// ---------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------

/// A 1-D convolution with zero padding on both sides; its bias is added by the next
/// elementwise kernel.
struct Conv {
    /// (out, in, kernel).
    weight: Tensor,
    bias: Vec<f32>,
    stride: usize,
    padding: usize,
    dilation: usize,
}

impl Conv {
    /// Without the bias.
    fn raw(&self, x: &Tensor) -> CResult<Tensor> {
        x.conv1d(&self.weight, self.padding, self.stride, self.dilation, 1)
    }

    fn forward(&self, x: &Tensor) -> CResult<Tensor> {
        add_bias(&self.raw(x)?, &self.bias)
    }
}

/// A transposed convolution (kernel 2 × stride, `padding` output samples cut from both
/// ends).
struct ConvTr {
    /// (in, out, kernel).
    weight: Tensor,
    bias: Vec<f32>,
    stride: usize,
    padding: usize,
}

impl ConvTr {
    fn forward(&self, x: &Tensor) -> CResult<Tensor> {
        // candle's fast transposed convolution (a matrix product and an overlap-add)
        // needs zero padding; with padding it falls back to a direct loop many times
        // slower. Padding only cuts output samples from both ends, so cut them here.
        let y = x.conv_transpose1d(&self.weight, 0, 0, self.stride, 1, 1)?;
        let len = y.dim(2)?;
        add_bias(&y.narrow(2, self.padding, len - 2 * self.padding)?, &self.bias)
    }
}

/// Snake activation x + sin²(αx)/α, α per channel.
struct Snake {
    alpha: Vec<f32>,
    /// 1 / (α + 10⁻⁹).
    inv: Vec<f32>,
}

/// Snake, dilated convolution (kernel 7), Snake, 1×1 convolution, plus the input.
struct ResUnit {
    snake1: Snake,
    conv1: Conv,
    snake2: Snake,
    conv2: Conv,
}

impl ResUnit {
    fn forward(&self, x: &Tensor) -> CResult<Tensor> {
        let h = self.conv1.raw(&snake(x, &self.snake1, None)?)?;
        let h = self.conv2.raw(&snake(&h, &self.snake2, Some(&self.conv1.bias))?)?;
        residual(&h, &self.conv2.bias, x)
    }
}

/// Three residual units, Snake, the strided convolution.
struct EncoderBlock {
    res: Vec<ResUnit>,
    snake: Snake,
    conv: Conv,
}

/// Snake, the transposed convolution, three residual units.
struct DecoderBlock {
    snake: Snake,
    conv: ConvTr,
    res: Vec<ResUnit>,
}

struct Encoder {
    conv1: Conv,
    blocks: Vec<EncoderBlock>,
    snake: Snake,
    conv2: Conv,
}

impl Encoder {
    /// (1, 1, 320·T) → (1, 1024, T).
    fn forward(&self, x: &Tensor) -> CResult<Tensor> {
        let mut x = self.conv1.forward(x)?;
        for b in &self.blocks {
            for r in &b.res {
                x = r.forward(&x)?;
            }
            x = b.conv.forward(&snake(&x, &b.snake, None)?)?;
        }
        self.conv2.forward(&snake(&x, &self.snake, None)?)
    }
}

struct Decoder {
    conv1: Conv,
    blocks: Vec<DecoderBlock>,
    snake: Snake,
    conv2: Conv,
}

impl Decoder {
    /// (1, 1024, T) → (1, 1, 320·T − 8): the stride-5 transposed convolution yields one
    /// sample less than 5× its input (its padding of 3 is cut from both ends).
    fn forward(&self, x: &Tensor) -> CResult<Tensor> {
        let mut x = self.conv1.forward(x)?;
        for b in &self.blocks {
            x = b.conv.forward(&snake(&x, &b.snake, None)?)?;
            for r in &b.res {
                x = r.forward(&x)?;
            }
        }
        self.conv2.forward(&snake(&x, &self.snake, None)?)?.tanh()
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
    fn conv(&self, p: &str, cin: usize, cout: usize, kernel: usize, stride: usize, padding: usize, dilation: usize) -> Result<Conv, EncodecError> {
        Ok(Conv {
            weight: self.tensor(&format!("{p}.weight"), &[cout, cin, kernel])?,
            bias: self.tensor(&format!("{p}.bias"), &[cout])?.to_vec1()?,
            stride,
            padding,
            dilation,
        })
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
            blocks.push(DecoderBlock {
                snake: self.snake(&format!("{p}.snake1"), dim)?,
                conv: ConvTr {
                    weight: self.tensor(&format!("{p}.conv_t1.weight"), &[dim, out, 2 * s])?,
                    bias: self.tensor(&format!("{p}.conv_t1.bias"), &[out])?.to_vec1()?,
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

/// The DAC 24 kHz model (immutable; `Send + Sync`).
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

/// The chunks of `frames` frames: (start, end) and the context taken on either side.
fn chunks(frames: usize) -> impl Iterator<Item = (usize, usize, usize, usize)> {
    (0..frames).step_by(CHUNK_FRAMES).map(move |c| {
        let end = (c + CHUNK_FRAMES).min(frames);
        (c, end, CONTEXT_FRAMES.min(c), CONTEXT_FRAMES.min(frames - end))
    })
}

impl DacModel {
    /// Load the model from a `model.safetensors` of `descript/dac_24khz` (299 MB).
    pub fn load(path: &Path) -> Result<Self, EncodecError> {
        let data = std::fs::read(path).map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: e.to_string() })?;
        let st = SliceSafetensors::new(&data)
            .map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: format!("not a safetensors file: {e}") })?;
        let l = Loader { st, path };
        let codebooks = (0..CODEBOOKS).map(|k| l.codebook(k)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { path: path.to_path_buf(), encoder: l.encoder()?, decoder: l.decoder()?, codebooks })
    }

    /// Encode 24 kHz mono PCM (padded with zeros to whole frames) into `codebooks` codes
    /// per frame, frame-major.
    pub fn encode(&self, pcm: &[f32], codebooks: usize) -> Result<Vec<u16>, EncodecError> {
        if !(1..=CODEBOOKS).contains(&codebooks) {
            return Err(EncodecError::InvalidInput(format!("{codebooks} codebooks (1-{CODEBOOKS} possible)")));
        }
        let frames = pcm.len().div_ceil(FRAME_SAMPLES);
        let mut x = pcm.to_vec();
        x.resize(frames * FRAME_SAMPLES, 0.0);
        let mut codes = Vec::with_capacity(frames * codebooks);
        for (c, end, l, r) in chunks(frames) {
            let input = &x[(c - l) * FRAME_SAMPLES..(end + r) * FRAME_SAMPLES];
            let latent = self.encoder.forward(&Tensor::from_slice(input, (1, 1, input.len()), &Device::Cpu)?)?;
            let latent = latent.squeeze(0)?.t()?.narrow(0, l, end - c)?.contiguous()?;
            codes.extend(self.quantize(&latent, codebooks)?);
        }
        Ok(codes)
    }

    /// Residual vector quantisation of latents (T, 1024): each codebook takes the
    /// entry nearest in direction to the residual's projection.
    fn quantize(&self, latent: &Tensor, codebooks: usize) -> Result<Vec<u16>, EncodecError> {
        let frames = latent.dim(0)?;
        let mut residual = latent.clone();
        let mut codes = vec![0u16; frames * codebooks];
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

    /// The latents (frame-major, [`LATENT_DIM`] values per frame) of codes
    /// (`codebooks` per frame) using the first `depth` codebooks.
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

    /// Decode latents (frame-major): 320 samples per frame.
    pub fn decode_latents(&self, latents: &[f32]) -> Result<Vec<f32>, EncodecError> {
        if latents.is_empty() || !latents.len().is_multiple_of(LATENT_DIM) {
            return Err(EncodecError::InvalidInput(format!("{} latent values is not a whole number of frames", latents.len())));
        }
        let frames = latents.len() / LATENT_DIM;
        let z = Tensor::from_slice(latents, (frames, LATENT_DIM), &Device::Cpu)?;
        let mut out = Vec::with_capacity(frames * FRAME_SAMPLES);
        for (c, end, l, r) in chunks(frames) {
            let x = z.narrow(0, c - l, end + r - c + l)?.t()?.unsqueeze(0)?.contiguous()?;
            let y: Vec<f32> = self.decoder.forward(&x)?.flatten_all()?.to_vec1()?;
            // The last chunk ends 8 samples short (see `Decoder::forward`): zeros.
            let start = l * FRAME_SAMPLES;
            out.extend((start..start + (end - c) * FRAME_SAMPLES).map(|i| y.get(i).copied().unwrap_or(0.0)));
        }
        Ok(out)
    }

    /// Decode codes (`codebooks` per frame) using the first `depth` codebooks.
    pub fn decode(&self, codes: &[u16], codebooks: usize, depth: usize) -> Result<Vec<f32>, EncodecError> {
        self.decode_latents(&self.latents(codes, codebooks, depth)?)
    }
}
