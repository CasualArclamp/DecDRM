//! Journaline (ETSI TS 102 979): hierarchical text news pages in NML format.
//!
//! * [`NmlObject`] — parse/encode one page (menu, plain text, title, list).
//! * [`JournalineDecoder`] — data groups in, new/changed pages out.
//! * [`JournalineBrowser`] — page cache with root-menu navigation for a GUI.
//! * [`JournalineEncoder`] — transmitter carousel.
//!
//! Journaline® is a registered trademark of Fraunhofer IIS; this is an independent
//! implementation modelled on the GPL decoder that ships with Dream.

mod browser;
mod decoder;
mod encoder;
mod nml;

pub use browser::{DEFAULT_CAPACITY, JournalineBrowser, MenuEntry};
pub use decoder::{JournalineDecoder, JournalineStats, JournalineUpdate, ObjectStatus};
pub use encoder::{JournalineEncoder, PageChanges};
pub use nml::{ListItem, MenuItem, NML_MAX_LEN, NmlBody, NmlObject, NmlObjectType, ROOT_OBJECT_ID};
