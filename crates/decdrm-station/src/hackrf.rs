//! HackRF One output: streams the transmitter signal to the radio over USB.
//!
//! The sink talks to the HackRF's USB protocol directly (the vendor requests of
//! libhackrf's `hackrf.c`, as the Python projects do), so neither libhackrf nor
//! its driver utilities are needed — only a WinUSB/libusb-compatible driver.
//!
//! The signal chain: the station's 48 kHz complex baseband (I/Q interleaved
//! `f32`, DC carrier at 0 Hz) is resampled to the HackRF's sample rate, shifted
//! to [`HackrfSettings::if_offset_hz`] (keeping the channel away from the
//! HackRF's DC spike and LO leakage, which would sit on a zero-IF signal's
//! centre), scaled to 8-bit signed I/Q and streamed to the bulk endpoint. The
//! local oscillator runs at `frequency − if_offset`, so the DRM channel appears
//! centred at `frequency` on air.
//!
//! Pacing: a dedicated writer thread keeps the bulk endpoint fed, and USB bulk
//! writes to a transmitting HackRF block once the radio's buffer is full — that
//! paces the station to real time exactly like a sound card does. The station
//! appends each frame to a queue and waits until it has drained back to
//! [`HackrfSettings::buffer_s`] of headroom. The radio's own buffer is tiny:
//! gaps between the station's frames (encoding, printing) would underrun it,
//! which older firmware does not reliably survive — hence the thread, and hence
//! the headroom the queue carries.

use crate::config::HackrfSettings;
use crate::error::{Result, StationError};
use crate::station::StopHandle;
use decdrm_io::{Resampler, ResamplerQuality};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// USB ids of every HackRF One.
const VENDOR_ID: u16 = 0x1d50;
const PRODUCT_ID: u16 = 0x6089;
/// The one USB interface and its bulk endpoints.
const INTERFACE: u8 = 0;
const ENDPOINT_TX: u8 = 0x02;

/// Vendor requests (USB control transfers), from libhackrf's `hackrf.c`.
const REQ_SET_TRANSCEIVER_MODE: u8 = 1;
const REQ_SAMPLE_RATE_SET: u8 = 6;
const REQ_BASEBAND_FILTER_BANDWIDTH_SET: u8 = 7;
const REQ_SET_FREQ: u8 = 16;
const REQ_AMP_ENABLE: u8 = 17;
const REQ_SET_TXVGA_GAIN: u8 = 21;

/// Transceiver modes.
const MODE_OFF: u16 = 0;
const MODE_TRANSMIT: u16 = 2;

/// One bulk transfer, bytes. Small blocks: synchronous libusb transfers of
/// libhackrf's 256 KB size crawl on some hosts (measured at a fifth of the
/// rate on Windows/WinUSB), while 16 KB keeps up with 2 Msps with ease.
const BLOCK_BYTES: usize = 16_384;

/// How long one bulk write may block (the radio accepts data at its sample
/// rate, so a healthy one frees room within milliseconds; a timeout means the
/// device went away).
const USB_TIMEOUT: Duration = Duration::from_secs(10);

/// How long any wait on the queue re-checks its condition (stop flags, errors).
const POLL: Duration = Duration::from_millis(50);

/// Settings that describe the radio on air, for [`crate::StationStatus`].
#[derive(Debug, Clone, PartialEq)]
pub struct HackrfStatus {
    /// Centre of the DRM channel on air, Hz.
    pub frequency_hz: u64,
    /// Local-oscillator frequency, Hz (the channel centre less the IF offset).
    pub lo_hz: u64,
    /// HackRF sample rate, Hz.
    pub sample_rate_hz: u32,
    /// The IF the signal was shifted to within the HackRF's passband, Hz.
    pub if_offset_hz: f64,
    /// Transmit VGA gain, dB.
    pub tx_gain_db: u32,
    /// Whether the 14 dB RF amplifier is on.
    pub amp: bool,
    /// Seconds of signal sent to the radio since it started transmitting.
    pub transmitted_s: f64,
    /// Seconds of signal buffered in the station, not yet handed to USB.
    pub queued_s: f64,
}

/// The queue between the station and the writer thread.
#[derive(Default)]
struct Shared {
    /// 8-bit I/Q bytes waiting to go to the radio.
    queue: VecDeque<u8>,
    /// Bytes handed to USB so far.
    sent_bytes: u64,
    /// The station asked for the tail to be flushed and the radio stopped.
    finishing: bool,
    /// The writer is done (radio off, or failed, or stopped).
    finished: bool,
    /// The station asked the writer to leave without draining (a stop).
    abandon: bool,
    /// The writer's first error, if any.
    error: Option<String>,
}

/// The shared state plus the two sides' wake-ups.
#[derive(Default)]
struct Sync {
    state: Mutex<Shared>,
    /// Wakes the writer (data arrived, or a flag changed).
    data: Condvar,
    /// Wakes the station (room freed, or an error arrived).
    room: Condvar,
}

/// An open HackRF One in transmit configuration, streaming the station's signal.
pub(crate) struct HackrfSink {
    settings: HackrfSettings,
    sync: Arc<Sync>,
    /// Resampling and the IF mixer, run by the station's thread.
    resampler: Resampler,
    phase: f64,
    phase_inc: f64,
    prebuffer_bytes: usize,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl HackrfSink {
    /// Find a HackRF (by serial, or the first one), configure it for `settings`
    /// and start the writer thread. Transmitting begins once `buffer_s` of
    /// signal has been queued; until then the samples just wait in the queue.
    pub fn open(settings: &HackrfSettings) -> Result<Self> {
        let mut handle = open_device(settings.serial.as_deref())?;
        // Linux: the kernel's driver (if any) is detached for our claims.
        let _ = handle.set_auto_detach_kernel_driver(true);
        handle
            .claim_interface(INTERFACE)
            .map_err(|e| StationError::Hackrf(format!("claiming the USB interface failed: {e}")))?;

        set_freq(&mut handle, settings.lo_hz())?;
        set_sample_rate(&mut handle, settings.sample_rate_hz)?;
        set_txvga_gain(&mut handle, settings.tx_gain_db)?;
        set_amp(&mut handle, settings.amp_enable)?;

        let rate = settings.sample_rate_hz;
        let resampler = Resampler::new(48_000, rate, 2, ResamplerQuality::Balanced)
            .map_err(StationError::Output)?;
        let prebuffer_bytes = (settings.buffer_s.clamp(0.05, 10.0) * rate as f64 * 2.0) as usize;

        let sync = Arc::new(Sync::default());
        let writer = {
            let sync = Arc::clone(&sync);
            std::thread::spawn(move || {
                writer_loop(handle, prebuffer_bytes, sync);
            })
        };

        Ok(Self {
            settings: settings.clone(),
            sync,
            resampler,
            phase: 0.0,
            phase_inc: 2.0 * std::f64::consts::PI * settings.if_offset_hz / f64::from(rate),
            prebuffer_bytes,
            writer: Some(writer),
        })
    }

    /// The settings, for the status.
    pub fn settings(&self) -> &HackrfSettings {
        &self.settings
    }

    /// Signal sent to the radio so far, seconds (0 before transmitting started).
    pub fn transmitted_s(&self) -> f64 {
        let s = self.sync.state.lock().expect("the queue lock");
        s.sent_bytes as f64 / (f64::from(self.settings.sample_rate_hz) * 2.0)
    }

    /// Signal queued in the station, not yet handed to USB, seconds.
    pub fn queued_s(&self) -> f64 {
        let s = self.sync.state.lock().expect("the queue lock");
        s.queue.len() as f64 / (f64::from(self.settings.sample_rate_hz) * 2.0)
    }

    /// Queue interleaved 48 kHz I/Q samples for the radio. Waits while the
    /// queue holds more than the prebuffer of headroom (checking `stop` in
    /// slices), which paces the station to real time.
    pub fn write(&mut self, samples: &[f32], stop: &StopHandle) -> Result<()> {
        debug_assert_eq!(samples.len() % 2, 0, "interleaved I/Q");
        if stop.is_stopped() {
            return Ok(());
        }
        // 1–2: resample to the HackRF's rate, shift the spectrum to the IF and
        //      scale to 8-bit signed I/Q (see `shift_and_convert`).
        let mut bytes = Vec::with_capacity(samples.len() / 48_000 * self.settings.sample_rate_hz as usize * 2);
        let mut resampled = Vec::new();
        self.resampler.process_into(samples, &mut resampled);
        shift_and_convert(&resampled, &mut self.phase, self.phase_inc, &mut bytes);
        // 3. Hand them over, then wait for the headroom to free up.
        let mut s = self.sync.state.lock().expect("the queue lock");
        if let Some(e) = &s.error {
            return Err(StationError::Hackrf(e.clone()));
        }
        s.queue.extend(bytes);
        self.sync.data.notify_all();
        while s.queue.len() > self.prebuffer_bytes {
            if stop.is_stopped() || s.error.is_some() || s.finished {
                return if let Some(e) = s.error.clone() { Err(StationError::Hackrf(e)) } else { Ok(()) };
            }
            let (guard, _) = self.sync.room.wait_timeout(s, POLL).expect("the queue lock");
            s = guard;
            if let Some(e) = &s.error {
                return Err(StationError::Hackrf(e.clone()));
            }
        }
        Ok(())
    }

    /// Let the last of the signal go out: push the resampler's tail through
    /// with silence, wait for the queue to drain, and switch the radio off.
    /// Skipped when `stop` is set (the station is being torn down): the writer
    /// abandons the queue and switches the radio off at once.
    pub fn drain(&mut self, stop: &StopHandle) {
        let mut s = self.sync.state.lock().expect("the queue lock");
        if stop.is_stopped() {
            s.abandon = true;
            self.sync.data.notify_all();
            drop(s);
            self.join();
            return;
        }
        // The resampler's tail and enough silence to push it (and whatever the
        // radio still holds) through: the writer paces them out at real time.
        let tail = self.resampler.flush();
        let mut converted = Vec::new();
        shift_and_convert(&tail, &mut self.phase, self.phase_inc, &mut converted);
        s.queue.extend(converted);
        let mut silence = vec![0.0; self.prebuffer_bytes.min(BLOCK_BYTES * 64)];
        let mut converted = Vec::new();
        shift_and_convert(&silence, &mut self.phase, self.phase_inc, &mut converted);
        s.queue.extend(converted);
        silence.clear();
        s.finishing = true;
        self.sync.data.notify_all();
        while !(s.queue.is_empty() && s.finished) && s.error.is_none() {
            let (guard, _) = self.sync.room.wait_timeout(s, POLL).expect("the queue lock");
            s = guard;
        }
        drop(s);
        self.join();
    }

    /// Wait for the writer thread to end and take it back.
    fn join(&mut self) {
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

/// The writer thread: keeps the radio fed, block by block, so it never
/// underruns between the station's frames. Starts transmitting once
/// `prebuffer_bytes` of signal is queued; sets the radio off at the end
/// (drained, abandoned or failed).
fn writer_loop(
    mut handle: rusb::DeviceHandle<rusb::GlobalContext>,
    prebuffer_bytes: usize,
    sync: Arc<Sync>,
) {
    let mut block = vec![0u8; BLOCK_BYTES];
    let mut started = false;
    'outer: loop {
        // Wait for work: data to send, the finish signal, or abandonment.
        let mut s = sync.state.lock().expect("the queue lock");
        loop {
            if s.abandon || s.error.is_some() {
                stop_radio(&mut handle);
                s.finished = true;
                sync.room.notify_all();
                return;
            }
            if s.finishing && s.queue.is_empty() {
                stop_radio(&mut handle);
                s.finished = true;
                sync.room.notify_all();
                return;
            }
            // Once transmitting, any block is taken; before that, the whole
            // prebuffer must be there (writing to the endpoint of an idle radio
            // would block until the timeout).
            if !s.queue.is_empty() && (started || s.queue.len() >= prebuffer_bytes) {
                break;
            }
            let (guard, _) = sync.data.wait_timeout(s, POLL).expect("the queue lock");
            s = guard;
        }
        // Transmitting starts with the first block sent.
        if !started {
            if let Err(e) = handle.write_control(
                request_type_out(),
                REQ_SET_TRANSCEIVER_MODE,
                MODE_TRANSMIT,
                0,
                &[],
                USB_TIMEOUT,
            ) {
                s.error = Some(format!("starting the transmitter failed: {e}"));
                s.finished = true;
                sync.room.notify_all();
                return;
            }
            started = true;
        }
        // Take one block out (the lock is not held during the USB write).
        let n = s.queue.len().min(BLOCK_BYTES);
        for (b, q) in block.iter_mut().zip(s.queue.drain(..n)) {
            *b = q;
        }
        sync.data.notify_all();
        drop(s);
        match handle.write_bulk(ENDPOINT_TX, &block[..n], USB_TIMEOUT) {
            Ok(written) => {
                let mut s = sync.state.lock().expect("the queue lock");
                s.sent_bytes += written as u64;
                sync.room.notify_all();
            }
            Err(e) => {
                let mut s = sync.state.lock().expect("the queue lock");
                s.error = Some(format!("writing to the HackRF failed: {e}"));
                stop_radio(&mut handle);
                s.finished = true;
                sync.room.notify_all();
                return;
            }
        }
        continue 'outer;
    }
}

/// Switch the radio off and release it (idempotent; errors ignored).
fn stop_radio(handle: &mut rusb::DeviceHandle<rusb::GlobalContext>) {
    let _ = handle.write_control(
        request_type_out(),
        REQ_SET_TRANSCEIVER_MODE,
        MODE_OFF,
        0,
        &[],
        USB_TIMEOUT,
    );
    let _ = handle.release_interface(INTERFACE);
}

/// Shift the spectrum of interleaved I/Q samples up by `if_offset` (continuing
/// the mixer from `phase`, which is advanced for the next call) and append them
/// to `out` as 8-bit signed I/Q: the format of the HackRF's bulk endpoint.
fn shift_and_convert(samples: &[f32], phase: &mut f64, phase_inc: f64, out: &mut Vec<u8>) {
    let mut ph = *phase;
    for frame in samples.as_chunks::<2>().0 {
        let (i, q) = (f64::from(frame[0]), f64::from(frame[1]));
        let (c, s) = (ph.cos(), ph.sin());
        let (i, q) = (i * c - q * s, i * s + q * c);
        let (i, q) = ((i * 127.0).round().clamp(-127.0, 127.0), (q * 127.0).round().clamp(-127.0, 127.0));
        out.push(i as i8 as u8);
        out.push(q as i8 as u8);
        ph += phase_inc;
        if ph >= 2.0 * std::f64::consts::PI {
            ph -= 2.0 * std::f64::consts::PI;
        }
    }
    *phase = ph;
}

/// Find the HackRF with this serial (or the first one) and open it.
fn open_device(serial: Option<&str>) -> Result<rusb::DeviceHandle<rusb::GlobalContext>> {
    let devices = rusb::devices()
        .map_err(|e| StationError::Hackrf(format!("listing USB devices failed: {e}")))?;
    let mut found = Vec::new();
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != VENDOR_ID || desc.product_id() != PRODUCT_ID {
            continue;
        }
        match device.open() {
            Ok(handle) => {
                let sn = handle
                    .read_languages(Duration::from_secs(1))
                    .ok()
                    .and_then(|langs| langs.first().copied())
                    .and_then(|lang| {
                        handle
                            .read_serial_number_string(lang, &desc, Duration::from_secs(1))
                            .ok()
                    })
                    .unwrap_or_default();
                found.push((handle, sn));
            }
            Err(e) => {
                return Err(StationError::Hackrf(format!(
                    "the HackRF was found but could not be opened ({}). On Windows, install the HackRF's WinUSB driver (Zadig); on Linux, check the udev rules or run as root.",
                    e
                )));
            }
        }
    }
    match (serial, found.is_empty()) {
        (_, true) => Err(StationError::Hackrf(
            "no HackRF One found (USB id 1d50:6089). Check that it is connected and not in use by another program.".into(),
        )),
        (None, false) => Ok(found.into_iter().next().expect("checked").0),
        (Some(serial), false) => found
            .into_iter()
            .find(|(_, sn)| sn == serial)
            .map(|(handle, _)| handle)
            .ok_or_else(|| StationError::Hackrf(format!("no HackRF with serial number \"{serial}\""))),
    }
}

/// Recipient device, vendor type, host-to-device: the request type of every
/// libhackrf control transfer with data or a value.
fn request_type_out() -> u8 {
    rusb::request_type(rusb::Direction::Out, rusb::RequestType::Vendor, rusb::Recipient::Device)
}

/// Recipient device, vendor type, device-to-host: the request type of libhackrf's
/// transfers that read a status reply.
fn request_type_in() -> u8 {
    rusb::request_type(rusb::Direction::In, rusb::RequestType::Vendor, rusb::Recipient::Device)
}

/// Set the local oscillator to `freq_hz` (request 16: MHz and Hz, two `u32`s).
fn set_freq(handle: &mut rusb::DeviceHandle<rusb::GlobalContext>, freq_hz: u64) -> Result<()> {
    if !(1_000_000..=6_000_000_000).contains(&freq_hz) {
        return Err(StationError::Hackrf(format!(
            "frequency {freq_hz} Hz is outside the HackRF's 1 MHz–6 GHz range"
        )));
    }
    let data: [u8; 8] = [
        (freq_hz / 1_000_000) as u8,
        ((freq_hz / 1_000_000) >> 8) as u8,
        ((freq_hz / 1_000_000) >> 16) as u8,
        ((freq_hz / 1_000_000) >> 24) as u8,
        (freq_hz % 1_000_000) as u8,
        ((freq_hz % 1_000_000) >> 8) as u8,
        ((freq_hz % 1_000_000) >> 16) as u8,
        ((freq_hz % 1_000_000) >> 24) as u8,
    ];
    handle
        .write_control(request_type_out(), REQ_SET_FREQ, 0, 0, &data, USB_TIMEOUT)
        .map(|_| ())
        .map_err(|e| StationError::Hackrf(format!("setting the frequency failed: {e}")))
}

/// Set the sample rate to `rate_hz` (request 6: rate and divider, two `u32`s;
/// the divider of 1 is libhackrf's for whole MHz, and the firmware's fractional
/// divider accepts any rate this way), and the baseband filter to match.
fn set_sample_rate(handle: &mut rusb::DeviceHandle<rusb::GlobalContext>, rate_hz: u32) -> Result<()> {
    if !(2_000_000..=20_000_000).contains(&rate_hz) {
        return Err(StationError::Hackrf(format!(
            "sample rate {rate_hz} Hz is outside the HackRF's 2–20 MHz range (2 MHz is plenty for DRM)"
        )));
    }
    let data: [u8; 8] = [
        rate_hz as u8,
        (rate_hz >> 8) as u8,
        (rate_hz >> 16) as u8,
        (rate_hz >> 24) as u8,
        1,
        0,
        0,
        0,
    ];
    handle
        .write_control(
            request_type_out(),
            REQ_SAMPLE_RATE_SET,
            0,
            0,
            &data,
            USB_TIMEOUT,
        )
        .map(|_| ())
        .map_err(|e| StationError::Hackrf(format!("setting the sample rate failed: {e}")))?;
    // 3/4 of the rate, within the filter's 1.75–28 MHz range.
    let bw = (rate_hz as u64 * 3 / 4).clamp(1_750_000, 28_000_000) as u32;
    handle
        .write_control(
            request_type_out(),
            REQ_BASEBAND_FILTER_BANDWIDTH_SET,
            (bw & 0xFFFF) as u16,
            (bw >> 16) as u16,
            &[],
            USB_TIMEOUT,
        )
        .map(|_| ())
        .map_err(|e| StationError::Hackrf(format!("setting the baseband filter failed: {e}")))
}

/// Set the transmit VGA gain, 0–47 dB (request 21). A device-to-host transfer
/// carrying the firmware's one-byte status reply, like libhackrf's.
fn set_txvga_gain(handle: &mut rusb::DeviceHandle<rusb::GlobalContext>, gain_db: u32) -> Result<()> {
    if gain_db > 47 {
        return Err(StationError::Hackrf(format!(
            "tx_gain_db {gain_db} is outside the HackRF's 0–47 dB range"
        )));
    }
    // The firmware replies with a non-zero byte once the gain is set (libhackrf's
    // hackrf_set_txvga_gain: `result != 1 || !retval` is the error case).
    let mut status = [0u8; 1];
    let read = handle
        .read_control(request_type_in(), REQ_SET_TXVGA_GAIN, 0, gain_db as u16, &mut status, USB_TIMEOUT)
        .map_err(|e| StationError::Hackrf(format!("setting the transmit gain failed: {e}")))?;
    if read == 1 && status[0] != 0 {
        Ok(())
    } else {
        Err(StationError::Hackrf(format!(
            "the HackRF rejected tx_gain_db {gain_db} (status 0x{:02x})",
            status[0]
        )))
    }
}

/// Enable or disable the 14 dB RF amplifier (request 17).
fn set_amp(handle: &mut rusb::DeviceHandle<rusb::GlobalContext>, enable: bool) -> Result<()> {
    handle
        .write_control(
            request_type_out(),
            REQ_AMP_ENABLE,
            u16::from(enable),
            0,
            &[],
            USB_TIMEOUT,
        )
        .map(|_| ())
        .map_err(|e| StationError::Hackrf(format!("switching the RF amplifier failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lo_is_below_the_channel_by_the_if() {
        let s = HackrfSettings {
            frequency_hz: 15_500_000,
            if_offset_hz: 250_000.0,
            ..Default::default()
        };
        assert_eq!(s.lo_hz(), 15_250_000);
    }

    /// A complex tone at f, shifted up by the IF, appears at IF + f: check the
    /// output samples against a cosine of that frequency.
    #[test]
    fn shift_and_convert_moves_the_spectrum_up() {
        // A 9.5 kHz tone in the (already resampled) baseband at 2 Msps, shifted
        // to +250 kHz: a 259.5 kHz tone at the HackRF's input.
        let (rate, tone, if_offset) = (2_000_000.0_f64, 9_500.0, 250_000.0);
        let phase_inc = 2.0 * std::f64::consts::PI * if_offset / rate;
        let n = 20_000;
        let samples: Vec<f32> = (0..n)
            .map(|k| {
                let a = 2.0 * std::f64::consts::PI * tone * k as f64 / rate;
                [a.cos() as f32, a.sin() as f32]
            })
            .flatten()
            .collect();
        let mut bytes = Vec::with_capacity(n * 2);
        let mut phase = 0.0;
        shift_and_convert(&samples, &mut phase, phase_inc, &mut bytes);
        assert_eq!(bytes.len(), n * 2);
        // The output frame k is 127·cos/sin of 2πk·(tone + if_offset)/rate;
        // 8-bit rounding allows 1/127 ≈ 0.008, the bound covers the f32 input.
        for k in [0usize, 1234, 15_000] {
            let a = 2.0 * std::f64::consts::PI * (tone + if_offset) * k as f64 / rate;
            assert!((f64::from(bytes[2 * k] as i8) / 127.0 - a.cos()).abs() < 0.02, "I at {k}");
            assert!((f64::from(bytes[2 * k + 1] as i8) / 127.0 - a.sin()).abs() < 0.02, "Q at {k}");
        }
        // The mixer continues across calls: the phase is kept, not reset.
        let mut more = Vec::new();
        shift_and_convert(&samples[..2], &mut phase, phase_inc, &mut more);
        let a = 2.0 * std::f64::consts::PI * (tone + if_offset) * n as f64 / rate;
        assert!((f64::from(more[0] as i8) / 127.0 - a.cos()).abs() < 0.02, "continues");
    }

    /// Full-scale samples are clipped, not wrapped (a wrapped int8 would flip sign
    /// and splatter over the whole spectrum).
    #[test]
    fn shift_and_convert_clips() {
        let mut bytes = Vec::new();
        let mut phase = 0.0;
        shift_and_convert(&[2.0, 2.0, -3.0, 0.0], &mut phase, 0.0, &mut bytes);
        assert_eq!(bytes, [127, 127, 0x81 /* −127 */, 0]);
    }
}
