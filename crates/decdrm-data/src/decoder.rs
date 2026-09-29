//! Receiver side: the per-service [`DataDecoder`] and the [`DataEvent`]s it emits.
//!
//! Port of Dream's `CDataDecoder` (`DataDecoder.cpp`): packet demultiplexing, then
//! dispatch by user application to the MOT (SlideShow, Broadcast Website, EPG),
//! Journaline or raw handlers. Unlike Dream it also passes synchronous-stream services
//! through (as [`DataEvent::StreamData`]) instead of ignoring them.

use crate::epg;
use crate::journaline::{JournalineDecoder, JournalineUpdate};
use crate::mot::{MotDecoder, MotHeader, MotObject, MotOutput, content_type};
use crate::packet::{PacketDemux, PacketStats};
use crate::{DataServiceConfig, UserApplication};

/// Something a data service produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataEvent {
    /// A complete SlideShow image.
    SlideShowImage {
        /// MOT transport id.
        transport_id: u16,
        /// ContentName.
        name: String,
        /// MIME type (`image/jpeg`, `image/png`).
        mime: String,
        /// Image bytes.
        data: Vec<u8>,
        /// The full MOT header (TriggerTime, CategoryID/SlideID, ClickThroughURL...);
        /// feed the event to [`crate::slideshow::SlideShow`] for trigger handling.
        header: MotHeader,
    },
    /// A Broadcast Website file (gzip transport compression already removed).
    WebsiteFile {
        /// ContentName (a relative path such as `news/index.html`).
        path: String,
        /// MIME type.
        mime: String,
        /// File content.
        data: Vec<u8>,
    },
    /// The Broadcast Website start page changed (MOT directory DirectoryIndex).
    WebsiteIndex {
        /// Path of the start page.
        path: String,
    },
    /// A new or updated Journaline page.
    Journaline(JournalineUpdate),
    /// A decoded EPG object.
    Epg {
        /// ContentName (or Dream-style synthesized name).
        name: String,
        /// TS 102 818 XML.
        xml: String,
    },
    /// Any other MOT object (EPG logos, non-image SlideShow objects, ...).
    MotObject {
        /// MOT transport id.
        transport_id: u16,
        /// MOT header.
        header: MotHeader,
        /// Body as transmitted.
        body: Vec<u8>,
    },
    /// A data unit of an application we do not interpret (TPEG, unknown).
    Raw {
        /// User application identifier from the SDC.
        user_app_id: u16,
        /// The complete data unit (normally an MSC data group, CRC not checked).
        data_group: Vec<u8>,
    },
    /// One frame of a synchronous stream mode service.
    StreamData {
        /// User application identifier from the SDC.
        user_app_id: u16,
        /// The stream bytes of the frame.
        data: Vec<u8>,
    },
    /// Counters, emitted after every frame.
    Stats(DataStats),
}

/// Reception statistics of one data service (cumulative unless noted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DataStats {
    /// Frames pushed.
    pub frames: u64,
    /// Frames the caller flagged as bad (`crc_ok_hint == false`).
    pub frames_flagged_bad: u64,
    /// Packets with a good CRC (all packet ids of the stream).
    pub packets_ok: u64,
    /// Packets with a bad CRC.
    pub packets_crc_error: u64,
    /// Padding packets on this service's packet id.
    pub padding_packets: u64,
    /// Continuity index jumps on this service's packet id.
    pub continuity_errors: u64,
    /// Complete data units on this service's packet id.
    pub data_units: u64,
    /// Data units lost on this service's packet id.
    pub data_units_dropped: u64,
    /// Data-group level CRC or format errors (MOT, Journaline).
    pub data_group_errors: u64,
    /// Application objects delivered (images, files, pages, raw units).
    pub objects: u64,
    /// Good packets in the last frame.
    pub last_frame_packets_ok: u32,
    /// Bad packets in the last frame.
    pub last_frame_packets_bad: u32,
}

#[derive(Debug)]
enum Handler {
    // Boxed: the MOT decoder is much larger than the other variants.
    Mot(Box<MotDecoder>),
    Journaline(JournalineDecoder),
    Raw,
}

/// Decoder for one data service: feed it the service's stream bytes every multiplex
/// frame, get [`DataEvent`]s back.
///
/// Several services on the same packet-mode stream (different packet ids) each get
/// their own decoder fed with the same bytes; each one only interprets its own packet
/// id (CRC counters therefore cover the whole stream).
#[derive(Debug)]
pub struct DataDecoder {
    cfg: DataServiceConfig,
    app: UserApplication,
    demux: Option<PacketDemux>,
    handler: Handler,
    stats: DataStats,
    last_index: Option<String>,
}

impl DataDecoder {
    /// Create a decoder for the service described by `cfg`.
    ///
    /// An invalid packet length (e.g. from a corrupted SDC) does not panic: the decoder
    /// then only counts frames, like Dream's `DoNotProcessData`.
    pub fn new(cfg: DataServiceConfig) -> Self {
        let app = cfg.application();
        let demux = if cfg.packet_mode {
            PacketDemux::new(cfg.packet_len).ok()
        } else {
            None
        };
        let handler = match app {
            UserApplication::SlideShow
            | UserApplication::BroadcastWebsite
            | UserApplication::Epg => Handler::Mot(Box::default()),
            // Dream: "No extended header will be used" (SDC application data ignored).
            UserApplication::Journaline => Handler::Journaline(JournalineDecoder::new(0)),
            UserApplication::Tpeg | UserApplication::Other(_) => Handler::Raw,
        };
        Self {
            cfg,
            app,
            demux,
            handler,
            stats: DataStats::default(),
            last_index: None,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &DataServiceConfig {
        &self.cfg
    }

    /// The application this decoder dispatches to.
    pub fn application(&self) -> UserApplication {
        self.app
    }

    /// Cumulative counters.
    pub fn stats(&self) -> &DataStats {
        &self.stats
    }

    /// Packet-layer counters for the whole stream (packet mode only).
    pub fn packet_stats(&self) -> Option<&PacketStats> {
        self.demux.as_ref().map(|d| d.stats())
    }

    /// The MOT decoder (SlideShow, Broadcast Website and EPG services), e.g. to look
    /// at the current MOT directory.
    pub fn mot(&self) -> Option<&MotDecoder> {
        match &self.handler {
            Handler::Mot(m) => Some(m),
            _ => None,
        }
    }

    /// Drop partially received data (e.g. after a loss of synchronisation). Counters
    /// and application caches are kept.
    pub fn reset(&mut self) {
        if let Some(d) = &mut self.demux {
            d.reset();
        }
        if let Handler::Mot(m) = &mut self.handler {
            m.reset();
        }
        self.last_index = None;
    }

    /// Feed the stream bytes of one multiplex frame.
    pub fn push_frame(&mut self, stream: &[u8]) -> Vec<DataEvent> {
        self.push_frame_with_hint(stream, true)
    }

    /// Feed one frame together with the channel decoder's opinion of it.
    ///
    /// In packet mode every packet carries its own CRC, so the hint is only counted; in
    /// synchronous stream mode a frame flagged bad is not passed on.
    pub fn push_frame_with_hint(&mut self, stream: &[u8], crc_ok_hint: bool) -> Vec<DataEvent> {
        let mut events = Vec::new();
        self.stats.frames += 1;
        if !crc_ok_hint {
            self.stats.frames_flagged_bad += 1;
        }
        if !self.cfg.packet_mode {
            if crc_ok_hint && !stream.is_empty() {
                self.stats.objects += 1;
                events.push(DataEvent::StreamData {
                    user_app_id: self.cfg.user_app_id,
                    data: stream.to_vec(),
                });
            }
        } else if let Some(demux) = &mut self.demux {
            let (ok0, bad0) = (demux.stats().packets_ok, demux.stats().packets_crc_error);
            let units = demux.push_frame(stream);
            let ps = demux.stats();
            self.stats.last_frame_packets_ok = (ps.packets_ok - ok0) as u32;
            self.stats.last_frame_packets_bad = (ps.packets_crc_error - bad0) as u32;
            let own = self.cfg.packet_id;
            for unit in units.into_iter().filter(|u| u.packet_id == own) {
                events.extend(self.handle_unit(&unit.data));
            }
        }
        self.refresh_stats();
        events.push(DataEvent::Stats(self.stats.clone()));
        events
    }

    /// Feed one complete data unit directly, bypassing the packet layer (useful for
    /// tests and for data units obtained elsewhere, e.g. recorded files).
    pub fn push_data_unit(&mut self, unit: &[u8]) -> Vec<DataEvent> {
        let events = self.handle_unit(unit);
        self.refresh_stats();
        events
    }

    fn refresh_stats(&mut self) {
        if let Some(demux) = &self.demux {
            let ps = demux.stats();
            let own = &ps.per_id[usize::from(self.cfg.packet_id & 3)];
            self.stats.packets_ok = ps.packets_ok;
            self.stats.packets_crc_error = ps.packets_crc_error + ps.packets_malformed;
            self.stats.padding_packets = own.padding_packets;
            self.stats.continuity_errors = own.continuity_errors;
            self.stats.data_units = own.data_units;
            self.stats.data_units_dropped = own.data_units_dropped;
        }
        self.stats.data_group_errors = match &self.handler {
            Handler::Mot(m) => m.stats().crc_errors + m.stats().malformed,
            Handler::Journaline(j) => j.stats().crc_errors + j.stats().malformed,
            Handler::Raw => 0,
        };
    }

    fn handle_unit(&mut self, unit: &[u8]) -> Vec<DataEvent> {
        match &mut self.handler {
            Handler::Raw => {
                self.stats.objects += 1;
                vec![DataEvent::Raw {
                    user_app_id: self.cfg.user_app_id,
                    data_group: unit.to_vec(),
                }]
            }
            Handler::Journaline(dec) => match dec.push_data_unit(unit) {
                Ok(Some(update)) => {
                    self.stats.objects += 1;
                    vec![DataEvent::Journaline(update)]
                }
                Ok(None) | Err(_) => Vec::new(),
            },
            Handler::Mot(dec) => {
                let outputs = dec.push_data_unit(unit);
                let mut events = Vec::new();
                for out in outputs {
                    match out {
                        MotOutput::Directory => {
                            let index = self
                                .mot()
                                .and_then(|m| m.directory())
                                .and_then(|d| d.best_index());
                            if self.app == UserApplication::BroadcastWebsite
                                && index.is_some()
                                && index != self.last_index
                            {
                                self.last_index = index.clone();
                                events.push(DataEvent::WebsiteIndex {
                                    path: index.unwrap_or_default(),
                                });
                            }
                        }
                        MotOutput::Object(obj) => {
                            self.stats.objects += 1;
                            events.push(self.object_event(obj));
                        }
                    }
                }
                events
            }
        }
    }

    fn object_event(&self, obj: MotObject) -> DataEvent {
        match self.app {
            UserApplication::SlideShow if obj.header.content_type == content_type::IMAGE => {
                DataEvent::SlideShowImage {
                    transport_id: obj.transport_id,
                    name: obj.name(),
                    mime: obj.mime(),
                    data: obj.body,
                    header: obj.header,
                }
            }
            UserApplication::BroadcastWebsite => {
                let (mut path, mime) = (obj.name(), obj.mime());
                let data = match obj.decompressed_body() {
                    Ok(d) => d,
                    Err(_) => {
                        // Dream: "Can't unzip so change the filename".
                        path.push_str(".gz");
                        obj.body
                    }
                };
                DataEvent::WebsiteFile { path, mime, data }
            }
            UserApplication::Epg if obj.header.content_type == content_type::EPG => {
                match obj.decompressed_body().and_then(|b| epg::decode_to_xml(&b)) {
                    Ok(xml) => DataEvent::Epg {
                        name: epg::object_name(&obj.header),
                        xml,
                    },
                    Err(_) => DataEvent::MotObject {
                        transport_id: obj.transport_id,
                        header: obj.header,
                        body: obj.body,
                    },
                }
            }
            _ => DataEvent::MotObject {
                transport_id: obj.transport_id,
                header: obj.header,
                body: obj.body,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppDomain;
    use crate::datagroup::{DataGroup, group_type};

    fn cfg(app: UserApplication) -> DataServiceConfig {
        DataServiceConfig::packet(app, 1, 40)
    }

    #[test]
    fn bad_packet_length_does_not_panic() {
        let mut c = cfg(UserApplication::SlideShow);
        c.packet_len = 2;
        let mut dec = DataDecoder::new(c);
        let ev = dec.push_frame(&[0; 100]);
        assert_eq!(ev.len(), 1);
        assert_eq!(dec.stats().frames, 1);
    }

    #[test]
    fn stream_mode_passes_frames_through() {
        let c = DataServiceConfig {
            packet_mode: false,
            user_app_id: 0x123,
            ..DataServiceConfig::default()
        };
        let mut dec = DataDecoder::new(c);
        let ev = dec.push_frame(&[1, 2, 3]);
        assert_eq!(
            ev[0],
            DataEvent::StreamData {
                user_app_id: 0x123,
                data: vec![1, 2, 3]
            }
        );
        let ev = dec.push_frame_with_hint(&[4, 5], false);
        assert!(matches!(ev.as_slice(), [DataEvent::Stats(s)] if s.frames_flagged_bad == 1));
    }

    #[test]
    fn raw_applications_get_data_units() {
        let mut c = cfg(UserApplication::Tpeg);
        c.app_domain = AppDomain::Dab;
        let mut dec = DataDecoder::new(c);
        let unit = DataGroup::new(group_type::GENERAL_DATA, b"tpeg frame".to_vec()).to_bytes();
        let ev = dec.push_data_unit(&unit);
        assert_eq!(
            ev,
            vec![DataEvent::Raw {
                user_app_id: 4,
                data_group: unit
            }]
        );
    }
}
