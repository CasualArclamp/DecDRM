//! Stream formats and their decoders:
//!
//! | Format | Framing | Decoder |
//! |---|---|---|
//! | MPEG audio (MP3, MP2, MP1) | [`Framer`]`<`[`MpegHeader`]`>` | symphonia |
//! | AAC, HE-AAC, HE-AAC v2 in ADTS | [`Framer`]`<`[`AdtsHeader`]`>` | FDK-AAC ([`FdkAdtsDecoder`]) |
//! | Ogg Vorbis | [`ogg`] (chained streams) | symphonia |
//! | Ogg Opus | [`ogg`] | libopus ([`OpusDrmDecoder`], RFC 7845 pre-skip and gain) |
//! | Ogg FLAC, native FLAC | [`ogg`] / [`FlacFramer`] | symphonia |
//!
//! The format is found from the first bytes ([`sniff`]) and, failing that, from the
//! response's `Content-Type` ([`classify`]); the bytes win when the two disagree (servers
//! label AAC streams `audio/mpeg` now and then).

use super::framing::{AdtsHeader, FrameHeader, Framer, MpegHeader, MpegVersion, has_frames, id3v2_len};
use super::ogg::{self, BOS, EOS, PageReader, Packets};
use super::playlist;
use decdrm_codecs::{DrmAudioDecoder, FdkAdtsDecoder, OpusDrmDecoder, PcmFrame};
use std::collections::VecDeque;
use std::io::{self, Read};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_FLAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_VORBIS};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::packet::PacketRef;
use symphonia::core::units::{Duration as Frames, Timestamp};
use symphonia::default::codecs::{FlacDecoder, MpaDecoder, VorbisDecoder};

/// Consecutive undecodable frames after which a stream is given up.
const MAX_CONSECUTIVE_ERRORS: u64 = 100;
/// Bytes searched for frames before a stream is declared not to be of its format.
const MAX_SEARCH_BYTES: u64 = 512 * 1024;

/// A stream format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Mpeg,
    Adts,
    Ogg,
    Flac,
}

/// What to do with a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Class {
    Audio(Format),
    Playlist,
    /// Not usable, with the reason.
    Rejected(String),
}

/// The format the first bytes show, if they are conclusive.
pub(crate) fn sniff(head: &[u8]) -> Option<Format> {
    let head = match id3v2_len(head) {
        Some(n) if n < head.len() => &head[n..],
        Some(_) => return None,
        None => head,
    };
    if head.starts_with(b"OggS") {
        Some(Format::Ogg)
    } else if head.starts_with(b"fLaC") {
        Some(Format::Flac)
    } else if has_frames::<AdtsHeader>(head, 16 * 1024) {
        Some(Format::Adts)
    } else if has_frames::<MpegHeader>(head, 16 * 1024) {
        Some(Format::Mpeg)
    } else {
        None
    }
}

/// Decide from the first bytes, the `Content-Type` and the URL path what a response is.
pub(crate) fn classify(content_type: Option<&str>, path: &str, head: &[u8]) -> Class {
    let ct = content_type.map(|c| c.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()).unwrap_or_default();
    if let Some(f) = sniff(head) {
        return Class::Audio(f);
    }
    let is_text = !head.is_empty() && std::str::from_utf8(head).is_ok_and(|s| !s.contains('\0'));
    if playlist::looks_like_playlist(head) || playlist::is_playlist_type(&ct) || (is_text && playlist::is_playlist_path(path)) {
        return Class::Playlist;
    }
    let lower = String::from_utf8_lossy(&head[..head.len().min(512)]).to_ascii_lowercase();
    let html = ct == "text/html" || ct == "application/xhtml+xml" || lower.contains("<html") || lower.contains("<!doctype");
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" | "audio/mpeg3" | "audio/x-mp3" | "audio/mpg" | "audio/x-mpg" => {
            Class::Audio(Format::Mpeg)
        }
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/x-aacp" | "audio/aac-adts" | "audio/x-hx-aac-adts"
        | "audio/vnd.dlna.adts" => Class::Audio(Format::Adts),
        "application/ogg" | "audio/ogg" | "audio/x-ogg" | "application/x-ogg" | "audio/opus" | "audio/vorbis" => {
            Class::Audio(Format::Ogg)
        }
        "audio/flac" | "audio/x-flac" => Class::Audio(Format::Flac),
        _ if html => Class::Rejected("the server sent a web page (text/html), not an audio stream".into()),
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "application/dash+xml" | "audio/webm" | "audio/x-ms-wma" => Class::Rejected(format!(
            "unsupported stream type {ct} (supported: MP3, AAC/HE-AAC in ADTS, Ogg Vorbis, Ogg Opus, FLAC)"
        )),
        _ if ct.starts_with("video/") => Class::Rejected(format!("{ct} is a video stream, not an audio stream")),
        "" => Class::Rejected("unrecognised stream format (no Content-Type, and the data is not MP3, AAC, Ogg or FLAC)".into()),
        _ => Class::Rejected(format!(
            "unrecognised stream format (Content-Type {ct}; supported: MP3, AAC/HE-AAC in ADTS, Ogg Vorbis, Ogg Opus, FLAC)"
        )),
    }
}

/// A block of decoded audio (interleaved).
#[derive(Debug, Clone)]
pub(crate) struct Decoded {
    pub rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl Decoded {
    fn silence(rate: u32, channels: usize, frames: usize) -> Self {
        Decoded { rate, channels, samples: vec![0.0; frames * channels] }
    }

    fn from_pcm(f: PcmFrame) -> Self {
        Decoded { rate: f.sample_rate, channels: usize::from(f.channels), samples: f.samples }
    }
}

/// Why decoding stopped.
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// Reading the stream failed (the connection).
    Io(io::Error),
    /// The stream cannot be decoded (unsupported coding, nothing decodable).
    Fatal(String),
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

/// A decoder for one connection's stream.
pub(crate) trait StreamDecoder {
    /// The next block of audio, reading the stream as needed; `None` at its end.
    fn next(&mut self, input: &mut dyn Read) -> Result<Option<Decoded>, DecodeError>;
    /// The coding, e.g. `"MP3"`, `"HE-AAC v2"`, `"Ogg Opus"`.
    fn codec(&self) -> String;
    /// The bit rate the stream itself declares, bit/s.
    fn nominal_bitrate(&self) -> Option<u32> {
        None
    }
    /// The stream's channel count when it differs from the decoded blocks' (FDK
    /// delivers mono HE-AAC as stereo).
    fn channels(&self) -> Option<u16> {
        None
    }
    /// A title from inside the stream (Ogg/FLAC comments) when it changed.
    fn take_title(&mut self) -> Option<String> {
        None
    }
    /// Frames that could not be decoded so far.
    fn errors(&self) -> u64;
}

/// The decoder of `format`.
pub(crate) fn open(format: Format) -> Box<dyn StreamDecoder> {
    match format {
        Format::Mpeg => Box::new(MpegDecoder::new()),
        Format::Adts => Box::new(AdtsDecoder::new()),
        Format::Ogg => Box::new(OggDecoder::new()),
        Format::Flac => Box::new(FlacStreamDecoder::new()),
    }
}

/// Counts consecutive errors and gives up after [`MAX_CONSECUTIVE_ERRORS`].
#[derive(Default)]
struct ErrorCount {
    total: u64,
    run: u64,
}

impl ErrorCount {
    fn error(&mut self, what: impl FnOnce() -> String) -> Result<(), DecodeError> {
        self.total += 1;
        self.run += 1;
        if self.run >= MAX_CONSECUTIVE_ERRORS {
            return Err(DecodeError::Fatal(format!("{MAX_CONSECUTIVE_ERRORS} frames in a row failed to decode ({})", what())));
        }
        Ok(())
    }

    fn ok(&mut self) {
        self.run = 0;
    }
}

/// A framer's error: `InvalidData` means the data is not of the format (it searched
/// [`MAX_SEARCH_BYTES`] without finding a frame), anything else is the connection.
fn framing(what: &str) -> impl Fn(io::Error) -> DecodeError + '_ {
    move |e| match e.kind() {
        io::ErrorKind::InvalidData => DecodeError::Fatal(format!("{what}: {e}")),
        _ => DecodeError::Io(e),
    }
}

fn options() -> AudioDecoderOptions {
    AudioDecoderOptions::default()
}

// ---------------------------------------------------------------------------------
// MPEG audio
// ---------------------------------------------------------------------------------

/// MP3/MP2/MP1 through symphonia's decoder, one frame per packet.
struct MpegDecoder {
    framer: Framer<MpegHeader>,
    /// The decoder and the header it was made for (a new one when the format changes).
    dec: Option<(MpegHeader, MpaDecoder)>,
    errors: ErrorCount,
    bitrate_sum: f64,
    frames: u64,
}

impl MpegDecoder {
    fn new() -> Self {
        MpegDecoder { framer: Framer::new(MAX_SEARCH_BYTES), dec: None, errors: ErrorCount::default(), bitrate_sum: 0.0, frames: 0 }
    }
}

impl StreamDecoder for MpegDecoder {
    fn next(&mut self, input: &mut dyn Read) -> Result<Option<Decoded>, DecodeError> {
        let Some((h, frame)) = self.framer.next_frame(input).map_err(framing("MPEG audio"))? else { return Ok(None) };
        self.bitrate_sum += f64::from(h.bitrate);
        self.frames += 1;
        let same = self.dec.as_ref().is_some_and(|(k, _)| k.compatible(&h) && k.channels == h.channels);
        if !same {
            let mut params = AudioCodecParameters::new();
            let codec = match h.layer {
                1 => CODEC_ID_MP1,
                2 => CODEC_ID_MP2,
                _ => CODEC_ID_MP3,
            };
            params.for_codec(codec).with_sample_rate(h.sample_rate);
            let dec = MpaDecoder::try_new(&params, &options())
                .map_err(|e| DecodeError::Fatal(format!("MPEG audio decoder: {e}")))?;
            self.dec = Some((h, dec));
        }
        let (_, dec) = self.dec.as_mut().expect("created above");
        let packet = PacketRef::new(0, Timestamp::new(0), Frames::new(h.samples as u64), &frame);
        let mut samples = Vec::new();
        match dec.decode_ref(&packet) {
            Ok(buf) => {
                buf.copy_to_vec_interleaved(&mut samples);
                self.errors.ok();
                Ok(Some(Decoded { rate: h.sample_rate, channels: usize::from(h.channels), samples }))
            }
            Err(e) => {
                // The first frames after joining a stream may refer to bit-reservoir data
                // that was never received; keep the timing with silence.
                self.errors.error(|| e.to_string())?;
                Ok(Some(Decoded::silence(h.sample_rate, usize::from(h.channels), h.samples)))
            }
        }
    }

    fn codec(&self) -> String {
        match self.dec.as_ref().map(|(h, _)| *h) {
            Some(h) => {
                let version = match h.version {
                    MpegVersion::Mpeg1 => "",
                    MpegVersion::Mpeg2 => " (MPEG-2)",
                    MpegVersion::Mpeg25 => " (MPEG-2.5)",
                };
                format!("MP{}{version}", h.layer)
            }
            None => "MPEG audio".into(),
        }
    }

    fn nominal_bitrate(&self) -> Option<u32> {
        (self.frames > 0).then(|| (self.bitrate_sum / self.frames as f64).round() as u32)
    }

    fn errors(&self) -> u64 {
        self.errors.total
    }
}

// ---------------------------------------------------------------------------------
// ADTS
// ---------------------------------------------------------------------------------

/// AAC in ADTS through FDK-AAC (which finds SBR and PS in the payload).
struct AdtsDecoder {
    framer: Framer<AdtsHeader>,
    fdk: Option<FdkAdtsDecoder>,
    header: Option<AdtsHeader>,
    pending: VecDeque<PcmFrame>,
    errors: ErrorCount,
}

impl AdtsDecoder {
    fn new() -> Self {
        AdtsDecoder { framer: Framer::new(MAX_SEARCH_BYTES), fdk: None, header: None, pending: VecDeque::new(), errors: ErrorCount::default() }
    }
}

impl StreamDecoder for AdtsDecoder {
    fn next(&mut self, input: &mut dyn Read) -> Result<Option<Decoded>, DecodeError> {
        loop {
            if let Some(f) = self.pending.pop_front() {
                return Ok(Some(Decoded::from_pcm(f)));
            }
            let Some((h, frame)) = self.framer.next_frame(input).map_err(framing("ADTS"))? else { return Ok(None) };
            if h.object_type != 2 {
                let name = match h.object_type {
                    1 => "AAC Main",
                    3 => "AAC SSR",
                    _ => "AAC LTP",
                };
                return Err(DecodeError::Fatal(format!("{name} is not supported (AAC-LC, HE-AAC and HE-AAC v2 are)")));
            }
            if self.header.is_none_or(|old| !old.compatible(&h)) || self.fdk.is_none() {
                self.fdk = Some(FdkAdtsDecoder::new().map_err(|e| DecodeError::Fatal(format!("AAC decoder: {e}")))?);
                self.header = Some(h);
            }
            match self.fdk.as_mut().expect("created above").decode(&frame) {
                Ok(frames) => {
                    if frames.iter().any(|f| !f.concealed) {
                        self.errors.ok();
                    }
                    for f in frames {
                        if f.concealed {
                            self.errors.error(|| "concealed by the AAC decoder".into())?;
                        }
                        self.pending.push_back(f);
                    }
                }
                Err(e) => {
                    self.errors.error(|| e.to_string())?;
                    self.fdk = None; // a fresh decoder for the next frame
                }
            }
        }
    }

    fn codec(&self) -> String {
        self.fdk.as_ref().and_then(FdkAdtsDecoder::coding).unwrap_or("AAC").to_string()
    }

    fn channels(&self) -> Option<u16> {
        self.fdk.as_ref().and_then(FdkAdtsDecoder::stream_channels)
    }

    fn errors(&self) -> u64 {
        self.errors.total
    }
}

// ---------------------------------------------------------------------------------
// Ogg
// ---------------------------------------------------------------------------------

/// The codec of an Ogg logical stream and its decoder state.
enum OggCodec {
    /// Waiting for the comment and setup headers.
    Vorbis { ident: Vec<u8>, rate: u32, channels: usize, nominal: Option<u32>, dec: Option<Box<VorbisDecoder>> },
    Opus { dec: Box<OpusDrmDecoder>, channels: usize, pre_skip: usize, gain: f32 },
    Flac { dec: Box<FlacDecoder>, rate: u32, channels: usize },
}

/// The logical stream being played.
struct Logical {
    serial: u32,
    packets: Packets,
    codec: OggCodec,
    /// Audio packets have arrived (so a new BOS page means a chained stream).
    audio_seen: bool,
    ended: bool,
}

/// Vorbis, Opus or FLAC in Ogg, following chained streams.
struct OggDecoder {
    pages: PageReader,
    stream: Option<Logical>,
    /// Names of logical streams that are not audio DecDRM decodes.
    others: Vec<String>,
    pages_without_audio: u32,
    title: Option<String>,
    errors: ErrorCount,
}

/// Append decoded samples to the block of a page.
fn append(out: &mut Option<Decoded>, rate: u32, channels: usize, samples: &[f32]) {
    match out {
        Some(d) if d.rate == rate && d.channels == channels => d.samples.extend_from_slice(samples),
        Some(_) => {} // a format change within a page: drop the rest of it
        None => *out = Some(Decoded { rate, channels, samples: samples.to_vec() }),
    }
}

/// The codec of a logical stream from its first packet: `Ok(None)` for a stream to
/// ignore (e.g. a Skeleton index), `Err` for audio DecDRM cannot decode.
fn identify(first: &[u8]) -> Result<Option<OggCodec>, String> {
    if first.len() >= 30 && first.starts_with(b"\x01vorbis") {
        let channels = usize::from(first[11]);
        let rate = u32::from_le_bytes(first[12..16].try_into().expect("4 bytes"));
        let nominal = i32::from_le_bytes(first[20..24].try_into().expect("4 bytes"));
        if channels == 0 || rate == 0 {
            return Err("Vorbis header with zero channels or rate".into());
        }
        return Ok(Some(OggCodec::Vorbis {
            ident: first.to_vec(),
            rate,
            channels,
            nominal: u32::try_from(nominal).ok().filter(|&b| b > 0),
            dec: None,
        }));
    }
    if first.len() >= 19 && first.starts_with(b"OpusHead") {
        let channels = usize::from(first[9]);
        let pre_skip = usize::from(u16::from_le_bytes([first[10], first[11]]));
        let gain_q8 = i16::from_le_bytes([first[16], first[17]]);
        let family = first[18];
        // Mapping family 0 is mono/stereo; family 1 with one stream and at most two
        // channels is a plain Opus stream too.
        let single_stream = family == 0 || (family == 1 && channels <= 2 && first.get(19) == Some(&1));
        if channels == 0 || channels > 2 || !single_stream {
            return Err(format!("Ogg Opus with {channels} channels (mapping family {family}) is not supported"));
        }
        let dec = OpusDrmDecoder::new().map_err(|e| format!("Opus decoder: {e}"))?;
        return Ok(Some(OggCodec::Opus {
            dec: Box::new(dec),
            channels,
            pre_skip,
            gain: 10f32.powf(f32::from(gain_q8) / 256.0 / 20.0),
        }));
    }
    if first.len() >= 51 && first.starts_with(b"\x7FFLAC") && &first[9..13] == b"fLaC" {
        let info = &first[17..51];
        let (dec, rate, channels) = flac_decoder(info)?;
        return Ok(Some(OggCodec::Flac { dec: Box::new(dec), rate, channels }));
    }
    // Video and Skeleton (index) streams next to the audio are ignored.
    if [&b"\x80theora"[..], b"fishead", b"\x00fishead", b"fisbone"].iter().any(|m| first.starts_with(m)) {
        return Ok(None);
    }
    let name = if first.starts_with(b"Speex") {
        "Speex"
    } else if first.starts_with(b"OggMIDI") || first.starts_with(b"\x7FMIDI") {
        "MIDI"
    } else {
        "an unknown codec"
    };
    Err(format!("Ogg stream with {name}"))
}

/// A FLAC decoder for a 34-byte STREAMINFO block; its rate and channel count.
fn flac_decoder(info: &[u8]) -> Result<(FlacDecoder, u32, usize), String> {
    if info.len() < 34 {
        return Err("FLAC STREAMINFO too short".into());
    }
    let rate = (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | (u32::from(info[12]) >> 4);
    let channels = usize::from((info[12] >> 1) & 7) + 1;
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_FLAC).with_extra_data(info[..34].to_vec().into_boxed_slice());
    let dec = FlacDecoder::try_new(&params, &options()).map_err(|e| format!("FLAC decoder: {e}"))?;
    Ok((dec, rate, channels))
}

impl OggDecoder {
    fn new() -> Self {
        OggDecoder { pages: PageReader::new(MAX_SEARCH_BYTES), stream: None, others: Vec::new(), pages_without_audio: 0, title: None, errors: ErrorCount::default() }
    }

    /// Handle one packet of the current stream, appending audio to `out`.
    fn packet(&mut self, packet: &[u8], out: &mut Option<Decoded>) -> Result<(), DecodeError> {
        let Some(s) = self.stream.as_mut() else { return Ok(()) };
        // Rust note: `s` borrows only the field `self.stream`, so `self.title` and
        // `self.errors` stay usable below.
        match &mut s.codec {
            OggCodec::Vorbis { ident, rate, channels, dec, .. } => {
                if packet.starts_with(b"\x03vorbis") {
                    self.title = ogg::title_of(&ogg::comments(&packet[7..])).or(self.title.take());
                } else if packet.starts_with(b"\x05vorbis") {
                    let mut extra = ident.clone();
                    extra.extend_from_slice(packet);
                    let mut params = AudioCodecParameters::new();
                    params.for_codec(CODEC_ID_VORBIS).with_sample_rate(*rate).with_extra_data(extra.into_boxed_slice());
                    let d = VorbisDecoder::try_new(&params, &options())
                        .map_err(|e| DecodeError::Fatal(format!("Vorbis setup header: {e}")))?;
                    *dec = Some(Box::new(d));
                } else if packet.first().is_some_and(|b| b & 1 == 0) {
                    s.audio_seen = true;
                    let Some(d) = dec.as_mut() else { return Ok(()) };
                    let pk = PacketRef::new(0, Timestamp::new(0), Frames::new(0), packet);
                    match d.decode_ref(&pk) {
                        Ok(buf) => {
                            let mut samples = Vec::new();
                            buf.copy_to_vec_interleaved(&mut samples);
                            self.errors.ok();
                            if !samples.is_empty() {
                                append(out, *rate, *channels, &samples);
                            }
                        }
                        Err(e) => self.errors.error(|| format!("Vorbis: {e}"))?,
                    }
                }
            }
            OggCodec::Opus { dec, pre_skip, gain, .. } => {
                if packet.starts_with(b"OpusTags") {
                    self.title = ogg::title_of(&ogg::comments(&packet[8..])).or(self.title.take());
                } else if !packet.starts_with(b"OpusHead") && !packet.is_empty() {
                    s.audio_seen = true;
                    match dec.decode(packet, None) {
                        Ok(pcm) => {
                            if pcm.concealed {
                                self.errors.error(|| "undecodable Opus packet".into())?;
                            } else {
                                self.errors.ok();
                            }
                            let ch = usize::from(pcm.channels);
                            let skip = (*pre_skip).min(pcm.frames());
                            *pre_skip -= skip;
                            let mut samples = pcm.samples[skip * ch..].to_vec();
                            if *gain != 1.0 {
                                samples.iter_mut().for_each(|v| *v = (*v * *gain).clamp(-1.0, 1.0));
                            }
                            append(out, pcm.sample_rate, ch, &samples);
                        }
                        Err(e) => self.errors.error(|| format!("Opus: {e}"))?,
                    }
                }
            }
            OggCodec::Flac { dec, rate, channels } => {
                if packet.first() == Some(&0xFF) {
                    s.audio_seen = true;
                    let pk = PacketRef::new(0, Timestamp::new(0), Frames::new(0), packet);
                    match dec.decode_ref(&pk) {
                        Ok(buf) => {
                            let mut samples = Vec::new();
                            buf.copy_to_vec_interleaved(&mut samples);
                            self.errors.ok();
                            append(out, *rate, *channels, &samples);
                        }
                        Err(e) => self.errors.error(|| format!("FLAC: {e}"))?,
                    }
                } else if packet.first().is_some_and(|b| b & 0x7F == 4) && packet.len() > 4 {
                    // A VORBIS_COMMENT metadata block.
                    self.title = ogg::title_of(&ogg::comments(&packet[4..])).or(self.title.take());
                }
            }
        }
        Ok(())
    }
}

impl StreamDecoder for OggDecoder {
    fn next(&mut self, input: &mut dyn Read) -> Result<Option<Decoded>, DecodeError> {
        loop {
            let Some(page) = self.pages.next_page(input).map_err(framing("Ogg"))? else { return Ok(None) };
            if page.flags & BOS != 0 {
                // A new logical stream: a chained stream follows ours (it ended, or has
                // sent audio), or one grouped with it during the headers (ignored).
                if self.stream.as_ref().is_none_or(|s| s.ended || s.audio_seen) {
                    let mut first = Vec::new();
                    Packets::new().push(&page, &mut first);
                    match first.first().map(|p| identify(p)) {
                        Some(Ok(Some(codec))) => {
                            self.stream = Some(Logical { serial: page.serial, packets: Packets::new(), codec, audio_seen: false, ended: false });
                            self.pages_without_audio = 0;
                        }
                        Some(Err(name)) => self.others.push(name),
                        _ => {}
                    }
                }
                continue;
            }
            let Some(s) = self.stream.as_mut().filter(|s| s.serial == page.serial) else {
                self.pages_without_audio += 1;
                if self.stream.is_none() && self.pages_without_audio > 64 {
                    return Err(DecodeError::Fatal(match self.others.first() {
                        Some(what) => format!("{what}, which is not supported (Vorbis, Opus and FLAC are)"),
                        None => "Ogg stream without a Vorbis, Opus or FLAC stream".into(),
                    }));
                }
                continue;
            };
            let mut packets = Vec::new();
            s.packets.push(&page, &mut packets);
            if page.flags & EOS != 0 {
                s.ended = true;
            }
            let mut out = None;
            for p in &packets {
                self.packet(p, &mut out)?;
            }
            if let Some(d) = out {
                return Ok(Some(d));
            }
        }
    }

    fn codec(&self) -> String {
        match self.stream.as_ref().map(|s| &s.codec) {
            Some(OggCodec::Vorbis { channels, .. }) => format!("Ogg Vorbis{}", if *channels == 1 { " mono" } else { "" }),
            Some(OggCodec::Opus { channels, .. }) => format!("Ogg Opus{}", if *channels == 1 { " mono" } else { "" }),
            Some(OggCodec::Flac { .. }) => "Ogg FLAC".into(),
            None => "Ogg".into(),
        }
    }

    fn nominal_bitrate(&self) -> Option<u32> {
        match self.stream.as_ref().map(|s| &s.codec) {
            Some(OggCodec::Vorbis { nominal, .. }) => *nominal,
            _ => None,
        }
    }

    fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    fn errors(&self) -> u64 {
        self.errors.total
    }
}

// ---------------------------------------------------------------------------------
// Native FLAC
// ---------------------------------------------------------------------------------

/// CRC-8 of FLAC frame headers (polynomial 0x07, initial value 0).
fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
        crc
    })
}

/// CRC-16 of FLAC frames (polynomial 0x8005, initial value 0), updated by one byte.
fn crc16_update(crc: u16, b: u8) -> u16 {
    let mut crc = crc ^ (u16::from(b) << 8);
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
    }
    crc
}

/// What a FLAC frame header says (the fields that can stand in for STREAMINFO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlacFrameHeader {
    /// Header length including its CRC-8.
    len: usize,
    block_size: u32,
    /// 0: "see STREAMINFO".
    sample_rate: u32,
    channels: u8,
    /// 0: "see STREAMINFO".
    bits: u8,
}

/// Parse a FLAC frame header (FLAC format, "FRAME_HEADER"); `None` unless its sync code,
/// reserved bits and CRC-8 are right. `Err(())`: more bytes are needed.
fn flac_frame_header(b: &[u8]) -> Result<Option<FlacFrameHeader>, ()> {
    if b.len() < 2 {
        return Err(());
    }
    if b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
        return Ok(None);
    }
    if b.len() < 5 {
        return Err(());
    }
    let (bs_code, sr_code) = (b[2] >> 4, b[2] & 0x0F);
    let (ch_code, ss_code) = (b[3] >> 4, (b[3] >> 1) & 7);
    if bs_code == 0 || sr_code == 15 || ch_code > 10 || ss_code == 3 || b[3] & 1 != 0 {
        return Ok(None);
    }
    // The UTF-8-like coded frame or sample number.
    let first = b[4];
    let extra = match first.leading_ones() {
        0 => 0,
        n @ 2..=7 => n as usize - 1,
        _ => return Ok(None),
    };
    let mut p = 5 + extra;
    let bs_extra = match bs_code {
        6 => 1,
        7 => 2,
        _ => 0,
    };
    let sr_extra = match sr_code {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    if b.len() < p + bs_extra + sr_extra + 1 {
        return Err(());
    }
    let block_size = match bs_code {
        1 => 192,
        2..=5 => 576 << (bs_code - 2),
        6 => u32::from(b[p]) + 1,
        7 => u32::from(u16::from_be_bytes([b[p], b[p + 1]])) + 1,
        _ => 256 << (bs_code - 8),
    };
    p += bs_extra;
    const RATES: [u32; 12] = [0, 88_200, 176_400, 192_000, 8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 96_000];
    let sample_rate = match sr_code {
        0..=11 => RATES[usize::from(sr_code)],
        12 => u32::from(b[p]) * 1000,
        13 => u32::from(u16::from_be_bytes([b[p], b[p + 1]])),
        _ => u32::from(u16::from_be_bytes([b[p], b[p + 1]])) * 10,
    };
    p += sr_extra;
    if crc8(&b[..p]) != b[p] {
        return Ok(None);
    }
    let channels = if ch_code <= 7 { ch_code + 1 } else { 2 };
    let bits = [0, 8, 12, 0, 16, 20, 24, 32][usize::from(ss_code)];
    Ok(Some(FlacFrameHeader { len: p + 1, block_size, sample_rate, channels, bits }))
}

/// A STREAMINFO block standing in for a missing stream header (a stream joined
/// mid-way), from a frame header that names its rate and sample size.
fn synthetic_streaminfo(h: &FlacFrameHeader) -> Option<[u8; 34]> {
    if h.sample_rate == 0 || h.bits == 0 {
        return None;
    }
    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&16u16.to_be_bytes()); // minimum block size
    info[2..4].copy_from_slice(&65_535u16.to_be_bytes()); // maximum block size
    // Frame sizes unknown (0). Rate (20 bits), channels − 1 (3), bits − 1 (5),
    // total samples (36 bits, 0 = unknown), MD5 (zero).
    let packed = (u64::from(h.sample_rate) << 44) | (u64::from(h.channels - 1) << 41) | (u64::from(h.bits - 1) << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    Some(info)
}

/// Cuts native FLAC into frames: a frame ends where the next valid frame header
/// begins and the CRC-16 of the bytes before it — the frame with its footer — is 0.
struct FlacFramer {
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    skipped: u64,
    /// Bytes skipped since the last frame.
    run: u64,
}

/// Longest frame accepted before sync is searched anew.
const MAX_FLAC_FRAME: usize = 1 << 20;

impl FlacFramer {
    /// Make `n` bytes available from `pos` (false if the stream ends first). Never moves
    /// buffered bytes: [`Self::next_frame`] holds positions in the buffer.
    fn fill(&mut self, input: &mut dyn Read, n: usize) -> io::Result<bool> {
        while self.buf.len() - self.pos < n && !self.eof {
            let mut chunk = [0u8; 16 * 1024];
            let k = input.read(&mut chunk)?;
            if k == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..k]);
            }
        }
        Ok(self.buf.len() - self.pos >= n)
    }

    /// The header at `pos`, reading more as needed.
    fn header_at(&mut self, input: &mut dyn Read, at: usize) -> io::Result<Option<FlacFrameHeader>> {
        let mut need = 2;
        loop {
            let avail = self.fill(input, at - self.pos + need)?;
            match flac_frame_header(&self.buf[at.min(self.buf.len())..]) {
                Ok(h) => return Ok(h),
                Err(()) if avail && need < 32 => need += 4,
                Err(()) => return Ok(None),
            }
        }
    }

    /// Skip to the next possible sync byte; an `InvalidData` error after searching
    /// [`MAX_SEARCH_BYTES`] in a row.
    fn skip(&mut self) -> io::Result<()> {
        let next = self.buf[self.pos + 1..].iter().position(|&b| b == 0xFF).map_or(self.buf.len(), |i| self.pos + 1 + i);
        self.skipped += (next - self.pos) as u64;
        self.run += (next - self.pos) as u64;
        self.pos = next;
        if self.run > MAX_SEARCH_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("no frames found in {} kB of data", self.run / 1024)));
        }
        Ok(())
    }

    /// The next frame and its header; `None` at the end of the stream.
    fn next_frame(&mut self, input: &mut dyn Read) -> io::Result<Option<(FlacFrameHeader, Vec<u8>)>> {
        if self.pos > 256 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        'sync: loop {
            if !self.fill(input, 2)? {
                return Ok(None);
            }
            let Some(h) = self.header_at(input, self.pos)? else {
                self.skip()?;
                continue;
            };
            let mut crc = 0u16;
            let mut p = self.pos;
            loop {
                if p >= self.buf.len() && !self.fill(input, p - self.pos + 1)? {
                    // The end of the stream: the rest is the last frame if it checks.
                    if crc == 0 && p > self.pos + h.len {
                        let frame = self.buf[self.pos..p].to_vec();
                        self.pos = p;
                        return Ok(Some((h, frame)));
                    }
                    return Ok(None);
                }
                crc = crc16_update(crc, self.buf[p]);
                p += 1;
                if crc == 0
                    && p > self.pos + h.len + 1
                    && self.fill(input, p - self.pos + 2)?
                    && self.buf[p] == 0xFF
                    && let Some(next) = self.header_at(input, p)?
                    && next.channels == h.channels
                {
                    let frame = self.buf[self.pos..p].to_vec();
                    self.pos = p;
                    self.run = 0;
                    return Ok(Some((h, frame)));
                }
                if p - self.pos > MAX_FLAC_FRAME {
                    // A false header: search on.
                    self.skip()?;
                    continue 'sync;
                }
            }
        }
    }
}

/// Native FLAC (`fLaC`, metadata blocks, frames) through symphonia's FLAC decoder.
struct FlacStreamDecoder {
    framer: FlacFramer,
    dec: Option<(FlacDecoder, u32, usize)>,
    header_done: bool,
    title: Option<String>,
    errors: ErrorCount,
}

impl FlacStreamDecoder {
    fn new() -> Self {
        FlacStreamDecoder {
            framer: FlacFramer { buf: Vec::new(), pos: 0, eof: false, skipped: 0, run: 0 },
            dec: None,
            header_done: false,
            title: None,
            errors: ErrorCount::default(),
        }
    }

    /// Read `fLaC` and the metadata blocks, if the stream starts with them.
    fn read_header(&mut self, input: &mut dyn Read) -> Result<(), DecodeError> {
        self.header_done = true;
        let f = &mut self.framer;
        if !f.fill(input, 4)? || &f.buf[f.pos..f.pos + 4] != b"fLaC" {
            return Ok(());
        }
        f.pos += 4;
        loop {
            if !f.fill(input, 4)? {
                return Ok(());
            }
            let h = &f.buf[f.pos..f.pos + 4];
            let (last, kind) = (h[0] & 0x80 != 0, h[0] & 0x7F);
            let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
            if !f.fill(input, 4 + len)? {
                return Ok(());
            }
            let body = f.buf[f.pos + 4..f.pos + 4 + len].to_vec();
            f.pos += 4 + len;
            match kind {
                0 => self.dec = Some(flac_decoder(&body).map_err(DecodeError::Fatal)?),
                4 => self.title = ogg::title_of(&ogg::comments(&body)),
                _ => {}
            }
            if last {
                return Ok(());
            }
        }
    }
}

impl StreamDecoder for FlacStreamDecoder {
    fn next(&mut self, input: &mut dyn Read) -> Result<Option<Decoded>, DecodeError> {
        if !self.header_done {
            self.read_header(input)?;
        }
        loop {
            let Some((h, frame)) = self.framer.next_frame(input).map_err(framing("FLAC"))? else { return Ok(None) };
            if self.dec.is_none() {
                let info = synthetic_streaminfo(&h).ok_or_else(|| {
                    DecodeError::Fatal("FLAC stream without its header, and the frames do not say the sample rate".into())
                })?;
                self.dec = Some(flac_decoder(&info).map_err(DecodeError::Fatal)?);
            }
            let (dec, rate, channels) = self.dec.as_mut().expect("set above");
            let pk = PacketRef::new(0, Timestamp::new(0), Frames::new(u64::from(h.block_size)), &frame);
            match dec.decode_ref(&pk) {
                Ok(buf) => {
                    let mut samples = Vec::new();
                    buf.copy_to_vec_interleaved(&mut samples);
                    self.errors.ok();
                    return Ok(Some(Decoded { rate: *rate, channels: *channels, samples }));
                }
                Err(e) => self.errors.error(|| format!("FLAC: {e}"))?,
            }
        }
    }

    fn codec(&self) -> String {
        "FLAC".into()
    }

    fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    fn errors(&self) -> u64 {
        self.errors.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let mp3: Vec<u8> = {
            let h = MpegHeader::parse(&[0xFF, 0xFB, 0x90, 0x44]).unwrap();
            let mut f = vec![0xFF, 0xFB, 0x90, 0x44];
            f.resize(h.frame_len(), 0);
            [f.clone(), f.clone(), f].concat()
        };
        assert_eq!(classify(Some("audio/aac"), "/", &mp3), Class::Audio(Format::Mpeg), "the bytes win");
        assert_eq!(classify(Some("audio/mpeg"), "/", b"\x00\x01"), Class::Audio(Format::Mpeg), "else the label");
        assert_eq!(classify(Some("application/ogg"), "/", b"OggS\0\x02"), Class::Audio(Format::Ogg));
        assert_eq!(classify(None, "/", b"fLaC\0\0\0\x22"), Class::Audio(Format::Flac));
        assert_eq!(classify(Some("audio/x-scpls"), "/x", b"[playlist]\nFile1=http://a/\n"), Class::Playlist);
        assert_eq!(classify(Some("text/plain"), "/radio.m3u", b"stream.mp3\n"), Class::Playlist);
        assert!(matches!(classify(Some("text/html; charset=utf-8"), "/", b"<!DOCTYPE html>"), Class::Rejected(r) if r.contains("web page")));
        assert!(matches!(classify(Some("audio/mp4"), "/", b"\0\0\0\x20ftyp"), Class::Rejected(r) if r.contains("unsupported")));
        assert!(matches!(classify(Some("video/mp2t"), "/", b"G@"), Class::Rejected(r) if r.contains("video")));
        assert!(matches!(classify(None, "/", b"\x01\x02\x03"), Class::Rejected(r) if r.contains("unrecognised")));
    }

    #[test]
    fn flac_crcs_and_header() {
        // CRC-8 check value for "123456789" (poly 0x07): 0xF4; CRC-16/UMTS (0x8005): 0xFEE8.
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(b"123456789".iter().fold(0, |c, &b| crc16_update(c, b)), 0xFEE8);
        // Fixed block size 4096 (code 12), 44.1 kHz (code 9), stereo independent (1),
        // 16 bits (code 4), frame number 0.
        let mut h = vec![0xFF, 0xF8, 0xC9, 0x18, 0x00];
        h.push(crc8(&h));
        let parsed = flac_frame_header(&h).unwrap().unwrap();
        assert_eq!((parsed.len, parsed.block_size, parsed.sample_rate, parsed.channels, parsed.bits), (6, 4096, 44_100, 2, 16));
        h[5] ^= 1;
        assert_eq!(flac_frame_header(&h), Ok(None), "bad CRC");
        assert_eq!(flac_frame_header(&h[..3]), Err(()), "needs more bytes");
        let info = synthetic_streaminfo(&parsed).unwrap();
        assert_eq!((info[10], info[11], info[12] >> 4), (0x0A, 0xC4, 0x4), "44100 = 0x0AC44");
        assert_eq!((info[12] >> 1) & 7, 1, "two channels");
    }
}
