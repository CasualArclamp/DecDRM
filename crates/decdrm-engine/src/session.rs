//! A receiving session without threads or devices: samples in, decoded things out.
//! The engine runs one inside its worker thread; tests and batch tools can use it
//! directly.

use decdrm_core::fac::{Fac, LANGUAGES, PROGRAMME_TYPES};
use decdrm_core::rx::{MscFrame, Receiver, ReceiverConfig, ReceiverEvent, RxStatus, SdcBlock, Visuals};

/// Output of a session step.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Something worth a log line.
    Log(String),
    Fac(Fac),
    Sdc(SdcBlock),
    Msc(MscFrame),
}

/// Receiver plus (soon) the service decoding pipelines.
pub struct Session {
    rx: Receiver,
    last_fac: Option<Fac>,
    samples_in: u64,
}

impl Session {
    pub fn new(cfg: ReceiverConfig) -> Self {
        Self { rx: Receiver::new(cfg), last_fac: None, samples_in: 0 }
    }

    pub fn status(&self) -> &RxStatus {
        self.rx.status()
    }

    pub fn visuals(&self) -> Visuals {
        self.rx.visuals()
    }

    pub fn restart(&mut self) {
        self.rx.restart();
        self.last_fac = None;
    }

    /// Seconds of 48 kHz input processed.
    pub fn time_s(&self) -> f64 {
        self.samples_in as f64 / 48_000.0
    }

    /// Feed interleaved 48 kHz frames.
    pub fn push(&mut self, frames: &[f32]) -> Vec<SessionEvent> {
        let ch = self.rx.config().channels.max(1);
        self.samples_in += (frames.len() / ch) as u64;
        let t = self.time_s();
        let mut out = Vec::new();
        for ev in self.rx.push(frames) {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, inverted } => out.push(SessionEvent::Log(format!(
                    "{t:7.2}s signal found at {dc_hz:.1} Hz{}",
                    if inverted { " (inverted spectrum)" } else { "" }
                ))),
                ReceiverEvent::ModeDetected(m) => out.push(SessionEvent::Log(format!("{t:7.2}s robustness mode {m}"))),
                ReceiverEvent::Restarted => {
                    self.last_fac = None;
                    out.push(SessionEvent::Log(format!("{t:7.2}s synchronisation lost, restarting")));
                }
                ReceiverEvent::Fac(fac) => {
                    let changed = self.last_fac.is_none_or(|f| {
                        f.channel.occupancy != fac.channel.occupancy
                            || f.channel.msc_mode != fac.channel.msc_mode
                            || f.channel.sdc_mode != fac.channel.sdc_mode
                            || f.channel.interleaving != fac.channel.interleaving
                    });
                    if changed {
                        let c = &fac.channel;
                        out.push(SessionEvent::Log(format!(
                            "{t:7.2}s {} · MSC {:?} · SDC {:?} · {:?} interleaving · {} audio / {} data services",
                            c.occupancy, c.msc_mode, c.sdc_mode, c.interleaving, c.num_audio, c.num_data
                        )));
                    }
                    self.last_fac = Some(fac);
                    out.push(SessionEvent::Fac(fac));
                }
                ReceiverEvent::FacError => {}
                ReceiverEvent::Sdc(b) => out.push(SessionEvent::Sdc(b)),
                ReceiverEvent::Msc(m) => out.push(SessionEvent::Msc(m)),
            }
        }
        out
    }
}

/// Short text for a FAC service entry (used until the SDC label is known).
pub fn describe_fac_service(fac: &Fac) -> String {
    let s = &fac.service;
    let lang = LANGUAGES.get(s.language as usize).copied().unwrap_or("?");
    if s.is_data {
        format!("data service, app {:#x}, {lang}", s.descriptor)
    } else {
        let pty = PROGRAMME_TYPES.get(s.descriptor as usize).copied().unwrap_or("?");
        format!("audio, {pty}, {lang}")
    }
}
