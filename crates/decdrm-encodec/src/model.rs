//! Streaming EnCodec 24 kHz inference on candle (CPU).
//!
//! The network is Meta's EnCodec as ported to Hugging Face `transformers`
//! (`modeling_encodec.py`; also `candle_transformers::models::encodec`): a SEANet
//! encoder (causal convolutions, four downsampling stages ×2 ×4 ×5 ×8, a two-layer
//! LSTM), a residual vector quantiser of 32 codebooks × 1024 entries × 128 dimensions,
//! and the mirror-image decoder with transposed convolutions.
//!
//! Those implementations process a whole signal at once. A radio link needs *streaming*:
//! 400 ms in, 400 ms out, forever, without seams. EnCodec 24 kHz is causal, so this
//! implementation runs every layer on consecutive chunks and carries its state over:
//!
//! * a causal convolution keeps the last `kernel − stride` input samples (exactly its
//!   left padding within a whole-signal run; at the very start the first sample is
//!   replicated, as candle-transformers pads);
//! * a transposed convolution keeps the `kernel − stride` output samples to which the
//!   next chunk's first input still contributes (the batch model trims them only at
//!   the end of the signal);
//! * the LSTMs keep their hidden and cell states.
//!
//! Chunked output is therefore identical to a whole-signal run (up to float rounding),
//! with no look-ahead: `n` frames of codes decode to `n × 320` samples at once.
//!
//! Tensors run on candle's CPU backend (its matrix products use all cores). The LSTM
//! steps are small, so their gate arithmetic is plain Rust. Residual vector
//! quantisation (encoder) uses matrix products; the decoder's codebook lookups are
//! plain Rust too, which lets every frame use a different number of codebooks.

use crate::config::{CODEBOOK_SIZE, FRAME_SAMPLES, LATENT_DIM, MAX_CODEBOOKS};
use crate::error::EncodecError;
use crate::weights;
use candle_core::safetensors::SliceSafetensors;
use candle_core::{DType, Device, Tensor};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Channels of the first/last convolution.
const NUM_FILTERS: usize = 32;
/// Upsampling ratios of the decoder (the encoder downsamples in reverse order).
const RATIOS: [usize; 4] = [8, 5, 4, 2];
/// Kernel of the first and last convolutions.
const KERNEL: usize = 7;
/// Kernel of the first convolution of a residual block.
const RESIDUAL_KERNEL: usize = 3;
/// LSTM layers and width (the widest SEANet stage).
const LSTM_LAYERS: usize = 2;
const LSTM_DIM: usize = NUM_FILTERS << RATIOS.len();

/// candle's result type (its errors convert into [`EncodecError`] with `?`).
type CResult<T> = candle_core::Result<T>;

/// Which halves of the network to load: a receiver needs only the decoder, a
/// transmitter only the encoder (the codebooks come with either).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelParts {
    Encoder,
    Decoder,
    Both,
}

impl ModelParts {
    fn encoder(self) -> bool {
        matches!(self, Self::Encoder | Self::Both)
    }

    fn decoder(self) -> bool {
        matches!(self, Self::Decoder | Self::Both)
    }
}

// ---------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------

/// A causal 1-D convolution with its weight norm folded in (HF `EncodecConv1d`).
struct Conv {
    /// (out, in, kernel), contiguous.
    weight: Tensor,
    /// (1, out, 1), for broadcasting over time.
    bias: Tensor,
    stride: usize,
    /// Input samples carried from chunk to chunk: kernel − stride.
    history: usize,
}

#[derive(Default)]
struct ConvState {
    history: Option<Tensor>,
}

impl Conv {
    /// (1, in, T) → (1, out, T / stride); T must be a multiple of the stride.
    fn forward(&self, st: &mut ConvState, x: &Tensor) -> CResult<Tensor> {
        let input = if self.history == 0 {
            // `clone` of a tensor only copies a reference to the same storage.
            x.clone()
        } else {
            let past = match st.history.take() {
                Some(h) => h,
                None => {
                    let (b, c, _) = x.dims3()?;
                    x.narrow(2, 0, 1)?.broadcast_as((b, c, self.history))?.contiguous()?
                }
            };
            let full = Tensor::cat(&[&past, x], 2)?;
            let len = full.dim(2)?;
            // `contiguous` copies the slice, so the whole chunk is not kept alive.
            st.history = Some(full.narrow(2, len - self.history, self.history)?.contiguous()?);
            full
        };
        input.conv1d(&self.weight, 0, self.stride, 1, 1)?.broadcast_add(&self.bias)
    }
}

/// A causal transposed convolution with kernel 2 × stride (HF
/// `EncodecConvTranspose1d`).
struct ConvTr {
    /// (in, out, kernel), contiguous.
    weight: Tensor,
    bias: Tensor,
    stride: usize,
    kernel: usize,
}

#[derive(Default)]
struct ConvTrState {
    /// Partial output samples (without bias) the next chunk completes.
    carry: Option<Tensor>,
}

impl ConvTr {
    /// (1, in, T) → (1, out, T × stride).
    fn forward(&self, st: &mut ConvTrState, x: &Tensor) -> CResult<Tensor> {
        let t = x.dim(2)?;
        let y = x.conv_transpose1d(&self.weight, 0, 0, self.stride, 1, 1)?;
        let len = y.dim(2)?; // (T − 1)·stride + kernel
        let overlap = self.kernel - self.stride;
        let y = match st.carry.take() {
            Some(c) => {
                let head = (y.narrow(2, 0, overlap)? + c)?;
                Tensor::cat(&[&head, &y.narrow(2, overlap, len - overlap)?], 2)?
            }
            None => y,
        };
        let out_len = t * self.stride;
        st.carry = Some(y.narrow(2, out_len, len - out_len)?.contiguous()?);
        y.narrow(2, 0, out_len)?.broadcast_add(&self.bias)
    }
}

/// SEANet residual block: ELU, conv k=3 (to half the channels), ELU, conv k=1, plus a
/// 1×1 convolution shortcut.
struct ResBlock {
    conv1: Conv,
    conv2: Conv,
    shortcut: Conv,
}

#[derive(Default)]
struct ResState {
    conv1: ConvState,
    conv2: ConvState,
    shortcut: ConvState,
}

impl ResBlock {
    fn forward(&self, st: &mut ResState, x: &Tensor) -> CResult<Tensor> {
        let y = self.conv1.forward(&mut st.conv1, &x.elu(1.0)?)?;
        let y = self.conv2.forward(&mut st.conv2, &y.elu(1.0)?)?;
        // candle implements `+` for tensors, returning a `Result`.
        self.shortcut.forward(&mut st.shortcut, x)? + y
    }
}

/// One LSTM layer (PyTorch gate order i, f, g, o), weights transposed for `x · W`.
struct LstmLayer {
    /// (input, 4·hidden).
    w_ih_t: Tensor,
    /// (hidden, 4·hidden).
    w_hh_t: Tensor,
    /// b_ih + b_hh, (4·hidden).
    bias: Tensor,
}

/// Two stacked LSTM layers with a skip connection (HF `EncodecLSTM`).
struct Lstm {
    layers: Vec<LstmLayer>,
}

struct LstmState {
    h: Vec<Vec<f32>>,
    c: Vec<Vec<f32>>,
}

impl LstmState {
    fn new() -> Self {
        Self { h: vec![vec![0.0; LSTM_DIM]; LSTM_LAYERS], c: vec![vec![0.0; LSTM_DIM]; LSTM_LAYERS] }
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl Lstm {
    /// (1, C, T) → (1, C, T).
    fn forward(&self, st: &mut LstmState, x: &Tensor) -> CResult<Tensor> {
        let dev = x.device();
        let seq = x.squeeze(0)?.t()?.contiguous()?; // (T, C)
        let t = seq.dim(0)?;
        let mut input = seq.clone();
        let mut gates = vec![0f32; 4 * LSTM_DIM];
        for (layer, (h, c)) in self.layers.iter().zip(st.h.iter_mut().zip(st.c.iter_mut())) {
            // Input projections of all steps in one product; only the recurrence is
            // sequential.
            let proj: Vec<f32> = input.matmul(&layer.w_ih_t)?.broadcast_add(&layer.bias)?.flatten_all()?.to_vec1()?;
            let mut out = Vec::with_capacity(t * LSTM_DIM);
            for p in proj.as_chunks::<{ 4 * LSTM_DIM }>().0 {
                let rec: Vec<f32> =
                    Tensor::from_slice(h.as_slice(), (1, LSTM_DIM), dev)?.matmul(&layer.w_hh_t)?.flatten_all()?.to_vec1()?;
                for ((g, a), b) in gates.iter_mut().zip(p).zip(&rec) {
                    *g = a + b;
                }
                let (gi, rest) = gates.split_at(LSTM_DIM);
                let (gf, rest) = rest.split_at(LSTM_DIM);
                let (gg, go) = rest.split_at(LSTM_DIM);
                for (j, (cj, hj)) in c.iter_mut().zip(h.iter_mut()).enumerate() {
                    *cj = sigmoid(gf[j]) * *cj + sigmoid(gi[j]) * gg[j].tanh();
                    *hj = sigmoid(go[j]) * cj.tanh();
                }
                out.extend_from_slice(h);
            }
            input = Tensor::from_vec(out, (t, LSTM_DIM), dev)?;
        }
        (input + seq)?.t()?.unsqueeze(0)?.contiguous()
    }
}

// ---------------------------------------------------------------------------------
// Encoder and decoder
// ---------------------------------------------------------------------------------

struct SeanetEncoder {
    init: Conv,
    /// Residual block, then (after an ELU) the downsampling convolution.
    stages: Vec<(ResBlock, Conv)>,
    lstm: Lstm,
    last: Conv,
}

/// Streaming state of the encoder network: create with
/// [`EncodecModel::encoder_state`], pass to every [`EncodecModel::encode`] call of one
/// stream.
pub struct EncoderState {
    init: ConvState,
    stages: Vec<(ResState, ConvState)>,
    lstm: LstmState,
    last: ConvState,
}

impl SeanetEncoder {
    /// (1, 1, 320·T) → (1, 128, T).
    fn forward(&self, st: &mut EncoderState, x: &Tensor) -> CResult<Tensor> {
        let mut x = self.init.forward(&mut st.init, x)?;
        for ((res, down), (rs, ds)) in self.stages.iter().zip(st.stages.iter_mut()) {
            x = res.forward(rs, &x)?;
            x = down.forward(ds, &x.elu(1.0)?)?;
        }
        let x = self.lstm.forward(&mut st.lstm, &x)?;
        self.last.forward(&mut st.last, &x.elu(1.0)?)
    }
}

struct SeanetDecoder {
    init: Conv,
    lstm: Lstm,
    /// (After an ELU) the upsampling convolution, then a residual block.
    stages: Vec<(ConvTr, ResBlock)>,
    last: Conv,
}

/// Streaming state of the decoder network (see [`EncoderState`]).
pub struct DecoderState {
    init: ConvState,
    lstm: LstmState,
    stages: Vec<(ConvTrState, ResState)>,
    last: ConvState,
}

impl SeanetDecoder {
    /// (1, 128, T) → (1, 1, 320·T).
    fn forward(&self, st: &mut DecoderState, x: &Tensor) -> CResult<Tensor> {
        let x = self.init.forward(&mut st.init, x)?;
        let mut x = self.lstm.forward(&mut st.lstm, &x)?;
        for ((up, res), (us, rs)) in self.stages.iter().zip(st.stages.iter_mut()) {
            x = up.forward(us, &x.elu(1.0)?)?;
            x = res.forward(rs, &x)?;
        }
        self.last.forward(&mut st.last, &x.elu(1.0)?)
    }
}

/// Codebooks for encoding: per codebook the entries (1024, 128), their transpose and
/// half their squared norms (nearest entry = argmin of ½‖e‖² − x·e).
struct QuantizerEnc {
    embed: Vec<Tensor>,
    embed_t: Vec<Tensor>,
    half_norm: Vec<Tensor>,
}

// ---------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------

/// Reads named tensors from the weights file with shape checks.
struct Loader<'a> {
    st: SliceSafetensors<'a>,
    path: &'a Path,
}

/// PyTorch weight normalisation, w = g · v / ‖v‖ with the norm over all dimensions but
/// the first.
fn weight_norm(g: &Tensor, v: &Tensor) -> CResult<Tensor> {
    let norm = v.sqr()?.sum_keepdim((1, 2))?.sqrt()?;
    v.broadcast_mul(g)?.broadcast_div(&norm)?.contiguous()
}

impl Loader<'_> {
    fn error(&self, message: String) -> EncodecError {
        EncodecError::Weights { path: self.path.to_path_buf(), message }
    }

    fn tensor(&self, name: &str, shape: &[usize]) -> Result<Tensor, EncodecError> {
        let t = self
            .st
            .load(name, &Device::Cpu)
            .map_err(|_| self.error(format!("tensor `{name}` is missing (is this the facebook/encodec_24khz model?)")))?;
        if t.dims() != shape {
            return Err(self.error(format!("tensor `{name}` has shape {:?}, expected {shape:?}", t.dims())));
        }
        Ok(t.to_dtype(DType::F32)?)
    }

    fn conv(&self, prefix: &str, cin: usize, cout: usize, kernel: usize, stride: usize) -> Result<Conv, EncodecError> {
        let g = self.tensor(&format!("{prefix}.conv.weight_g"), &[cout, 1, 1])?;
        let v = self.tensor(&format!("{prefix}.conv.weight_v"), &[cout, cin, kernel])?;
        let bias = self.tensor(&format!("{prefix}.conv.bias"), &[cout])?.reshape((1, cout, 1))?;
        Ok(Conv { weight: weight_norm(&g, &v)?, bias, stride, history: kernel - stride })
    }

    fn conv_tr(&self, prefix: &str, cin: usize, cout: usize, kernel: usize, stride: usize) -> Result<ConvTr, EncodecError> {
        let g = self.tensor(&format!("{prefix}.conv.weight_g"), &[cin, 1, 1])?;
        let v = self.tensor(&format!("{prefix}.conv.weight_v"), &[cin, cout, kernel])?;
        let bias = self.tensor(&format!("{prefix}.conv.bias"), &[cout])?.reshape((1, cout, 1))?;
        Ok(ConvTr { weight: weight_norm(&g, &v)?, bias, stride, kernel })
    }

    fn resblock(&self, prefix: &str, dim: usize) -> Result<ResBlock, EncodecError> {
        Ok(ResBlock {
            conv1: self.conv(&format!("{prefix}.block.1"), dim, dim / 2, RESIDUAL_KERNEL, 1)?,
            conv2: self.conv(&format!("{prefix}.block.3"), dim / 2, dim, 1, 1)?,
            shortcut: self.conv(&format!("{prefix}.shortcut"), dim, dim, 1, 1)?,
        })
    }

    fn lstm(&self, prefix: &str) -> Result<Lstm, EncodecError> {
        let four = 4 * LSTM_DIM;
        let layers = (0..LSTM_LAYERS)
            .map(|l| {
                let w_ih = self.tensor(&format!("{prefix}.lstm.weight_ih_l{l}"), &[four, LSTM_DIM])?;
                let w_hh = self.tensor(&format!("{prefix}.lstm.weight_hh_l{l}"), &[four, LSTM_DIM])?;
                let b_ih = self.tensor(&format!("{prefix}.lstm.bias_ih_l{l}"), &[four])?;
                let b_hh = self.tensor(&format!("{prefix}.lstm.bias_hh_l{l}"), &[four])?;
                Ok(LstmLayer { w_ih_t: w_ih.t()?.contiguous()?, w_hh_t: w_hh.t()?.contiguous()?, bias: (b_ih + b_hh)? })
            })
            .collect::<Result<Vec<_>, EncodecError>>()?;
        Ok(Lstm { layers })
    }

    /// `encoder.layers.*`: conv 0; per stage a residual block, an ELU and the
    /// downsampling conv (1-3, 4-6, 7-9, 10-12); LSTM 13; ELU 14; conv 15.
    fn encoder(&self) -> Result<SeanetEncoder, EncodecError> {
        let p = "encoder.layers";
        let init = self.conv(&format!("{p}.0"), 1, NUM_FILTERS, KERNEL, 1)?;
        let (mut dim, mut idx, mut stages) = (NUM_FILTERS, 1, Vec::new());
        for &ratio in RATIOS.iter().rev() {
            let res = self.resblock(&format!("{p}.{idx}"), dim)?;
            let down = self.conv(&format!("{p}.{}", idx + 2), dim, 2 * dim, 2 * ratio, ratio)?;
            stages.push((res, down));
            dim *= 2;
            idx += 3;
        }
        let lstm = self.lstm(&format!("{p}.{idx}"))?;
        let last = self.conv(&format!("{p}.{}", idx + 2), dim, LATENT_DIM, KERNEL, 1)?;
        Ok(SeanetEncoder { init, stages, lstm, last })
    }

    /// `decoder.layers.*`: conv 0; LSTM 1; per stage an ELU, the upsampling transposed
    /// conv and a residual block (2-4, 5-7, 8-10, 11-13); ELU 14; conv 15.
    fn decoder(&self) -> Result<SeanetDecoder, EncodecError> {
        let p = "decoder.layers";
        let mut dim = LSTM_DIM;
        let init = self.conv(&format!("{p}.0"), LATENT_DIM, dim, KERNEL, 1)?;
        let lstm = self.lstm(&format!("{p}.1"))?;
        let (mut idx, mut stages) = (3, Vec::new());
        for &ratio in &RATIOS {
            let up = self.conv_tr(&format!("{p}.{idx}"), dim, dim / 2, 2 * ratio, ratio)?;
            let res = self.resblock(&format!("{p}.{}", idx + 1), dim / 2)?;
            stages.push((up, res));
            dim /= 2;
            idx += 3;
        }
        let last = self.conv(&format!("{p}.{idx}"), dim, 1, KERNEL, 1)?;
        Ok(SeanetDecoder { init, lstm, stages, last })
    }

    fn codebook(&self, k: usize) -> Result<Tensor, EncodecError> {
        self.tensor(&format!("quantizer.layers.{k}.codebook.embed"), &[CODEBOOK_SIZE, LATENT_DIM])
    }

    fn quantizer_enc(&self) -> Result<QuantizerEnc, EncodecError> {
        let (mut embed, mut embed_t, mut half_norm) = (Vec::new(), Vec::new(), Vec::new());
        for k in 0..MAX_CODEBOOKS {
            let e = self.codebook(k)?;
            half_norm.push((e.sqr()?.sum(1)? * 0.5)?);
            embed_t.push(e.t()?.contiguous()?);
            embed.push(e);
        }
        Ok(QuantizerEnc { embed, embed_t, half_norm })
    }

    /// All codebooks as one table, `[codebook][entry][dimension]`.
    fn codebook_table(&self) -> Result<Vec<f32>, EncodecError> {
        let mut table = Vec::with_capacity(MAX_CODEBOOKS * CODEBOOK_SIZE * LATENT_DIM);
        for k in 0..MAX_CODEBOOKS {
            table.extend(self.codebook(k)?.flatten_all()?.to_vec1::<f32>()?);
        }
        Ok(table)
    }
}

// ---------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------

/// The EnCodec 24 kHz model (weights only; streams keep their state in
/// [`EncoderState`] / [`DecoderState`]). Share it between encoders and decoders with
/// `Arc` — it is immutable, and `Send + Sync`.
pub struct EncodecModel {
    path: PathBuf,
    parts: ModelParts,
    encoder: Option<(SeanetEncoder, QuantizerEnc)>,
    decoder: Option<(SeanetDecoder, Vec<f32>)>,
}

impl std::fmt::Debug for EncodecModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodecModel").field("path", &self.path).field("parts", &self.parts).finish_non_exhaustive()
    }
}

/// Models loaded by [`EncodecModel::load_cached`], kept for the life of the process.
/// (`OnceLock` initialises the static on first use; the `Mutex` serialises access.)
type ModelCache = Mutex<HashMap<(PathBuf, ModelParts), Arc<EncodecModel>>>;
static CACHE: OnceLock<ModelCache> = OnceLock::new();

impl EncodecModel {
    /// Load `parts` of the model from a `model.safetensors` file of
    /// `facebook/encodec_24khz` (under 0.1 s; the decoder half and the codebooks take
    /// about 50 MB of memory, the whole model about 90 MB).
    pub fn load(path: &Path, parts: ModelParts) -> Result<Self, EncodecError> {
        let data = std::fs::read(path).map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: e.to_string() })?;
        let st = SliceSafetensors::new(&data)
            .map_err(|e| EncodecError::Weights { path: path.to_path_buf(), message: format!("not a safetensors file: {e}") })?;
        let l = Loader { st, path };
        let encoder = if parts.encoder() { Some((l.encoder()?, l.quantizer_enc()?)) } else { None };
        let decoder = if parts.decoder() { Some((l.decoder()?, l.codebook_table()?)) } else { None };
        Ok(Self { path: path.to_path_buf(), parts, encoder, decoder })
    }

    /// [`Self::load`], or the model already loaded from `path` (with at least `parts`).
    pub fn load_cached(path: &Path, parts: ModelParts) -> Result<Arc<Self>, EncodecError> {
        let cache = CACHE.get_or_init(Default::default);
        // A panic while the lock was held cannot leave the map inconsistent: go on.
        let mut map = cache.lock().unwrap_or_else(PoisonError::into_inner);
        for p in [parts, ModelParts::Both] {
            if let Some(m) = map.get(&(path.to_path_buf(), p)) {
                return Ok(Arc::clone(m));
            }
        }
        let model = Arc::new(Self::load(path, parts)?);
        map.insert((path.to_path_buf(), parts), Arc::clone(&model));
        Ok(model)
    }

    /// [`Self::load_cached`] from the default location ([`weights::find_weights`]).
    pub fn load_default(parts: ModelParts) -> Result<Arc<Self>, EncodecError> {
        Self::load_cached(&weights::find_weights()?, parts)
    }

    /// The weights file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parts that were loaded.
    pub fn parts(&self) -> ModelParts {
        self.parts
    }

    /// A fresh encoder stream state.
    pub fn encoder_state(&self) -> Result<EncoderState, EncodecError> {
        let (enc, _) = self.encoder.as_ref().ok_or(EncodecError::MissingPart("encoder"))?;
        Ok(EncoderState {
            init: ConvState::default(),
            stages: enc.stages.iter().map(|_| Default::default()).collect(),
            lstm: LstmState::new(),
            last: ConvState::default(),
        })
    }

    /// A fresh decoder stream state.
    pub fn decoder_state(&self) -> Result<DecoderState, EncodecError> {
        let (dec, _) = self.decoder.as_ref().ok_or(EncodecError::MissingPart("decoder"))?;
        Ok(DecoderState {
            init: ConvState::default(),
            lstm: LstmState::new(),
            stages: dec.stages.iter().map(|_| Default::default()).collect(),
            last: ConvState::default(),
        })
    }

    /// Encode 24 kHz mono PCM — a whole number of 320-sample frames, continuing the
    /// stream of `st` — into `codebooks` codes per frame (frame-major).
    pub fn encode(&self, st: &mut EncoderState, pcm: &[f32], codebooks: usize) -> Result<Vec<u16>, EncodecError> {
        let (enc, q) = self.encoder.as_ref().ok_or(EncodecError::MissingPart("encoder"))?;
        if pcm.is_empty() || !pcm.len().is_multiple_of(FRAME_SAMPLES) {
            return Err(EncodecError::InvalidInput(format!("{} samples is not a whole number of 320-sample frames", pcm.len())));
        }
        if !(1..=MAX_CODEBOOKS).contains(&codebooks) {
            return Err(EncodecError::InvalidInput(format!("{codebooks} codebooks (1-{MAX_CODEBOOKS} possible)")));
        }
        let x = Tensor::from_slice(pcm, (1, 1, pcm.len()), &Device::Cpu)?;
        let latent = enc.forward(st, &x)?;
        let frames = latent.dim(2)?;
        // Residual vector quantisation: each codebook quantises what the previous ones
        // left over.
        let mut residual = latent.squeeze(0)?.t()?.contiguous()?; // (T, 128)
        let mut codes = vec![0u16; frames * codebooks];
        for k in 0..codebooks {
            let dots = residual.matmul(&q.embed_t[k])?; // (T, 1024)
            let idx = q.half_norm[k].broadcast_sub(&dots)?.argmin(1)?;
            residual = (residual - q.embed[k].index_select(&idx, 0)?)?;
            for (f, v) in idx.to_vec1::<u32>()?.into_iter().enumerate() {
                codes[f * codebooks + k] = v as u16;
            }
        }
        Ok(codes)
    }

    /// The latent of one frame from its codes: the sum of the entries of the first
    /// `depth` codebooks (`out` gets [`LATENT_DIM`] values).
    pub fn latent(&self, codes: &[u16], depth: usize, out: &mut [f32]) -> Result<(), EncodecError> {
        let (_, table) = self.decoder.as_ref().ok_or(EncodecError::MissingPart("decoder"))?;
        out.fill(0.0);
        for (k, &c) in codes.iter().enumerate().take(depth.min(MAX_CODEBOOKS)) {
            let start = (k * CODEBOOK_SIZE + usize::from(c) % CODEBOOK_SIZE) * LATENT_DIM;
            for (o, v) in out.iter_mut().zip(&table[start..start + LATENT_DIM]) {
                *o += v;
            }
        }
        Ok(())
    }

    /// Decode latents (frame-major, [`LATENT_DIM`] values per frame), continuing the
    /// stream of `st`: 320 samples per frame.
    pub fn decode_latents(&self, st: &mut DecoderState, latents: &[f32]) -> Result<Vec<f32>, EncodecError> {
        let (dec, _) = self.decoder.as_ref().ok_or(EncodecError::MissingPart("decoder"))?;
        if latents.is_empty() || !latents.len().is_multiple_of(LATENT_DIM) {
            return Err(EncodecError::InvalidInput(format!("{} latent values is not a whole number of frames", latents.len())));
        }
        let frames = latents.len() / LATENT_DIM;
        let x = Tensor::from_slice(latents, (frames, LATENT_DIM), &Device::Cpu)?.t()?.unsqueeze(0)?.contiguous()?;
        Ok(dec.forward(st, &x)?.flatten_all()?.to_vec1()?)
    }

    /// Decode codes (frame-major, `codebooks` per frame) using the first `depth`
    /// codebooks of every frame.
    pub fn decode_codes(&self, st: &mut DecoderState, codes: &[u16], codebooks: usize, depth: usize) -> Result<Vec<f32>, EncodecError> {
        if codebooks == 0 || !codes.len().is_multiple_of(codebooks) {
            return Err(EncodecError::InvalidInput(format!("{} codes of {codebooks} codebooks", codes.len())));
        }
        let mut latents = vec![0f32; codes.len() / codebooks * LATENT_DIM];
        for (frame, out) in codes.chunks_exact(codebooks).zip(latents.as_chunks_mut::<LATENT_DIM>().0) {
            self.latent(frame, depth, out)?;
        }
        self.decode_latents(st, &latents)
    }
}
