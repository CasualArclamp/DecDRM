//! Streaming WAV/FLAC reader built on symphonia.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

use crate::{AudioFormat, Error, Result};

/// Upper bound on the silence substituted for one undecodable packet (a FLAC block is at most
/// 65535 frames; this guards against a corrupt duration field).
const MAX_CONCEALED_FRAMES: u64 = 1 << 16;

/// Reads a WAV or FLAC file as normalised, interleaved `f32` samples, a block at a time.
///
/// Supported: WAV with 8/16/24/32-bit integer or 32/64-bit float PCM (plain and
/// `WAVE_FORMAT_EXTENSIBLE` headers), and FLAC at any bit depth. Integer samples are scaled
/// so that full scale maps to `[-1.0, 1.0)` (e.g. 16-bit `s` → `s / 32768`, unsigned 8-bit
/// `u` → `(u - 128) / 128`); float samples are passed through unchanged.
///
/// The file is decoded incrementally — memory use does not depend on the file length.
///
/// ```no_run
/// # fn main() -> decdrm_io::Result<()> {
/// let mut reader = decdrm_io::FileReader::open("samples/DW_ModeB_10kHz.flac")?;
/// println!("{} ({:?})", reader.format(), reader.duration());
/// while let Some(block) = reader.read(4800)? {
///     // `block` holds up to 4800 frames, interleaved.
/// #   let _ = block;
/// }
/// # Ok(()) }
/// ```
///
/// # Damaged files
///
/// A packet that fails to decode is replaced by silence of the packet's nominal length (and
/// counted in [`decode_errors`](Self::decode_errors)) instead of being dropped, so the sample
/// timeline — which the DRM demodulator depends on — stays intact. A truncated file simply
/// ends early.
pub struct FileReader {
    path: PathBuf,
    /// `None` for a valid but empty FLAC stream (see [`empty_flac_format`]).
    source: Option<Source>,
    time_base: Option<TimeBase>,
    format: AudioFormat,
    total_frames: Option<u64>,
    bits_per_sample: Option<u32>,
    container: &'static str,
    /// Decoded samples not yet returned; `pending[pending_pos..]` is still unread.
    pending: Vec<f32>,
    pending_pos: usize,
    /// Frames still to discard after a seek that landed before the requested frame.
    skip_frames: u64,
    position: u64,
    decode_errors: u64,
    eof: bool,
}

/// The demuxer/decoder pair for the selected track.
struct Source {
    // `Box<dyn Trait>` is an owned, heap-allocated value whose concrete type is only known at
    // run time: symphonia picks the WAV or FLAC reader after probing the file content.
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
}

// A compile-time check that the reader can be moved to another thread (e.g. an input thread
// in the engine). `FormatReader` and `AudioDecoder` are `Send + Sync` supertraits, so it is.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<FileReader>();
};

impl FileReader {
    /// Opens `path` and prepares the first audio track for decoding.
    ///
    /// The container is detected from the file content (the extension is only a hint).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|e| Error::io(&path, e))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let reader = match symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        ) {
            Ok(reader) => reader,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // symphonia's FLAC demuxer needs at least one audio frame; a zero-length FLAC
                // stream is still a valid (empty) file.
                if let Some((format, bits)) = empty_flac_format(&path) {
                    return Ok(FileReader::empty(path, format, bits));
                }
                return Err(Error::Decode { path, source: SymError::IoError(e) });
            }
            Err(source) => return Err(Error::Decode { path, source }),
        };

        let unsupported = |reason: &str| Error::UnsupportedFile {
            path: path.clone(),
            reason: reason.to_string(),
        };

        // `default_track` borrows `reader`; copy out everything we need before `reader` is
        // moved into the struct below (the borrow checker would otherwise reject the move).
        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| unsupported("no decodable audio track"))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| unsupported("audio track has no codec parameters"))?
            .clone();

        let sample_rate = params
            .sample_rate
            .filter(|&r| r > 0)
            .ok_or_else(|| unsupported("sample rate unknown"))?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count())
            .filter(|&c| (1..=64).contains(&c))
            .ok_or_else(|| unsupported("channel count unknown or unsupported"))?;

        // Prefer the exact frame count; fall back to the duration when the time base is one
        // tick per frame (true for WAV and FLAC).
        let total_frames = track.num_frames.or_else(|| match (time_base, track.duration) {
            (Some(tb), Some(dur)) if Some(tb) == TimeBase::try_from_recip(sample_rate) => {
                Some(dur.get())
            }
            _ => None,
        });

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|source| Error::Decode { path: path.clone(), source })?;

        let container = reader.format_info().short_name;

        Ok(FileReader {
            path,
            source: Some(Source { reader, decoder, track_id }),
            time_base,
            format: AudioFormat::new(sample_rate, channels),
            total_frames,
            bits_per_sample: params.bits_per_sample,
            container,
            pending: Vec::new(),
            pending_pos: 0,
            skip_frames: 0,
            position: 0,
            decode_errors: 0,
            eof: false,
        })
    }

    /// A reader for a file that is known to contain no audio.
    fn empty(path: PathBuf, format: AudioFormat, bits: u32) -> Self {
        FileReader {
            path,
            source: None,
            time_base: TimeBase::try_from_recip(format.sample_rate),
            format,
            total_frames: Some(0),
            bits_per_sample: Some(bits),
            container: "flac",
            pending: Vec::new(),
            pending_pos: 0,
            skip_frames: 0,
            position: 0,
            decode_errors: 0,
            eof: true,
        }
    }

    /// Sample rate and channel count of the file.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Total number of frames, if the container states it.
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// Playing time, if the container states the length.
    pub fn duration(&self) -> Option<Duration> {
        self.total_frames.map(|n| self.format.frames_to_duration(n))
    }

    /// Bits per sample of the stored data (8, 16, 24, 32 or 64), if known.
    pub fn bits_per_sample(&self) -> Option<u32> {
        self.bits_per_sample
    }

    /// Short name of the detected container, e.g. `"wave"` or `"flac"`.
    pub fn container(&self) -> &'static str {
        self.container
    }

    /// The path this reader was opened with.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Index of the next frame [`read`](Self::read) will return (frames since the start of
    /// the file).
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Number of packets that failed to decode and were replaced by silence.
    pub fn decode_errors(&self) -> u64 {
        self.decode_errors
    }

    /// Reads up to `max_frames` frames.
    ///
    /// Returns `Ok(Some(samples))` with `1..=max_frames` whole frames (interleaved), or
    /// `Ok(None)` at the end of the file. Blocks shorter than `max_frames` only occur at the
    /// end of the file. `max_frames == 0` is rejected, because it could never make progress.
    pub fn read(&mut self, max_frames: usize) -> Result<Option<Vec<f32>>> {
        if max_frames == 0 {
            return Err(Error::invalid("FileReader::read: max_frames must be > 0"));
        }
        let want = max_frames.saturating_mul(self.format.channels);
        // Cap the up-front reservation so a huge `max_frames` does not allocate eagerly.
        let mut out = Vec::with_capacity(want.min(1 << 20));
        self.fill(&mut out, want)?;
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    /// Like [`read`](Self::read) but appends into a caller-provided buffer (no allocation once
    /// `out` has grown). Returns the number of frames appended; `0` means end of file.
    pub fn read_into(&mut self, out: &mut Vec<f32>, max_frames: usize) -> Result<usize> {
        let start = out.len();
        let want = max_frames.saturating_mul(self.format.channels);
        let target = start.saturating_add(want);
        self.fill_to(out, target)?;
        Ok((out.len() - start) / self.format.channels)
    }

    /// Reads everything from the current position to the end of the file.
    pub fn read_all(&mut self) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        if let Some(total) = self.total_frames {
            let remaining = total.saturating_sub(self.position) as usize;
            out.reserve(remaining.saturating_mul(self.format.channels).min(1 << 28));
        }
        self.fill_to(&mut out, usize::MAX)?;
        Ok(out)
    }

    /// Moves to frame `frame` (counted from the start of the file) so that the next
    /// [`read`](Self::read) starts exactly there. Returns the new position; seeking past
    /// the end positions the reader at the end.
    pub fn seek(&mut self, frame: u64) -> Result<u64> {
        let rate = self.format.sample_rate;
        if self.time_base.is_none() || self.time_base != TimeBase::try_from_recip(rate) {
            return Err(Error::UnsupportedFile {
                path: self.path.clone(),
                reason: "seeking needs a one-tick-per-frame time base".into(),
            });
        }
        self.pending.clear();
        self.pending_pos = 0;
        self.skip_frames = 0;
        if let Some(total) = self.total_frames
            && frame >= total
        {
            self.eof = true;
            self.position = total;
            return Ok(total);
        }
        let Some(src) = self.source.as_mut() else {
            return Ok(self.position);
        };
        let ts = Timestamp::new(i64::try_from(frame).unwrap_or(i64::MAX));
        let seeked = src
            .reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: src.track_id })
            .map_err(|source| Error::Decode { path: self.path.clone(), source })?;
        // After a seek the decoder's inter-packet state is stale.
        src.decoder.reset();
        self.eof = false;
        let actual = u64::try_from(seeked.actual_ts.get()).unwrap_or(0);
        // An accurate seek lands on the packet containing `frame`; decode and drop the lead-in.
        self.skip_frames = frame.saturating_sub(actual);
        self.position = frame.max(actual);
        Ok(self.position)
    }

    fn fill(&mut self, out: &mut Vec<f32>, want: usize) -> Result<()> {
        let target = out.len().saturating_add(want);
        self.fill_to(out, target)
    }

    /// Appends decoded samples to `out` until it holds `target` samples or the file ends.
    fn fill_to(&mut self, out: &mut Vec<f32>, target: usize) -> Result<()> {
        let ch = self.format.channels;
        let start = out.len();
        // Only ever hand out whole frames.
        let target = start + (target.saturating_sub(start) / ch) * ch;
        while out.len() < target {
            if self.pending_pos >= self.pending.len() {
                if self.eof || !self.decode_next()? {
                    break;
                }
                continue;
            }
            let n = (self.pending.len() - self.pending_pos).min(target - out.len());
            out.extend_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
            self.pending_pos += n;
        }
        self.position += ((out.len() - start) / ch) as u64;
        Ok(())
    }

    /// Decodes packets until `pending` holds at least one frame. Returns `false` at the end of
    /// the stream.
    fn decode_next(&mut self) -> Result<bool> {
        let ch = self.format.channels;
        let Some(src) = self.source.as_mut() else {
            self.eof = true;
            return Ok(false);
        };
        loop {
            self.pending.clear();
            self.pending_pos = 0;

            let packet = match src.reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.eof = true;
                    return Ok(false);
                }
                // A truncated file: treat like a normal end of stream.
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(false);
                }
                // Chained streams (a new track list) are not supported; stop cleanly.
                Err(SymError::ResetRequired) => {
                    self.eof = true;
                    return Ok(false);
                }
                Err(source) => return Err(Error::Decode { path: self.path.clone(), source }),
            };
            if packet.track_id != src.track_id {
                continue;
            }

            match src.decoder.decode(&packet) {
                Ok(buf) => {
                    let buf_ch = buf.spec().channels().count();
                    if buf_ch != ch {
                        return Err(Error::UnsupportedFile {
                            path: self.path.clone(),
                            reason: format!("channel count changed mid-stream ({ch} -> {buf_ch})"),
                        });
                    }
                    // Converts from whatever integer/float type the codec produced to f32 with
                    // full-scale normalisation, resizing `pending` to fit.
                    buf.copy_to_vec_interleaved(&mut self.pending);
                }
                Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => {
                    // Conceal: keep the timeline by substituting silence.
                    self.decode_errors += 1;
                    let frames = packet.dur.get().min(MAX_CONCEALED_FRAMES) as usize;
                    self.pending.resize(frames * ch, 0.0);
                }
                Err(source) => return Err(Error::Decode { path: self.path.clone(), source }),
            }

            if self.skip_frames > 0 {
                let frames = (self.pending.len() / ch) as u64;
                let drop = self.skip_frames.min(frames);
                self.skip_frames -= drop;
                self.pending_pos = drop as usize * ch;
            }
            if self.pending_pos < self.pending.len() {
                return Ok(true);
            }
        }
    }
}

impl std::fmt::Debug for FileReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReader")
            .field("path", &self.path)
            .field("container", &self.container)
            .field("format", &self.format)
            .field("bits_per_sample", &self.bits_per_sample)
            .field("total_frames", &self.total_frames)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// If `path` is a FLAC stream whose STREAMINFO declares zero samples, returns its format and
/// bit depth. Layout: `fLaC`, a 4-byte metadata block header (type 0 = STREAMINFO, length 34),
/// then STREAMINFO, whose bytes 10..18 pack sample rate (20 bits), channels − 1 (3),
/// bits per sample − 1 (5) and the total sample count (36) — see RFC 9639 §8.2.
fn empty_flac_format(path: &Path) -> Option<(AudioFormat, u32)> {
    let mut head = [0u8; 42];
    let mut file = File::open(path).ok()?;
    std::io::Read::read_exact(&mut file, &mut head).ok()?;
    if &head[..4] != b"fLaC" || head[4] & 0x7f != 0 || head[5..8] != [0, 0, 34] {
        return None;
    }
    let packed = u64::from_be_bytes(head[18..26].try_into().ok()?);
    let rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 0x7) as usize + 1;
    let bits = ((packed >> 36) & 0x1f) as u32 + 1;
    let total = packed & ((1 << 36) - 1);
    (total == 0 && rate > 0).then_some((AudioFormat::new(rate, channels), bits))
}
