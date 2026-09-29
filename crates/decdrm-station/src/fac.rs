//! FAC service signalling: which service's parameters each frame's FAC carries.
//!
//! ES 201 980 §6.3.6: services of one kind are signalled in turn; mixtures of audio
//! and data services follow the repetition patterns of table 60, which favour the
//! audio services (port of Dream's `CFACTransmit::Init`).

use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::SpectrumOccupancy;

/// The Short Id sequence for audio services `a` and data services `d` (each in Short
/// Id order).
pub(crate) fn repetition_pattern(a: &[u8], d: &[u8]) -> Vec<u8> {
    match (a.len(), d.len()) {
        (_, 0) => a.to_vec(),
        (0, _) => d.to_vec(),
        // A1A1A1A1D1, A1A1A1A1D1A1A1A1A1D2, A1A1A1A1D1A1A1A1A1D2A1A1A1A1D3
        (1, _) => d.iter().flat_map(|&di| [a[0], a[0], a[0], a[0], di]).collect(),
        // A1A2A1A2D1, A1A2A1A2D1A1A2A1A2D2
        (2, _) => d.iter().flat_map(|&di| [a[0], a[1], a[0], a[1], di]).collect(),
        // A1A2A3A1A2A3D1
        _ => {
            let mut v: Vec<u8> = a.iter().chain(a.iter()).copied().collect();
            v.extend_from_slice(d);
            v
        }
    }
}

/// Produces the FAC of every frame, cycling through the services.
#[derive(Debug, Clone)]
pub(crate) struct FacScheduler {
    channel: ChannelParams,
    services: Vec<ServiceParams>,
    pattern: Vec<u8>,
    pos: usize,
}

impl FacScheduler {
    /// `services[short_id]` are the FAC service parameters. The channel fields the
    /// transmitter fills in itself (frame index, occupancy, modes) are placeholders.
    pub fn new(services: Vec<ServiceParams>) -> Self {
        let audio: Vec<u8> = services.iter().filter(|s| !s.is_data).map(|s| s.short_id).collect();
        let data: Vec<u8> = services.iter().filter(|s| s.is_data).map(|s| s.short_id).collect();
        let channel = ChannelParams {
            enhancement: false,
            frame_index: 0,
            // No AFS list is sent, so the AFS index is not valid.
            afs_valid: false,
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: audio.len() as u8,
            num_data: data.len() as u8,
            reconfiguration_index: 0,
            toggle: false,
        };
        Self { channel, pattern: repetition_pattern(&audio, &data), services, pos: 0 }
    }

    /// The FAC for the next frame.
    pub fn next_fac(&mut self) -> Fac {
        let id = self.pattern[self.pos % self.pattern.len()];
        self.pos = (self.pos + 1) % self.pattern.len();
        let service = self.services.iter().find(|s| s.short_id == id).copied().expect("pattern uses known ids");
        Fac { channel: self.channel, service }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_60_patterns() {
        assert_eq!(repetition_pattern(&[0, 1, 2], &[]), [0, 1, 2]);
        assert_eq!(repetition_pattern(&[], &[0, 1]), [0, 1]);
        assert_eq!(repetition_pattern(&[0], &[1]), [0, 0, 0, 0, 1]);
        assert_eq!(repetition_pattern(&[0], &[1, 2]), [0, 0, 0, 0, 1, 0, 0, 0, 0, 2]);
        assert_eq!(repetition_pattern(&[1], &[0, 2, 3]).len(), 15);
        assert_eq!(repetition_pattern(&[0, 2], &[1]), [0, 2, 0, 2, 1]);
        assert_eq!(repetition_pattern(&[0, 1], &[2, 3]), [0, 1, 0, 1, 2, 0, 1, 0, 1, 3]);
        assert_eq!(repetition_pattern(&[0, 1, 2], &[3]), [0, 1, 2, 0, 1, 2, 3]);
    }
}
