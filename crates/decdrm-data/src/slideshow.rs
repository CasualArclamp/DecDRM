//! MOT SlideShow (ETSI TS 101 499): a receiver-side slide model for GUIs and the
//! transmitter feeder that cycles through a set of images.
//!
//! Receiver semantics implemented by [`SlideShow`]:
//!
//! * A slide whose TriggerTime is "Now" (or absent) is shown on reception; one with an
//!   absolute TriggerTime in the future waits until [`SlideShow::tick`] reaches it.
//! * A slide with the same ContentName as an earlier one replaces it in the history
//!   (the SlideShow memory is managed by ContentName).
//! * While the user browses older slides, new slides are stored but do not take over
//!   the display (Dream `SlideShowViewer`: "if the last received picture was selected,
//!   automatically show new picture").
//!
//! The feeder follows Dream's `CMOTSlideShowEncoder`: header mode, a fresh transport id
//! for each transmission, ContentName and TriggerTime = Now on every slide.

use crate::decoder::DataEvent;
use crate::encoder::DataUnitSource;
use crate::error::{DataError, Result};
use crate::mot::{MotEncoder, MotHeader, MotObject, content_type};
use crate::time::TriggerTime;
use std::collections::VecDeque;
use std::path::Path;

/// One received slide.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slide {
    /// Transport id it arrived with.
    pub transport_id: u16,
    /// ContentName.
    pub name: String,
    /// MIME type (`image/jpeg`, `image/png`, ...).
    pub mime: String,
    /// Image bytes.
    pub data: Vec<u8>,
    /// Presentation time.
    pub trigger: TriggerTime,
    /// Full MOT header (CategoryID/SlideID, ClickThroughURL, ...).
    pub header: MotHeader,
}

impl Slide {
    /// Build from a decoded MOT object.
    pub fn from_mot(transport_id: u16, header: MotHeader, data: Vec<u8>) -> Self {
        Self {
            transport_id,
            name: header.content_name().unwrap_or_default(),
            mime: header.inferred_mime(),
            data,
            trigger: header.trigger_time().unwrap_or(TriggerTime::Now),
            header,
        }
    }

    /// Build from a [`MotObject`].
    pub fn from_object(obj: MotObject) -> Self {
        Self::from_mot(obj.transport_id, obj.header, obj.body)
    }

    /// SlideShow CategoryID/SlideID, if signalled.
    pub fn category_slide_id(&self) -> Option<(u8, u8)> {
        self.header.sls_category_slide_id()
    }

    /// SlideShow ClickThroughURL, if signalled.
    pub fn click_through_url(&self) -> Option<String> {
        self.header.sls_click_through_url()
    }

    fn due_at(&self) -> Option<i64> {
        match self.trigger {
            TriggerTime::Now => None,
            TriggerTime::At(t) => Some(t.to_unix()),
        }
    }
}

/// Default number of slides kept for browsing.
pub const DEFAULT_HISTORY: usize = 32;

/// Receiver-side slide show state.
#[derive(Debug, Clone)]
pub struct SlideShow {
    history: VecDeque<Slide>,
    pending: Vec<Slide>,
    capacity: usize,
    /// `None` = follow the newest slide; `Some(i)` = the user selected history[i].
    selected: Option<usize>,
}

impl Default for SlideShow {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY)
    }
}

impl SlideShow {
    /// Keep at most `capacity` slides (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            history: VecDeque::new(),
            pending: Vec::new(),
            capacity: capacity.max(1),
            selected: None,
        }
    }

    /// Feed a decoder event; [`DataEvent::SlideShowImage`] events are added with
    /// [`Self::push`], others are ignored. Returns `true` if the displayed slide changed.
    pub fn apply(&mut self, event: &DataEvent, now_unix: Option<i64>) -> bool {
        match event {
            DataEvent::SlideShowImage {
                transport_id,
                data,
                header,
                ..
            } => self.push(
                Slide::from_mot(*transport_id, header.clone(), data.clone()),
                now_unix,
            ),
            _ => false,
        }
    }

    /// Add a received slide. `now_unix` is the current UTC time in seconds, if known
    /// (without it, slides are shown on reception). Returns `true` if the displayed
    /// slide changed.
    pub fn push(&mut self, slide: Slide, now_unix: Option<i64>) -> bool {
        match (slide.due_at(), now_unix) {
            (Some(due), Some(now)) if due > now => {
                self.pending.push(slide);
                self.pending.sort_by_key(|s| s.due_at());
                false
            }
            _ => self.show(slide),
        }
    }

    /// Advance the clock: slides whose TriggerTime has come are shown. Returns `true`
    /// if the displayed slide changed.
    pub fn tick(&mut self, now_unix: i64) -> bool {
        let mut changed = false;
        while self
            .pending
            .first()
            .is_some_and(|s| s.due_at().is_some_and(|due| due <= now_unix))
        {
            let slide = self.pending.remove(0);
            changed |= self.show(slide);
        }
        changed
    }

    fn show(&mut self, slide: Slide) -> bool {
        if !slide.name.is_empty()
            && let Some(pos) = self.history.iter().position(|s| s.name == slide.name)
        {
            self.history.remove(pos);
            self.selected = match self.selected {
                Some(sel) if pos < sel => Some(sel - 1),
                Some(sel) if pos == sel => None,
                other => other,
            };
        }
        self.history.push_back(slide);
        if self.history.len() > self.capacity {
            self.history.pop_front();
            self.selected = match self.selected {
                Some(0) | None => None,
                Some(sel) => Some(sel - 1),
            };
        }
        self.selected.is_none()
    }

    /// The slide to display.
    pub fn current(&self) -> Option<&Slide> {
        match self.selected {
            Some(i) => self.history.get(i),
            None => self.history.back(),
        }
    }

    /// Index of the displayed slide in [`Self::history`].
    pub fn current_index(&self) -> Option<usize> {
        match self.selected {
            Some(i) => Some(i),
            None => self.history.len().checked_sub(1),
        }
    }

    /// Received slides, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &Slide> {
        self.history.iter()
    }

    /// Slides waiting for their TriggerTime.
    pub fn pending(&self) -> &[Slide] {
        &self.pending
    }

    /// Number of slides in the history.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// `true` if no slide has been shown yet.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// `true` while new slides are shown as they arrive.
    pub fn is_live(&self) -> bool {
        self.selected.is_none()
    }

    /// Show history slide `index`; selecting the newest one resumes live display.
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.history.len() {
            return false;
        }
        self.selected = if index + 1 == self.history.len() {
            None
        } else {
            Some(index)
        };
        true
    }

    /// Step back in the history.
    pub fn step_back(&mut self) -> bool {
        match self.current_index() {
            Some(i) if i > 0 => self.select(i - 1),
            _ => false,
        }
    }

    /// Step forward in the history.
    pub fn step_forward(&mut self) -> bool {
        match self.current_index() {
            Some(i) => self.select(i + 1),
            None => false,
        }
    }

    /// Resume live display.
    pub fn live(&mut self) {
        self.selected = None;
    }

    /// Forget everything (e.g. on a service change).
    pub fn clear(&mut self) {
        self.history.clear();
        self.pending.clear();
        self.selected = None;
    }
}

/// Image formats TS 101 499 allows: (ContentSubType, MIME type), from the magic bytes
/// or else the file extension.
fn detect_image(name: &str, data: &[u8]) -> Option<(u16, &'static str)> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some((content_type::IMAGE_JFIF, "image/jpeg"));
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some((content_type::IMAGE_PNG, "image/png"));
    }
    match crate::mot::type_from_name(name) {
        (content_type::IMAGE, st @ (content_type::IMAGE_JFIF | content_type::IMAGE_PNG), mime) => {
            Some((st, mime))
        }
        _ => None,
    }
}

/// Transmitter: cycles through a list of JPEG/PNG slides.
#[derive(Debug, Clone)]
pub struct SlideShowFeeder {
    mot: MotEncoder,
}

impl Default for SlideShowFeeder {
    fn default() -> Self {
        Self::new()
    }
}

impl SlideShowFeeder {
    /// Empty feeder (header mode, fresh transport id per transmission).
    pub fn new() -> Self {
        Self {
            mot: MotEncoder::header_mode(),
        }
    }

    /// Load every `.jpg`/`.jpeg`/`.png` file of `dir` (not recursive), sorted by name.
    pub fn from_dir(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                        matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
                    })
            })
            .collect();
        paths.sort();
        let mut feeder = Self::new();
        for path in paths {
            let data = std::fs::read(&path)?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            feeder.add_image(&name, data).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })?;
        }
        Ok(feeder)
    }

    /// Add an image with ContentName `name` and TriggerTime "Now".
    pub fn add_image(&mut self, name: &str, data: Vec<u8>) -> Result<u16> {
        let (subtype, _) =
            detect_image(name, &data).ok_or(DataError::Unsupported("SlideShow image format"))?;
        let mut header = MotHeader::new(content_type::IMAGE, subtype, 0);
        header.set_content_name(name);
        header.set_trigger_time(TriggerTime::Now);
        self.mot.add_object(header, data)
    }

    /// Add an image with a caller-built header (for CategoryID/SlideID, ClickThroughURL,
    /// timed triggers...).
    pub fn add_image_with_header(&mut self, header: MotHeader, data: Vec<u8>) -> Result<u16> {
        self.mot.add_object(header, data)
    }

    /// Segment size of the MOT data groups.
    pub fn set_segment_size(&mut self, size: usize) -> Result<()> {
        self.mot.set_segment_size(size)
    }

    /// Number of slides in the cycle.
    pub fn len(&self) -> usize {
        self.mot.objects().len()
    }

    /// `true` if there are no slides.
    pub fn is_empty(&self) -> bool {
        self.mot.objects().is_empty()
    }

    /// Remove all slides.
    pub fn clear(&mut self) {
        self.mot.clear();
    }

    /// Slides transmitted (or started) so far.
    pub fn transmissions(&self) -> u64 {
        self.mot.transmissions()
    }

    /// The underlying MOT encoder.
    pub fn encoder_mut(&mut self) -> &mut MotEncoder {
        &mut self.mot
    }
}

impl DataUnitSource for SlideShowFeeder {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        self.mot.next_data_group()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::MotTime;

    fn slide(name: &str, trigger: TriggerTime) -> Slide {
        let mut h = MotHeader::new(content_type::IMAGE, content_type::IMAGE_PNG, 3);
        h.set_content_name(name);
        h.set_trigger_time(trigger);
        Slide::from_mot(1, h, vec![1, 2, 3])
    }

    #[test]
    fn live_display_and_browsing() {
        let mut ss = SlideShow::new(3);
        assert!(ss.push(slide("a", TriggerTime::Now), None));
        assert!(ss.push(slide("b", TriggerTime::Now), None));
        assert_eq!(ss.current().unwrap().name, "b");
        assert!(ss.step_back());
        assert!(!ss.is_live());
        assert!(!ss.push(slide("c", TriggerTime::Now), None));
        assert_eq!(ss.current().unwrap().name, "a");
        assert!(ss.step_forward() && ss.step_forward());
        assert!(ss.is_live());
        assert_eq!(ss.current().unwrap().name, "c");
        // Same name replaces the old copy; capacity is respected.
        ss.push(slide("a", TriggerTime::Now), None);
        ss.push(slide("d", TriggerTime::Now), None);
        let names: Vec<_> = ss.history().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "d"]);
    }

    #[test]
    fn trigger_times() {
        let mut ss = SlideShow::default();
        let t0 = MotTime::from_ymd_hms(2026, 9, 29, 12, 0, 0);
        let later = TriggerTime::At(MotTime::from_unix(t0.to_unix() + 60));
        assert!(!ss.push(slide("future", later), Some(t0.to_unix())));
        assert!(ss.current().is_none());
        assert_eq!(ss.pending().len(), 1);
        assert!(!ss.tick(t0.to_unix() + 59));
        assert!(ss.tick(t0.to_unix() + 60));
        assert_eq!(ss.current().unwrap().name, "future");
        // Past trigger times and unknown clocks show immediately.
        assert!(ss.push(slide("past", TriggerTime::At(t0)), Some(t0.to_unix() + 5)));
        assert!(ss.push(slide("noclock", later), None));
    }

    #[test]
    fn feeder_rejects_non_images() {
        let mut f = SlideShowFeeder::new();
        assert!(
            f.add_image("x.png", vec![0x89, b'P', b'N', b'G', 0, 0])
                .is_ok()
        );
        assert!(f.add_image("y.jpg", vec![0xFF, 0xD8, 0xFF, 0xE0]).is_ok());
        assert!(f.add_image("z.gif", b"GIF89a".to_vec()).is_err());
        assert_eq!(f.len(), 2);
    }
}
