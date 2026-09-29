//! Transmitter side: data-unit sources and the top-level [`DataEncoder`], which turns
//! them into the MSC stream bytes of each multiplex frame.
//!
//! Application encoders ([`crate::mot::MotEncoder`], [`crate::slideshow::SlideShowFeeder`],
//! [`crate::journaline::JournalineEncoder`], [`RawSource`]) all implement
//! [`DataUnitSource`]; [`DataEncoder`] packetises their data units (packet mode) or
//! streams their bytes (synchronous stream mode).

use crate::DataServiceConfig;
use crate::datagroup::{DataGroup, group_type};
use crate::error::{DataError, Result};
use crate::packet::PacketMux;
use std::collections::VecDeque;

/// Anything that can supply data units (normally complete MSC data groups).
///
/// Rust note: a *trait* is an interface. Implementing it for a type lets that type be
/// plugged into [`DataEncoder`] / [`PacketMux`], which only see a
/// `Box<dyn DataUnitSource + Send>` and never the concrete encoder type.
pub trait DataUnitSource {
    /// The next data unit, or `None` if nothing is waiting (a carousel never runs dry,
    /// a queue may).
    fn next_data_unit(&mut self) -> Option<Vec<u8>>;
}

impl DataUnitSource for VecDeque<Vec<u8>> {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        self.pop_front()
    }
}

/// Adapter that turns a closure into a [`DataUnitSource`] (see [`from_fn`]).
pub struct FromFn<F>(F);

impl<F: FnMut() -> Option<Vec<u8>>> DataUnitSource for FromFn<F> {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        (self.0)()
    }
}

/// Wrap a closure as a [`DataUnitSource`].
///
/// Rust note: `impl FnMut() -> ...` accepts any closure that may mutate captured state.
pub fn from_fn<F: FnMut() -> Option<Vec<u8>>>(f: F) -> FromFn<F> {
    FromFn(f)
}

/// A FIFO of data units for applications we do not interpret (TPEG, experimental,
/// raw capture replay), optionally cycling like a carousel.
#[derive(Debug, Clone, Default)]
pub struct RawSource {
    queue: VecDeque<Vec<u8>>,
    repeat: bool,
    continuity: u8,
}

impl RawSource {
    /// Empty one-shot queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// When `true`, every unit handed out is appended again (carousel).
    pub fn set_repeat(&mut self, repeat: bool) {
        self.repeat = repeat;
    }

    /// Queue a complete data unit verbatim.
    pub fn push_data_unit(&mut self, unit: Vec<u8>) {
        self.queue.push_back(unit);
    }

    /// Queue `data` wrapped in a "general data" MSC data group with CRC.
    pub fn push_general_data(&mut self, data: Vec<u8>) {
        let mut dg = DataGroup::new(group_type::GENERAL_DATA, data);
        dg.continuity = self.continuity;
        self.continuity = (self.continuity + 1) & 0x0F;
        self.queue.push_back(dg.to_bytes());
    }

    /// Units waiting.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// `true` if nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl DataUnitSource for RawSource {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        let unit = self.queue.pop_front()?;
        if self.repeat {
            self.queue.push_back(unit.clone());
        }
        Some(unit)
    }
}

enum Inner {
    Packet(PacketMux),
    Stream {
        source: Box<dyn DataUnitSource + Send>,
        pending: VecDeque<u8>,
    },
}

/// Produces the MSC stream bytes of one data service, frame by frame — the inverse of
/// [`crate::DataDecoder`].
pub struct DataEncoder {
    inner: Inner,
}

impl std::fmt::Debug for DataEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            Inner::Packet(mux) => f.debug_tuple("DataEncoder").field(mux).finish(),
            Inner::Stream { pending, .. } => f
                .debug_struct("DataEncoder")
                .field("stream_pending_bytes", &pending.len())
                .finish_non_exhaustive(),
        }
    }
}

impl DataEncoder {
    /// Encoder for the service described by `cfg`, fed from `source`.
    ///
    /// In packet mode `cfg.packet_len` (total length) and `cfg.packet_id` are used;
    /// more packet ids can be multiplexed into the same stream with
    /// [`Self::add_packet_channel`].
    pub fn new(cfg: &DataServiceConfig, source: Box<dyn DataUnitSource + Send>) -> Result<Self> {
        let inner = if cfg.packet_mode {
            cfg.validate()?;
            let mut mux = PacketMux::new(cfg.packet_len)?;
            mux.add_channel(cfg.packet_id, cfg.data_unit_indicator, source)?;
            Inner::Packet(mux)
        } else {
            Inner::Stream {
                source,
                pending: VecDeque::new(),
            }
        };
        Ok(Self { inner })
    }

    /// Packet mode: add another service on its own packet id to the same stream.
    pub fn add_packet_channel(
        &mut self,
        packet_id: u8,
        data_unit_indicator: bool,
        source: Box<dyn DataUnitSource + Send>,
    ) -> Result<()> {
        match &mut self.inner {
            Inner::Packet(mux) => mux.add_channel(packet_id, data_unit_indicator, source),
            Inner::Stream { .. } => {
                Err(DataError::Config("packet channel on a synchronous stream"))
            }
        }
    }

    /// The stream bytes for one multiplex frame (`stream_len` = the stream's length in
    /// bytes per frame, from the SDC multiplex description).
    pub fn next_frame(&mut self, stream_len: usize) -> Vec<u8> {
        match &mut self.inner {
            Inner::Packet(mux) => mux.next_frame(stream_len),
            Inner::Stream { source, pending } => {
                while pending.len() < stream_len {
                    match source.next_data_unit() {
                        Some(unit) => pending.extend(unit),
                        None => break,
                    }
                }
                let take = stream_len.min(pending.len());
                let mut out: Vec<u8> = pending.drain(..take).collect();
                out.resize(stream_len, 0);
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_source_repeat_and_wrapping() {
        let mut src = RawSource::new();
        src.push_general_data(b"tpeg".to_vec());
        src.push_data_unit(vec![1, 2, 3]);
        src.set_repeat(true);
        let a = src.next_data_unit().unwrap();
        assert_eq!(DataGroup::parse(&a).unwrap().data, b"tpeg");
        assert_eq!(src.next_data_unit().unwrap(), vec![1, 2, 3]);
        assert_eq!(src.next_data_unit().unwrap(), a);
        assert_eq!(src.len(), 2);
    }

    #[test]
    fn stream_mode_frames() {
        let cfg = DataServiceConfig {
            packet_mode: false,
            ..DataServiceConfig::default()
        };
        let mut n = 0u8;
        let src = from_fn(move || {
            n += 1;
            (n <= 3).then(|| vec![n; 4])
        });
        let mut enc = DataEncoder::new(&cfg, Box::new(src)).unwrap();
        assert_eq!(enc.next_frame(6), vec![1, 1, 1, 1, 2, 2]);
        assert_eq!(enc.next_frame(6), vec![2, 2, 3, 3, 3, 3]);
        assert_eq!(enc.next_frame(3), vec![0, 0, 0]);
        assert!(
            enc.add_packet_channel(1, true, Box::new(RawSource::new()))
                .is_err()
        );
    }
}
