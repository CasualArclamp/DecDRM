//! KiwiSDR client for DecDRM: tunes a remote KiwiSDR (kiwisdr.com) to a DRM frequency and
//! streams its I/Q (12 kHz, or 20.25 kHz on wideband Kiwis) into the receiver, in place
//! of a virtual audio cable and a browser tab.
//!
//! * [`address`] — what people paste (`host`, `host:port`, a Kiwi URL with `?f=`).
//! * [`protocol`] — the Kiwi's WebSocket messages, after the reference client
//!   `kiwiclient` (github.com/jks-prv/kiwiclient).
//! * [`client`] — [`KiwiStream`]: the connection thread, its I/Q FIFO and status.
//! * [`directory`] — the public list of KiwiSDRs (which allow apps, how busy, where).
//! * [`mock`] — a stand-in KiwiSDR on 127.0.0.1 for tests.
//!
//! Courtesy: a public Kiwi has few channels. DecDRM connects only when asked to, shows
//! its name in the Kiwi's user list, does not retry a Kiwi that is busy or refuses it,
//! and does not reconnect after the Kiwi closes the connection (its time limit).

pub mod address;
pub mod client;
pub mod directory;
pub mod mock;
pub mod protocol;

pub use address::{AddressError, DEFAULT_PORT, KiwiAddress, frequency_from_url};
pub use client::{KiwiConfig, KiwiError, KiwiState, KiwiStatus, KiwiStream, RETUNE_SETTLE};
pub use directory::{DIRECTORY_URL, KiwiEntry, parse_directory};
pub use protocol::Agc;
