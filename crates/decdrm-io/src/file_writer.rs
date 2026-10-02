//! WAV (hound) and FLAC (flacenc) writers.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::{check_whole_frames, AudioFormat, Error, Result};

/// RIFF sizes are 32-bit: keep the data chunk safely below 4 GiB (room for the header).
const WAV_MAX_DATA_BYTES: u64 = u32::MAX as u64 - 1024;

/// FLAC block size (frames per FLAC frame). 4096 is the reference encoder's default.
const FLAC_BLOCK_SIZE: usize = 4096;

/// Output container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Container {
    /// RIFF/WAVE. Limited to 4 GiB of sample data.
    Wav,
    /// Free Lossless Audio Codec. Integer samples only; typically ~50–70 % of WAV size for
    /// noisy radio signals.
    Flac,
}

impl Container {
    /// Guesses the container from the file extension (`.wav`, `.flac`; case-insensitive).
    pub fn from_path(path: impl AsRef<Path>) -> Option<Container> {
        let ext = path.as_ref().extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "wav" | "wave" => Some(Container::Wav),
            "flac" => Some(Container::Flac),
            _ => None,
        }
    }
}

/// How samples are stored in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// 8-bit integer (unsigned in WAV, signed in FLAC — handled transparently).
    Int8,
    /// 16-bit signed integer. The usual choice for recordings.
    Int16,
    /// 24-bit signed integer.
    Int24,
    /// 32-bit signed integer (WAV only).
    Int32,
    /// 32-bit IEEE float (WAV only). Lossless for our `f32` pipeline, no clipping.
    Float32,
}

impl Encoding {
    /// Bits per stored sample.
    pub fn bits(self) -> u16 {
        match self {
            Encoding::Int8 => 8,
            Encoding::Int16 => 16,
            Encoding::Int24 => 24,
            Encoding::Int32 | Encoding::Float32 => 32,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Encoding::Float32)
    }
}

/// Writes interleaved `f32` samples to a WAV or FLAC file.
///
/// Integer encodings scale by full scale (`2^(bits-1)`), round to nearest and clip to the
/// representable range, so data read with [`FileReader`](crate::FileReader) and written back
/// at the same bit depth is reproduced bit-exactly. No dither is added.
///
/// Call [`finalize`](Self::finalize) when done to write the final header (WAV sizes, FLAC
/// sample count and MD5) and to observe any error. If the writer is simply dropped, it
/// finalizes itself on a best-effort basis — this is Rust's RAII idiom: `Drop::drop` runs
/// automatically when the value goes out of scope, even on early return or panic unwinding,
/// but it cannot report errors.
///
/// ```no_run
/// use decdrm_io::{AudioFormat, Container, Encoding, FileWriter};
/// # fn main() -> decdrm_io::Result<()> {
/// let mut w = FileWriter::create("out.flac", AudioFormat::new(48_000, 1),
///                                Container::Flac, Encoding::Int16)?;
/// w.write(&[0.0, 0.25, 0.5, -0.5])?;
/// w.finalize()?;
/// # Ok(()) }
/// ```
pub struct FileWriter {
    path: PathBuf,
    format: AudioFormat,
    container: Container,
    encoding: Encoding,
    /// `None` once finalized. Wrapping the inner writer in an `Option` lets `finalize(self)`
    /// move it out (`take()`) while `Drop` still sees a valid (empty) struct.
    inner: Option<Inner>,
    frames_written: u64,
}

enum Inner {
    Wav {
        writer: hound::WavWriter<BufWriter<File>>,
        data_bytes: u64,
    },
    // Boxed because the FLAC state is much larger than the WAV variant.
    Flac(Box<FlacStreamWriter>),
}

impl FileWriter {
    /// Creates (or truncates) `path` and writes the file header.
    ///
    /// Errors if the encoding is not available for the container (FLAC supports only
    /// `Int8`/`Int16`/`Int24`) or if the format is invalid.
    pub fn create(
        path: impl AsRef<Path>,
        format: AudioFormat,
        container: Container,
        encoding: Encoding,
    ) -> Result<Self> {
        format.validate()?;
        let path = path.as_ref().to_path_buf();
        let inner = match container {
            Container::Wav => {
                let channels = u16::try_from(format.channels)
                    .map_err(|_| Error::invalid("too many channels for WAV"))?;
                let spec = hound::WavSpec {
                    channels,
                    sample_rate: format.sample_rate,
                    bits_per_sample: encoding.bits(),
                    sample_format: if encoding.is_float() {
                        hound::SampleFormat::Float
                    } else {
                        hound::SampleFormat::Int
                    },
                };
                let file = File::create(&path).map_err(|e| Error::io(&path, e))?;
                let writer = hound::WavWriter::new(BufWriter::new(file), spec)?;
                Inner::Wav { writer, data_bytes: 0 }
            }
            Container::Flac => {
                if !matches!(encoding, Encoding::Int8 | Encoding::Int16 | Encoding::Int24) {
                    return Err(Error::invalid(format!(
                        "FLAC supports 8/16/24-bit integer samples, not {encoding:?}"
                    )));
                }
                if format.channels > 8 {
                    return Err(Error::invalid("FLAC supports at most 8 channels"));
                }
                if format.sample_rate > 655_350 {
                    return Err(Error::invalid("FLAC sample rate must be <= 655350 Hz"));
                }
                let file = File::create(&path).map_err(|e| Error::io(&path, e))?;
                let writer =
                    FlacStreamWriter::new(BufWriter::new(file), format, usize::from(encoding.bits()))
                        .map_err(|e| e.with_path(&path))?;
                Inner::Flac(Box::new(writer))
            }
        };
        Ok(FileWriter { path, format, container, encoding, inner: Some(inner), frames_written: 0 })
    }

    /// Appends interleaved samples. `samples.len()` must be a whole number of frames.
    ///
    /// Values outside `[-1, 1)` are clipped for integer encodings; NaN is written as zero.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        check_whole_frames(samples, self.format.channels)?;
        let encoding = self.encoding;
        let inner = self.inner.as_mut().ok_or(Error::Finalized)?;
        match inner {
            Inner::Wav { writer, data_bytes } => {
                let bytes = samples.len() as u64 * u64::from(encoding.bits() / 8);
                if *data_bytes + bytes > WAV_MAX_DATA_BYTES {
                    return Err(Error::FileTooLarge(format!(
                        "{}: WAV files are limited to 4 GiB; use FLAC for long recordings",
                        self.path.display()
                    )));
                }
                match encoding {
                    Encoding::Float32 => {
                        for &s in samples {
                            writer.write_sample(s)?;
                        }
                    }
                    Encoding::Int8 => {
                        for &s in samples {
                            writer.write_sample(to_int(s, 8) as i8)?;
                        }
                    }
                    Encoding::Int16 => {
                        for &s in samples {
                            writer.write_sample(to_int(s, 16) as i16)?;
                        }
                    }
                    Encoding::Int24 | Encoding::Int32 => {
                        let bits = u32::from(encoding.bits());
                        for &s in samples {
                            writer.write_sample(to_int(s, bits))?;
                        }
                    }
                }
                *data_bytes += bytes;
            }
            Inner::Flac(flac) => flac.write(samples).map_err(|e| e.with_path(&self.path))?,
        }
        self.frames_written += (samples.len() / self.format.channels) as u64;
        Ok(())
    }

    /// Flushes buffered data, completes the header and closes the file.
    pub fn finalize(mut self) -> Result<()> {
        self.finish()
    }

    /// A checkpoint for long recordings: writes everything buffered to the file and,
    /// for WAV, the sizes in the header, so the file reads correctly up to here even if
    /// the program then dies. FLAC keeps its last partial block in memory, and its
    /// header's sample count and MD5 stay unset until [`finalize`](Self::finalize).
    pub fn flush(&mut self) -> Result<()> {
        let path = &self.path;
        match self.inner.as_mut().ok_or(Error::Finalized)? {
            Inner::Wav { writer, .. } => writer.flush()?,
            Inner::Flac(flac) => flac.file.flush().map_err(|e| Error::io(path, e))?,
        }
        Ok(())
    }

    /// Frames written so far.
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// The stream format being written.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Container of the file being written.
    pub fn container(&self) -> Container {
        self.container
    }

    /// Sample encoding of the file being written.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Path of the file being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn finish(&mut self) -> Result<()> {
        match self.inner.take() {
            None => Ok(()),
            Some(Inner::Wav { writer, .. }) => Ok(writer.finalize()?),
            Some(Inner::Flac(mut flac)) => flac.finish().map_err(|e| e.with_path(&self.path)),
        }
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        // Best effort: errors cannot be reported from a destructor.
        let _ = self.finish();
    }
}

impl std::fmt::Debug for FileWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWriter")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("container", &self.container)
            .field("encoding", &self.encoding)
            .field("frames_written", &self.frames_written)
            .field("finalized", &self.inner.is_none())
            .finish()
    }
}

/// Scales a normalised float to a `bits`-bit signed integer with rounding and clipping.
fn to_int(x: f32, bits: u32) -> i32 {
    let full = f64::from(1u32 << (bits - 1));
    // Computed in f64 so that 32-bit output keeps full precision. `as i32` saturates and maps
    // NaN to 0, so no input can panic here.
    (f64::from(x) * full).round().clamp(-full, full - 1.0) as i32
}

/// Private error helper so FLAC internals need not know the path.
enum FlacErr {
    Io(std::io::Error),
    Encode(String),
}

impl FlacErr {
    fn with_path(self, path: &Path) -> Error {
        match self {
            FlacErr::Io(e) => Error::io(path, e),
            FlacErr::Encode(msg) => Error::Flac(format!("{}: {msg}", path.display())),
        }
    }
}

impl From<std::io::Error> for FlacErr {
    fn from(e: std::io::Error) -> Self {
        FlacErr::Io(e)
    }
}

fn enc_err(e: impl std::fmt::Display) -> FlacErr {
    FlacErr::Encode(e.to_string())
}

/// Streaming FLAC encoder: flacenc's high-level API builds the whole stream in memory, so we
/// drive its per-frame encoder ourselves. The STREAMINFO block (which holds the total sample
/// count and MD5 that are only known at the end) is written as a placeholder first and
/// rewritten in place by [`finish`](Self::finish).
struct FlacStreamWriter {
    file: BufWriter<File>,
    config: Verified<flacenc::config::Encoder>,
    stream_info: StreamInfo,
    framebuf: FrameBuf,
    context: Context,
    /// Integer samples waiting for a full block (interleaved, `< block * channels`).
    pending: Vec<i32>,
    channels: usize,
    bits: usize,
    header_len: usize,
    frames_encoded: usize,
    sink: ByteSink,
}

impl FlacStreamWriter {
    fn new(mut file: BufWriter<File>, format: AudioFormat, bits: usize) -> Result<Self, FlacErr> {
        let mut config = flacenc::config::Encoder::default();
        config.block_size = FLAC_BLOCK_SIZE;
        config.multithread = false;
        let config = config.into_verified().map_err(|(_, e)| enc_err(e))?;
        let stream_info =
            StreamInfo::new(format.sample_rate as usize, format.channels, bits).map_err(enc_err)?;
        let framebuf = FrameBuf::with_size(format.channels, FLAC_BLOCK_SIZE).map_err(enc_err)?;
        let header = header_bytes(&stream_info)?;
        file.write_all(&header)?;
        Ok(FlacStreamWriter {
            file,
            config,
            stream_info,
            framebuf,
            context: Context::new(bits, format.channels),
            pending: Vec::with_capacity(FLAC_BLOCK_SIZE * format.channels),
            channels: format.channels,
            bits,
            header_len: header.len(),
            frames_encoded: 0,
            sink: ByteSink::new(),
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), FlacErr> {
        let block = FLAC_BLOCK_SIZE * self.channels;
        let bits = self.bits as u32;
        for chunk in samples.chunks(block) {
            let room = block - self.pending.len();
            let (now, later) = chunk.split_at(room.min(chunk.len()));
            self.pending.extend(now.iter().map(|&s| to_int(s, bits)));
            if self.pending.len() == block {
                self.encode_pending()?;
                self.pending.extend(later.iter().map(|&s| to_int(s, bits)));
            }
        }
        Ok(())
    }

    fn encode_pending(&mut self) -> Result<(), FlacErr> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // `(A, B)` implements `Fill` when both do: one call feeds the frame buffer and the
        // MD5/sample-count context.
        (&mut self.framebuf, &mut self.context)
            .fill_interleaved(&self.pending)
            .map_err(enc_err)?;
        let frame_number = self.context.current_frame_number().unwrap_or(0);
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.framebuf,
            frame_number,
            &self.stream_info,
        )
        .map_err(enc_err)?;
        self.stream_info.update_frame_info(&frame);
        self.sink.clear();
        frame.write(&mut self.sink).map_err(enc_err)?;
        self.file.write_all(self.sink.as_slice())?;
        self.frames_encoded += 1;
        self.pending.clear();
        Ok(())
    }

    fn finish(&mut self) -> Result<(), FlacErr> {
        self.encode_pending()?;
        let digest = self.context.md5_digest();
        self.stream_info.set_md5_digest(&digest);
        self.stream_info.set_total_samples(self.context.total_samples());
        // A fixed-block-size stream advertises min == max == nominal block size (the short
        // last block is exempt), as the reference encoder does.
        self.stream_info
            .set_block_sizes(FLAC_BLOCK_SIZE, FLAC_BLOCK_SIZE)
            .map_err(enc_err)?;
        if self.frames_encoded == 0 {
            // 0 means "unknown" for the frame-size fields.
            self.stream_info.set_frame_sizes(0, 0).map_err(enc_err)?;
        }
        let header = header_bytes(&self.stream_info)?;
        if header.len() != self.header_len {
            return Err(FlacErr::Encode("STREAMINFO size changed; refusing to corrupt file".into()));
        }
        self.file.flush()?;
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.flush()?;
        Ok(())
    }
}

/// Serialises `fLaC` + the STREAMINFO metadata block (42 bytes).
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>, FlacErr> {
    let stream = Stream::with_stream_info(info.clone());
    let mut sink = ByteSink::new();
    stream.write(&mut sink).map_err(enc_err)?;
    Ok(sink.as_slice().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_int_scaling() {
        assert_eq!(to_int(0.0, 16), 0);
        assert_eq!(to_int(0.5, 16), 16384);
        assert_eq!(to_int(-1.0, 16), -32768);
        assert_eq!(to_int(1.0, 16), 32767);
        assert_eq!(to_int(7.0, 16), 32767);
        assert_eq!(to_int(-7.0, 8), -128);
        assert_eq!(to_int(f32::NAN, 24), 0);
        assert_eq!(to_int(f32::INFINITY, 24), 8_388_607);
        assert_eq!(to_int(-1.0, 32), i32::MIN);
        assert_eq!(to_int(1.0, 32), i32::MAX);
        // Round trip of every 16-bit code.
        for s in [-32768i32, -12345, -1, 0, 1, 12345, 32767] {
            assert_eq!(to_int(s as f32 / 32768.0, 16), s);
        }
    }

    #[test]
    fn container_from_path() {
        assert_eq!(Container::from_path("a/b.WAV"), Some(Container::Wav));
        assert_eq!(Container::from_path("x.flac"), Some(Container::Flac));
        assert_eq!(Container::from_path("x.mp3"), None);
        assert_eq!(Container::from_path("noext"), None);
    }
}
