//! # decdrm-mdi — MDI, RSCI and RCI over DCP
//!
//! DRM's distribution interfaces carry a multiplex as packets over IP or in files:
//!
//! * **DCP** (ETSI TS 102 821), the transport: an [`af`] packet (sync, length,
//!   sequence number, CRC) carries a TAG packet; the optional [`pft`] layer splits AF
//!   packets into fragments for the network and may protect them with Reed–Solomon
//!   ([`rs`]) so that lost fragments can be rebuilt.
//! * **TAG items** ([`tag`]): a 4-character name, a length in bits and the value.
//! * **MDI** (ETSI TS 102 820): what a content server sends a modulator for each 400 ms
//!   logical frame — FAC, SDC (once per super frame), the MSC streams, the robustness
//!   mode and the stream layout ([`mdi::MdiFrame`]).
//! * **RSCI** (ETSI TS 102 349): a receiver's output — the MDI items of what it
//!   decoded plus its status: signal level, sync and CRC status, MER/WMER, Doppler,
//!   delay, spectrum, impulse response, GPS … ([`rsci::RsciStatus`]); and **RCI**, the
//!   commands that control such a receiver (frequency, mode, …, [`rci`]).
//!
//! Sources: UDP (unicast or multicast, [`net`]) and recordings ([`file`]: raw AF or
//! PFT packets, the RSCI file framing Dream writes as `.rsA`…, and pcap/pcapng
//! captures). [`source::DcpReceiver`] puts it together: packets in, [`mdi::MdiFrame`]s
//! out.
//!
//! Cross-checked with Dream r1548 (`src/MDI/`), which reads RSCI and MDI but has no
//! PFT error correction.

#![forbid(unsafe_code)]

pub mod af;
pub mod file;
pub mod mdi;
pub mod net;
pub mod pft;
pub mod rci;
pub mod rs;
pub mod rsci;
pub mod source;
pub mod tag;

pub use af::AfPacket;
pub use mdi::{MdiFrame, Protocol, Sdci};
pub use pft::{PftFragment, PftReassembler};
pub use rci::RciCommand;
pub use rsci::RsciStatus;
pub use source::{DcpReceiver, DcpStats};
pub use tag::TagItem;

/// What can go wrong reading DCP packets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DcpError {
    /// The data ended before the packet did.
    #[error("packet truncated")]
    Truncated,
    /// The packet does not start with the expected sync bytes ("AF", "PF").
    #[error("no {0} sync")]
    Sync(&'static str),
    /// A header or packet CRC failed.
    #[error("{0} CRC error")]
    Crc(&'static str),
    /// A field has a value the standard does not allow.
    #[error("invalid {0}")]
    Invalid(&'static str),
}
