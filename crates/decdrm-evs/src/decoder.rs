//! The 3GPP EVS floating-point reference decoder (TS 26.443), built from the
//! user-supplied source (see build.rs) and driven frame by frame through
//! `csrc/evs_shim.c`.

use std::ffi::c_void;
use std::os::raw::{c_int, c_uchar};
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

// Rust note: edition 2024 marks foreign declarations `unsafe extern`; calling them
// still needs `unsafe` blocks.
unsafe extern "C" {
    fn decdrm_evs_open(output_fs: c_int) -> *mut c_void;
    fn decdrm_evs_decode(handle: *mut c_void, bits: *const c_uchar, nbits: c_int, out: *mut f32) -> c_int;
    fn decdrm_evs_close(handle: *mut c_void);
}

/// The reference decoder keeps some state in C `static`s; one lock serialises every
/// call into it.
static LOCK: Mutex<()> = Mutex::new(());

/// Samples of the longest output frame (20 ms at 48 kHz).
const MAX_OUTPUT: usize = 960;

/// Errors of the EVS decoder.
#[derive(Debug, thiserror::Error)]
pub enum EvsError {
    #[error("EVS decoder: cannot open for {0} Hz output (8000, 16000, 32000 or 48000)")]
    Open(u32),
    #[error("EVS decoder: frame of {0} bits is not a valid size")]
    FrameSize(usize),
}

/// One EVS decoder instance (mono).
pub struct EvsDecoder {
    handle: NonNull<c_void>,
    rate: u32,
}

// Rust note: the raw pointer makes the type `!Send` by default. The C state belongs to
// this value alone and every call holds `LOCK`, so moving it between threads is sound.
unsafe impl Send for EvsDecoder {}

impl EvsDecoder {
    /// A decoder producing `output_rate` Hz (8000, 16000, 32000 or 48000).
    pub fn new(output_rate: u32) -> Result<Self, EvsError> {
        let rate = c_int::try_from(output_rate).map_err(|_| EvsError::Open(output_rate))?;
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: plain constructor; returns NULL on failure.
        let handle = unsafe { decdrm_evs_open(rate) };
        NonNull::new(handle).map(|handle| Self { handle, rate: output_rate }).ok_or(EvsError::Open(output_rate))
    }

    /// Output sampling rate, Hz.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Decode one frame (packed MSB first; its size gives the bit rate, e.g. 33 bytes
    /// = 264 bits = 13.2 kbit/s), or conceal a lost frame with `None`. Returns 20 ms of
    /// mono samples in [-1, 1].
    pub fn decode(&mut self, frame: Option<&[u8]>) -> Result<Vec<f32>, EvsError> {
        let mut out = vec![0f32; MAX_OUTPUT];
        let (ptr, nbits) = match frame {
            Some(f) => (f.as_ptr(), f.len() * 8),
            None => (std::ptr::null(), 0),
        };
        let nbits_c = c_int::try_from(nbits).map_err(|_| EvsError::FrameSize(nbits))?;
        let n = {
            let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `handle` is live (freed only in Drop), `ptr` points to `nbits / 8`
            // bytes or is NULL with nbits 0, and `out` holds the 960 samples the shim
            // may write.
            unsafe { decdrm_evs_decode(self.handle.as_ptr(), ptr, nbits_c, out.as_mut_ptr()) }
        };
        let n = usize::try_from(n).map_err(|_| EvsError::FrameSize(nbits))?;
        out.truncate(n);
        // The reference decoder works on a 16-bit scale.
        for s in &mut out {
            *s = (*s / 32768.0).clamp(-1.0, 1.0);
        }
        Ok(out)
    }
}

impl Drop for EvsDecoder {
    fn drop(&mut self) {
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the handle came from decdrm_evs_open and is closed once.
        unsafe { decdrm_evs_close(self.handle.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_concealment_give_20_ms() {
        assert!(EvsDecoder::new(44_100).is_err());
        let mut d = EvsDecoder::new(48_000).unwrap();
        // Before any frame: silence.
        assert_eq!(d.decode(None).unwrap(), vec![0.0; 960]);
        // Inactive super-wideband frames with zero parameters, then a lost frame.
        let mut frame = [0u8; 33];
        frame[0] = 0x74;
        for _ in 0..5 {
            let pcm = d.decode(Some(&frame)).unwrap();
            assert_eq!(pcm.len(), 960);
            assert!(pcm.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert_eq!(d.decode(None).unwrap().len(), 960);
        assert!(matches!(d.decode(Some(&[0u8; 400])), Err(EvsError::FrameSize(3200))));
        let mut d16 = EvsDecoder::new(16_000).unwrap();
        assert_eq!(d16.decode(Some(&frame)).unwrap().len(), 320);
    }
}
