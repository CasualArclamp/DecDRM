//! Multiplex layer (ES 201 980 §5.2–§6.5): everything between the decoded FAC/SDC/MSC
//! bits and the audio/data decoders.
//!
//! * [`sdc`] — SDC data entities: parsing ([`sdc::parse_sdc`]) and encoding
//!   ([`sdc::encode_sdc_data`]) of every DRM30 entity type.
//! * [`service`] — [`service::Ensemble`], the accumulated picture of the tuned
//!   multiplex built from FAC and SDC (services, labels, audio/data parameters,
//!   multiplex description, time, alternative frequencies), with reconfiguration
//!   handling and the [`crate::rx::MscConfig`] the receiver needs.
//! * [`msc`] — MSC demultiplexing of a decoded [`crate::rx::MscFrame`] into the
//!   logical frames of each stream, and the inverse multiplexer.
//! * [`audio`] — audio super frames: AAC (and Dream's Opus framing) parser/builder and
//!   the stateful xHE-AAC deframer/framer.
//! * [`text`] — text messages carried in the last four bytes of an audio stream.
//!
//! Typical receiver use:
//!
//! ```ignore
//! let mut ens = Ensemble::new();
//! match event {
//!     ReceiverEvent::Fac(fac) => { ens.update_fac(&fac); }
//!     ReceiverEvent::Sdc(block) if block.crc_ok => { ens.update_sdc(&block.data); }
//!     ReceiverEvent::Msc(frame) => {
//!         let streams = msc::demultiplex(&frame, ens.multiplex().unwrap());
//!         // hand the audio stream to an audio::AudioDeframer, data streams to decdrm-data
//!     }
//!     _ => {}
//! }
//! receiver.set_msc_config(ens.msc_config());
//! ```

pub mod audio;
pub mod msc;
pub mod sdc;
pub mod service;
pub mod text;

#[cfg(test)]
mod tests;

pub use audio::{AudioDeframer, AudioFrame, AudioSuperFrame};
pub use msc::{LogicalFrame, demultiplex, multiplex};
pub use sdc::{EntityBody, SdcEntity, encode_sdc_data, parse_sdc};
pub use service::{AudioCodec, AudioMode, AudioParams, Ensemble, ServiceInfo};
pub use text::{TextMessageDecoder, TextMessageEncoder};
