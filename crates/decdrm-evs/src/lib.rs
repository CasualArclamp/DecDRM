//! 3GPP EVS (Enhanced Voice Services) audio sent as DRM data.
//!
//! EVS is not a DRM audio coding, but Korean Central Broadcasting (KCBS, 6140 kHz)
//! sends EVS at 13.2 kbit/s inside a data service ([`kcbs`] describes the framing).
//! This crate recognises those frames and cuts them out ([`kcbs`], [`signalling`]), so
//! the receiver can show the service as EVS audio.
//!
//! DecDRM does not decode EVS. The decoder at hand, the 3GPP reference code, is
//! © 3GPP Organizational Partners, and EVS is a patent-licensed codec. An optional
//! build of it was removed on 2026-10-01 so that DecDRM's repository can be public:
//! no 3GPP code goes into DecDRM.

pub mod kcbs;
pub mod signalling;
