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
use std::time::{Duration, Instant};

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

/// How long the queue must stay empty, while transmitting, before it is called an
/// underrun: the radio's own buffer bridges shorter gaps.
const UNDERRUN_GUARD: Duration = Duration::from_millis(100);

/// Extra time `drain` allows on top of the queued signal's own length before it
/// gives up on a radio that stopped taking data.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The most the station's queue may hold, bytes. `buffer_s` seconds at the sample
/// rate would be 400 MB at 20 Msps with the longest setting; 64 MB is 16 s at
/// 2 Msps and 1.6 s at 20 Msps, plenty of headroom either way.
const MAX_PREBUFFER_BYTES: usize = 64 << 20;

/// The signal kept queued in the station before/pacing the radio, bytes: `buffer_s`
/// seconds of 8-bit I/Q at the sample rate, clamped and capped.
fn prebuffer_bytes(settings: &HackrfSettings) -> usize {
    let bytes = settings.buffer_s.clamp(0.05, 10.0) * f64::from(settings.sample_rate_hz) * 2.0;
    (bytes as usize).min(MAX_PREBUFFER_BYTES)
}

/// Settings that describe the radio on air, for [`crate::StationStatus`].
#[derive(Debug, Clone, PartialEq)]
pub struct HackrfStatus {
    /// Nominal frequency of the DRM channel on air, Hz: the DC carrier.
    pub frequency_hz: u64,
    /// Lowest and highest carrier of the signal on air, Hz: the 18 and 20 kHz
    /// occupancies reach well above `frequency_hz` and only a little below it.
    pub band_lo_hz: f64,
    /// See [`Self::band_lo_hz`].
    pub band_hi_hz: f64,
    /// Local-oscillator frequency, Hz (the nominal frequency less the IF offset).
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
    /// Times the station fell behind and left the radio's buffer empty while
    /// transmitting (each risks a gap on air; should stay 0).
    pub underruns: u64,
}

/// The queue between the station and the writer thread.
#[derive(Default)]
struct Shared {
    /// 8-bit I/Q bytes waiting to go to the radio.
    queue: VecDeque<u8>,
    /// Bytes handed to USB so far.
    sent_bytes: u64,
    /// Times the queue ran dry while transmitting: the station fell behind.
    underruns: u64,
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
    /// The resampler's output, reused between frames.
    resampled: Vec<f32>,
    mixer: Mixer,
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
        let prebuffer_bytes = prebuffer_bytes(settings);

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
            resampled: Vec::new(),
            mixer: Mixer::new(settings.if_offset_hz, f64::from(rate)),
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

    /// Times the queue ran dry while transmitting (see [`HackrfStatus::underruns`]).
    pub fn underruns(&self) -> u64 {
        self.sync.state.lock().expect("the queue lock").underruns
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
        // The output is the input rate times the HackRF's, two bytes per frame.
        let rate = self.settings.sample_rate_hz as usize;
        let mut bytes = Vec::with_capacity(samples.len() * rate / 48_000 + 4 * 1024);
        self.resampled.clear();
        self.resampler.process_into(samples, &mut self.resampled);
        shift_and_convert(&self.resampled, &mut self.mixer, &mut bytes);
        // 3. Hand them over, then wait for the headroom to free up.
        let mut s = self.sync.state.lock().expect("the queue lock");
        if let Some(e) = &s.error {
            return Err(StationError::Hackrf(e.clone()));
        }
        s.queue.extend(bytes);
        self.sync.data.notify_all();
        while s.queue.len() > self.prebuffer_bytes {
            if let Some(e) = &s.error {
                return Err(StationError::Hackrf(e.clone()));
            }
            if stop.is_stopped() {
                return Ok(());
            }
            if s.finished {
                return Err(StationError::Hackrf("the HackRF writer has stopped".into()));
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
    /// abandons the queue and switches the radio off at once. An error the writer
    /// met while flushing is returned, so a truncated tail is not reported as a
    /// clean end.
    pub fn drain(&mut self, stop: &StopHandle) -> Result<()> {
        let mut s = self.sync.state.lock().expect("the queue lock");
        if stop.is_stopped() {
            s.abandon = true;
            self.sync.data.notify_all();
            drop(s);
            self.join();
            return Ok(());
        }
        // The resampler's tail and enough silence to push it (and whatever the
        // radio still holds) through: the writer paces them out at real time.
        let tail = self.resampler.flush();
        let mut converted = Vec::new();
        shift_and_convert(&tail, &mut self.mixer, &mut converted);
        s.queue.extend(converted);
        let mut silence = vec![0.0; self.prebuffer_bytes.min(BLOCK_BYTES * 64)];
        let mut converted = Vec::new();
        shift_and_convert(&silence, &mut self.mixer, &mut converted);
        s.queue.extend(converted);
        silence.clear();
        s.finishing = true;
        self.sync.data.notify_all();
        // The writer takes the queue at the radio's rate, so the drain takes about
        // as long as what is queued; a stall well past that (a wedged device) must
        // not hang the station for ever.
        let left_s = s.queue.len() as f64 / (f64::from(self.settings.sample_rate_hz) * 2.0);
        let deadline = Instant::now() + Duration::from_secs_f64(left_s) + DRAIN_GRACE;
        while !(s.queue.is_empty() && s.finished) && s.error.is_none() {
            if Instant::now() >= deadline {
                s.abandon = true;
                self.sync.data.notify_all();
                drop(s);
                self.join();
                return Err(StationError::Hackrf(
                    "the HackRF did not take the last of the signal".into(),
                ));
            }
            let (guard, _) = self.sync.room.wait_timeout(s, POLL).expect("the queue lock");
            s = guard;
        }
        let error = s.error.clone();
        drop(s);
        self.join();
        match error {
            Some(e) => Err(StationError::Hackrf(e)),
            None => Ok(()),
        }
    }

    /// Wait for the writer thread to end and take it back.
    fn join(&mut self) {
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

impl Drop for HackrfSink {
    /// A station dropped without `finish` (a panic, an early error) must not leave
    /// the writer thread holding the radio — the next start would find it busy. The
    /// writer switches the radio off and exits; `drain` has already joined when the
    /// station finished normally, so this is then a no-op. (A write already in
    /// flight ends by its USB timeout, so a drop can take that long in the worst case.)
    fn drop(&mut self) {
        if self.writer.is_none() {
            return;
        }
        if let Ok(mut s) = self.sync.state.lock() {
            s.abandon = true;
            self.sync.data.notify_all();
        }
        self.join();
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
    // When the queue ran dry while transmitting, and whether that episode has been
    // counted already (one per episode, and only past `UNDERRUN_GUARD`).
    let mut starving_since: Option<Instant> = None;
    let mut starving_counted = false;
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
            // would block until the timeout). Finishing bypasses the prebuffer: a
            // transmission shorter than it must still flush its tail.
            if ready_to_send(started, s.finishing, s.queue.len(), prebuffer_bytes) {
                starving_since = None;
                starving_counted = false;
                break;
            }
            // Transmitting with nothing queued: the station fell behind and the
            // radio's buffer is draining. Counted as an underrun once the queue has
            // been empty for `UNDERRUN_GUARD`: the radio's buffer bridges shorter
            // gaps, and the tail of a transmission drains the queue too.
            if started
                && !s.finishing
                && starvation_episode(
                    &mut starving_since,
                    &mut starving_counted,
                    UNDERRUN_GUARD,
                    Instant::now(),
                )
            {
                s.underruns += 1;
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

/// The IF mixer: rotates the baseband I/Q by the intermediate frequency, one
/// complex multiply per sample (the step's trigonometry is computed once), with the
/// phasor renormalized every few thousand samples so rounding cannot drift.
struct Mixer {
    /// The next sample's rotation: `(cos, sin)` of the accumulated phase.
    cos: f64,
    sin: f64,
    /// One sample's rotation.
    step_cos: f64,
    step_sin: f64,
    /// Samples since the phasor was last renormalized.
    since_norm: u32,
}

/// Samples between the mixer's phasor renormalizations.
const NORM_EVERY: u32 = 4096;

impl Mixer {
    /// A mixer that shifts by `if_offset_hz` at `rate_hz` (a negative offset shifts
    /// the spectrum down).
    fn new(if_offset_hz: f64, rate_hz: f64) -> Self {
        let step = 2.0 * std::f64::consts::PI * if_offset_hz / rate_hz;
        Self {
            cos: 1.0,
            sin: 0.0,
            step_cos: step.cos(),
            step_sin: step.sin(),
            since_norm: 0,
        }
    }

    /// The rotation for the next sample: identity first, like a phase started at zero.
    fn next(&mut self) -> (f64, f64) {
        let (cos, sin) = (self.cos, self.sin);
        self.cos = cos * self.step_cos - sin * self.step_sin;
        self.sin = sin * self.step_cos + cos * self.step_sin;
        self.since_norm += 1;
        if self.since_norm >= NORM_EVERY {
            let magnitude = self.cos.hypot(self.sin);
            self.cos /= magnitude;
            self.sin /= magnitude;
            self.since_norm = 0;
        }
        (cos, sin)
    }
}

/// Shift the spectrum of interleaved I/Q samples by the mixer's offset and append
/// them to `out` as 8-bit signed I/Q: the format of the HackRF's bulk endpoint.
/// Full-scale input is clipped, not wrapped (a wrapped int8 would flip sign and
/// splatter over the whole spectrum).
fn shift_and_convert(samples: &[f32], mixer: &mut Mixer, out: &mut Vec<u8>) {
    for frame in samples.as_chunks::<2>().0 {
        let (cos, sin) = mixer.next();
        let (i, q) = (f64::from(frame[0]), f64::from(frame[1]));
        let (i, q) = (i * cos - q * sin, i * sin + q * cos);
        let (i, q) = (
            (i * 127.0).round().clamp(-127.0, 127.0),
            (q * 127.0).round().clamp(-127.0, 127.0),
        );
        out.push(i as i8 as u8);
        out.push(q as i8 as u8);
    }
}

/// Whether the writer may take a block out of the queue: a transmitting radio takes
/// whatever is there, a station that is finishing flushes its tail (even when the
/// prebuffer was never reached — a short transmission must still go out), and a
/// fresh radio waits for the whole prebuffer (writing to the endpoint of an idle
/// radio would block until the timeout).
fn ready_to_send(started: bool, finishing: bool, queued: usize, prebuffer_bytes: usize) -> bool {
    queued > 0 && (started || finishing || queued >= prebuffer_bytes)
}

/// Track one episode of the queue running dry while transmitting: `true` once,
/// when the episode has lasted `guard` (the radio's own buffer bridges shorter
/// gaps). `since`/`counted` are reset when data arrives again.
fn starvation_episode(
    since: &mut Option<Instant>,
    counted: &mut bool,
    guard: Duration,
    now: Instant,
) -> bool {
    match *since {
        None => {
            *since = Some(now);
            false
        }
        Some(start) if !*counted && now.duration_since(start) >= guard => {
            *counted = true;
            true
        }
        Some(_) => false,
    }
}

/// Find the HackRF with this serial (or the first one) and open it.
///
/// A radio that cannot be opened (busy, no driver) does not hide the others: the
/// first one that opens is used, and its error is only reported when nothing opens.
fn open_device(serial: Option<&str>) -> Result<rusb::DeviceHandle<rusb::GlobalContext>> {
    let devices = rusb::devices()
        .map_err(|e| StationError::Hackrf(format!("listing USB devices failed: {e}")))?;
    let mut found = Vec::new();
    let mut unopenable: Vec<String> = Vec::new();
    for device in devices.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != VENDOR_ID || desc.product_id() != PRODUCT_ID {
            continue;
        }
        match device.open() {
            Ok(handle) => {
                let sn = read_serial(&handle, &desc).unwrap_or_default();
                found.push((handle, sn));
            }
            Err(e) => unopenable.push(e.to_string()),
        }
    }
    let answered = found.len();
    // The advice for a radio that is on the bus but would not open.
    let why = |reason: &str| {
        format!(
            "another program may be using it, or its driver is missing ({reason}); on Windows install the WinUSB driver (Zadig), on Linux check the udev rules or run as root"
        )
    };
    match serial {
        Some(serial) => match found.into_iter().find(|(_, sn)| sn == serial) {
            Some((handle, _)) => Ok(handle),
            None => {
                let mut message =
                    format!("no HackRF with serial number \"{serial}\" ({answered} other HackRF(s) answered)");
                if let Some(first) = unopenable.first() {
                    message.push_str("; a HackRF could not be opened: ");
                    message.push_str(&why(first));
                }
                Err(StationError::Hackrf(message))
            }
        },
        None => match (answered, unopenable.first()) {
            (0, None) => Err(StationError::Hackrf(
                "no HackRF One found (USB id 1d50:6089). Check that it is connected and not in use by another program.".into(),
            )),
            (0, Some(first)) => Err(StationError::Hackrf(format!(
                "the HackRF could not be opened: {}",
                why(first)
            ))),
            _ => Ok(found.into_iter().next().expect("checked").0),
        },
    }
}

/// The radio's serial number, if it answers.
fn read_serial(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    desc: &rusb::DeviceDescriptor,
) -> Option<String> {
    let lang = handle
        .read_languages(Duration::from_secs(1))
        .ok()?
        .first()
        .copied()?;
    handle
        .read_serial_number_string(lang, desc, Duration::from_secs(1))
        .ok()
}

/// A HackRF One seen on the USB bus (see [`devices`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HackrfDevice {
    /// The radio's serial number, when it could be read.
    pub serial: Option<String>,
    /// Why the radio could not be opened (a missing WinUSB driver, udev rules),
    /// when it could not.
    pub error: Option<String>,
}

/// The HackRF Ones on the USB bus, without keeping any of them open: what the GUI
/// shows before transmitting. A radio that is present but cannot be opened (no
/// WinUSB driver on Windows, no udev rule on Linux) is listed with the reason.
pub fn devices() -> Result<Vec<HackrfDevice>> {
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
        found.push(match device.open() {
            Ok(handle) => HackrfDevice {
                serial: read_serial(&handle, &desc),
                error: None,
            },
            Err(e) => HackrfDevice {
                serial: None,
                error: Some(e.to_string()),
            },
        });
    }
    Ok(found)
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
        let n = 20_000;
        let samples: Vec<f32> = (0..n)
            .flat_map(|k| {
                let a = 2.0 * std::f64::consts::PI * tone * k as f64 / rate;
                [a.cos() as f32, a.sin() as f32]
            })
            .collect();
        let mut bytes = Vec::with_capacity(n * 2);
        let mut mixer = Mixer::new(if_offset, rate);
        shift_and_convert(&samples, &mut mixer, &mut bytes);
        assert_eq!(bytes.len(), n * 2);
        // The output frame k is 127·cos/sin of 2πk·(tone + if_offset)/rate;
        // 8-bit rounding allows 1/127 ≈ 0.008, the bound covers the f32 input.
        for k in [0usize, 1234, 15_000] {
            let a = 2.0 * std::f64::consts::PI * (tone + if_offset) * k as f64 / rate;
            assert!((f64::from(bytes[2 * k] as i8) / 127.0 - a.cos()).abs() < 0.02, "I at {k}");
            assert!((f64::from(bytes[2 * k + 1] as i8) / 127.0 - a.sin()).abs() < 0.02, "Q at {k}");
        }
        // The mixer continues across calls: the rotation is kept, not reset.
        let mut more = Vec::new();
        shift_and_convert(&samples[..2], &mut mixer, &mut more);
        let a = 2.0 * std::f64::consts::PI * (tone + if_offset) * n as f64 / rate;
        assert!((f64::from(more[0] as i8) / 127.0 - a.cos()).abs() < 0.02, "continues");
    }

    /// Full-scale samples are clipped, not wrapped (a wrapped int8 would flip sign
    /// and splatter over the whole spectrum).
    #[test]
    fn shift_and_convert_clips() {
        let mut bytes = Vec::new();
        let mut mixer = Mixer::new(0.0, 48_000.0);
        shift_and_convert(&[2.0, 2.0, -3.0, 0.0], &mut mixer, &mut bytes);
        assert_eq!(bytes, [127, 127, 0x81 /* −127 */, 0]);
    }

    /// The rotation stays on the unit circle over a long run: one multiply-add pair
    /// per sample, renormalized so rounding cannot drift into gain or loss.
    #[test]
    fn mixer_stays_unit_and_advances() {
        let rate = 2_000_000.0;
        let offset = 250_000.0;
        let mut mixer = Mixer::new(offset, rate);
        let n = 1_000_000;
        let mut last = (0.0, 0.0);
        for _ in 0..n {
            last = mixer.next();
        }
        assert!((last.0.hypot(last.1) - 1.0).abs() < 1e-9, "{last:?}");
        // After n samples the phase is n steps on; the phasor matches cos/sin of it.
        let phase = 2.0 * std::f64::consts::PI * offset * n as f64 / rate;
        assert!((mixer.cos - phase.cos()).abs() < 1e-9);
        assert!((mixer.sin - phase.sin()).abs() < 1e-9);
    }
    /// The queue is `buffer_s` seconds of 8-bit I/Q, clamped and capped so a high
    /// sample rate with a long buffer cannot eat the memory.
    #[test]
    fn prebuffer_follows_the_settings_with_a_cap() {
        let bytes = |settings: HackrfSettings| prebuffer_bytes(&settings);
        assert_eq!(bytes(HackrfSettings::default()), 4_000_000, "1 s at 2 Msps");
        assert_eq!(
            bytes(HackrfSettings { buffer_s: 0.05, ..Default::default() }),
            200_000,
            "0.05 s at 2 Msps"
        );
        assert_eq!(
            bytes(HackrfSettings { sample_rate_hz: 20_000_000, buffer_s: 10.0, ..Default::default() }),
            64 << 20,
            "400 MB wanted, the 64 MB cap kept"
        );
        assert_eq!(
            bytes(HackrfSettings { buffer_s: 100.0, ..Default::default() }),
            40_000_000,
            "clamped to 10 s"
        );
        assert_eq!(
            bytes(HackrfSettings { buffer_s: 0.0, ..Default::default() }),
            200_000,
            "clamped to 0.05 s"
        );
    }

    /// The writer's decision: a fresh radio waits for the whole prebuffer, a
    /// transmitting one takes any block, and a finishing one flushes its tail even
    /// below the prebuffer (a short transmission must not stall in `drain`).
    #[test]
    fn ready_to_send_table() {
        let pre = 4_000_000;
        assert!(!ready_to_send(false, false, 0, pre), "nothing to send");
        assert!(!ready_to_send(false, false, pre - 1, pre), "a fresh radio waits");
        assert!(ready_to_send(false, false, pre, pre), "the prebuffer is there");
        assert!(ready_to_send(true, false, 1, pre), "a transmitting radio takes any block");
        assert!(ready_to_send(false, true, 1, pre), "finishing flushes the tail");
        assert!(!ready_to_send(true, true, 0, pre), "nothing left to flush");
    }

    /// A dry queue counts as an underrun once, and only after the guard: the
    /// radio's buffer bridges shorter gaps, and the station resets the episode
    /// when data arrives again.
    #[test]
    fn starvation_counts_once_past_the_guard() {
        let guard = Duration::from_millis(100);
        let t0 = Instant::now();
        let mut since = None;
        let mut counted = false;
        // The first sighting only starts the episode.
        assert!(!starvation_episode(&mut since, &mut counted, guard, t0));
        // Shorter than the guard: the radio's buffer covers it.
        assert!(!starvation_episode(
            &mut since,
            &mut counted,
            guard,
            t0 + Duration::from_millis(50)
        ));
        // Past the guard: an underrun, counted.
        assert!(starvation_episode(
            &mut since,
            &mut counted,
            guard,
            t0 + Duration::from_millis(120)
        ));
        // Still dry: not counted again.
        assert!(!starvation_episode(
            &mut since,
            &mut counted,
            guard,
            t0 + Duration::from_millis(500)
        ));
        // Data arrived (the loop resets the episode), then it runs dry again.
        since = Some(t0);
        counted = false;
        assert!(starvation_episode(
            &mut since,
            &mut counted,
            guard,
            t0 + Duration::from_millis(200)
        ));
    }
}
