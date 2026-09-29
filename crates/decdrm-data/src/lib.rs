//! # decdrm-data — DRM data applications
//!
//! Decoding and encoding of the data services a DRM30 multiplex can carry, from the
//! MSC stream bytes of one service up to application objects:
//!
//! ```text
//!  stream bytes per 400 ms frame
//!        │  packet mode (ES 201 980 §6.6)          synchronous stream mode
//!        ▼                                          ▼
//!  packet::PacketDemux ── data units ──┐     DataEvent::StreamData
//!   (CRC, continuity, padding, ids)    ▼
//!                           MSC data groups (datagroup, EN 300 401 §5.3.3)
//!        ┌──────────────────────┬──────────────┴───────────────┐
//!        ▼                      ▼                              ▼
//!   mot::MotDecoder        journaline::JournalineDecoder   raw (TPEG, unknown)
//!   (EN 301 234)           (TS 102 979, NML)
//!   ├ SlideShow  → DataEvent::SlideShowImage   (slideshow::SlideShow model)
//!   ├ Website    → DataEvent::WebsiteFile/Index (website::Website model)
//!   └ EPG/SPI    → epg (TS 102 371 binary) → DataEvent::Epg (TS 102 818 XML)
//! ```
//!
//! [`DataDecoder`] wires this together for one service described by a
//! [`DataServiceConfig`] (from the SDC); [`DataEncoder`] is the exact inverse for the
//! transmitter, fed by application encoders ([`slideshow::SlideShowFeeder`],
//! [`mot::MotEncoder`] for websites and EPG, [`journaline::JournalineEncoder`],
//! [`RawSource`]).
//!
//! The design follows Dream (`src/datadecoding/*`, `util-QT/epgdec.cpp`), which this
//! crate ports freely (GPL-2.0-or-later); module docs name the Dream classes and list
//! where we deviate.
//!
//! ```
//! use decdrm_data::slideshow::SlideShowFeeder;
//! use decdrm_data::{DataDecoder, DataEncoder, DataEvent, DataServiceConfig, UserApplication};
//!
//! // SDC type 5: packet mode, packet id 0, packet length 60 (data field bytes).
//! let cfg = DataServiceConfig::packet(UserApplication::SlideShow, 0, 60);
//! let mut feeder = SlideShowFeeder::new();
//! feeder.add_image("hello.png", vec![0x89, b'P', b'N', b'G', 1, 2, 3]).unwrap();
//!
//! // Rust note: `Box::new(feeder)` moves the feeder to the heap so the encoder can own
//! // it as a `Box<dyn DataUnitSource + Send>` trait object.
//! let mut tx = DataEncoder::new(&cfg, Box::new(feeder)).unwrap();
//! let mut rx = DataDecoder::new(cfg);
//! let mut images = 0;
//! for _ in 0..4 {
//!     let frame = tx.next_frame(630); // this stream's bytes per multiplex frame
//!     for event in rx.push_frame(&frame) {
//!         if let DataEvent::SlideShowImage { name, .. } = event {
//!             assert_eq!(name, "hello.png");
//!             images += 1;
//!         }
//!     }
//! }
//! assert!(images > 0);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bits;
pub mod charset;
mod compress;
pub mod crc;
pub mod datagroup;
mod decoder;
pub mod encoder;
pub mod epg;
pub mod error;
pub mod journaline;
pub mod mot;
pub mod packet;
pub mod slideshow;
pub mod time;
pub mod website;

pub use decoder::{DataDecoder, DataEvent, DataStats};
pub use encoder::{DataEncoder, DataUnitSource, RawSource};
pub use error::DataError;
pub use journaline::JournalineUpdate;

use error::Result;

/// Application domain of SDC data entity type 5 (ES 201 980 §6.4.3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AppDomain {
    /// 0: DRM-specific application (identifier from TS 101 968).
    Drm,
    /// 1: DAB-specific application (user application type from TS 101 756).
    #[default]
    Dab,
    /// 2..=7: reserved.
    Other(u8),
}

impl AppDomain {
    /// From the SDC field value.
    pub fn from_sdc(value: u8) -> Self {
        match value {
            0 => Self::Drm,
            1 => Self::Dab,
            v => Self::Other(v),
        }
    }

    /// The SDC field value.
    pub fn sdc_value(self) -> u8 {
        match self {
            Self::Drm => 0,
            Self::Dab => 1,
            Self::Other(v) => v,
        }
    }
}

/// The data applications this crate knows, by user application identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserApplication {
    /// 0x002: MOT SlideShow (TS 101 499).
    SlideShow,
    /// 0x003: MOT Broadcast Website (TS 101 498).
    BroadcastWebsite,
    /// 0x004: TPEG (captured raw).
    Tpeg,
    /// 0x007: EPG / SPI (TS 102 818 / TS 102 371).
    Epg,
    /// 0x44A: Journaline (TS 102 979).
    Journaline,
    /// Anything else (captured raw).
    Other(u16),
}

impl UserApplication {
    /// Map a user application identifier. DRM broadcasts signal these applications
    /// with the DAB application domain; TS 101 968 lists the same numbers for the DRM
    /// domain, so both domains map identically (reserved domains map to `Other`).
    pub fn from_id(domain: AppDomain, id: u16) -> Self {
        if let AppDomain::Other(_) = domain {
            return Self::Other(id);
        }
        match id {
            0x002 => Self::SlideShow,
            0x003 => Self::BroadcastWebsite,
            0x004 => Self::Tpeg,
            0x007 => Self::Epg,
            0x44A => Self::Journaline,
            other => Self::Other(other),
        }
    }

    /// The user application identifier.
    pub fn id(self) -> u16 {
        match self {
            Self::SlideShow => 0x002,
            Self::BroadcastWebsite => 0x003,
            Self::Tpeg => 0x004,
            Self::Epg => 0x007,
            Self::Journaline => 0x44A,
            Self::Other(id) => id,
        }
    }

    /// `true` for applications carried in MOT.
    pub fn uses_mot(self) -> bool {
        matches!(self, Self::SlideShow | Self::BroadcastWebsite | Self::Epg)
    }
}

/// How one data service is carried, as signalled in the SDC (data entity type 5,
/// ES 201 980 §6.4.3.6; Dream `CDataParam`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct DataServiceConfig {
    /// Packet mode (`true`) or synchronous stream mode (`false`).
    pub packet_mode: bool,
    /// Packet mode: SDC "data unit indicator" — `true` if data units may span several
    /// packets (first/last flags), `false` for single-packet data units.
    pub data_unit_indicator: bool,
    /// Packet mode: packet id 0..=3 of this service.
    pub packet_id: u8,
    /// Packet mode: **total** packet length in bytes, i.e. the SDC type 5 "packet
    /// length" field (data field only) **plus 3** for header and CRC. Use
    /// [`DataServiceConfig::total_packet_len`] to convert.
    pub packet_len: usize,
    /// Application domain.
    pub app_domain: AppDomain,
    /// User application identifier.
    pub user_app_id: u16,
    /// Application-specific bytes from the SDC (not interpreted yet).
    pub app_data: Vec<u8>,
}

impl DataServiceConfig {
    /// Total packet length for an SDC type 5 "packet length" value.
    pub const fn total_packet_len(sdc_packet_length: u8) -> usize {
        sdc_packet_length as usize + packet::OVERHEAD
    }

    /// A packet-mode service with data units spanning packets, DAB application domain.
    pub fn packet(app: UserApplication, packet_id: u8, sdc_packet_length: u8) -> Self {
        Self {
            packet_mode: true,
            data_unit_indicator: true,
            packet_id,
            packet_len: Self::total_packet_len(sdc_packet_length),
            app_domain: AppDomain::Dab,
            user_app_id: app.id(),
            app_data: Vec::new(),
        }
    }

    /// A synchronous stream mode service.
    pub fn stream(app: UserApplication) -> Self {
        Self {
            packet_mode: false,
            user_app_id: app.id(),
            ..Self::default()
        }
    }

    /// The application, from domain and identifier.
    pub fn application(&self) -> UserApplication {
        UserApplication::from_id(self.app_domain, self.user_app_id)
    }

    /// Check the packet-mode parameters.
    pub fn validate(&self) -> Result<()> {
        if self.packet_mode {
            if self.packet_id > 3 {
                return Err(DataError::Config("packet id"));
            }
            if !(packet::MIN_PACKET_LEN..=packet::MAX_PACKET_LEN).contains(&self.packet_len) {
                return Err(DataError::Config("packet length"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_mapping() {
        assert_eq!(
            UserApplication::from_id(AppDomain::Dab, 0x44A),
            UserApplication::Journaline
        );
        assert_eq!(
            UserApplication::from_id(AppDomain::Drm, 2),
            UserApplication::SlideShow
        );
        assert_eq!(
            UserApplication::from_id(AppDomain::Other(3), 2),
            UserApplication::Other(2)
        );
        for app in [
            UserApplication::SlideShow,
            UserApplication::BroadcastWebsite,
            UserApplication::Tpeg,
            UserApplication::Epg,
            UserApplication::Journaline,
            UserApplication::Other(0x123),
        ] {
            assert_eq!(UserApplication::from_id(AppDomain::Dab, app.id()), app);
        }
        assert_eq!(
            AppDomain::from_sdc(AppDomain::Other(5).sdc_value()),
            AppDomain::Other(5)
        );
    }

    #[test]
    fn config_helpers() {
        let c = DataServiceConfig::packet(UserApplication::Epg, 2, 45);
        assert_eq!(c.packet_len, 48);
        assert!(c.validate().is_ok());
        assert!(
            DataServiceConfig {
                packet_id: 4,
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        assert!(DataServiceConfig { packet_len: 3, ..c }.validate().is_err());
        assert!(
            DataServiceConfig::stream(UserApplication::Tpeg)
                .validate()
                .is_ok()
        );
    }
}
