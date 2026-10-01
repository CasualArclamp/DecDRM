//! The audio bytes of a stream response: ICY metadata removed, the first bytes kept for
//! format detection, titles collected.
//!
//! ICY metadata (SHOUTCAST's protocol, also served by Icecast): when the request says
//! `Icy-MetaData: 1`, the server may answer `icy-metaint: N` and then insert a metadata
//! block after every N audio bytes — one length byte L followed by 16·L bytes of text
//! such as `StreamTitle='Artist - Title';StreamUrl='';`, padded with NULs (L = 0: no
//! change). The other `icy-*` headers name the station (`icy-name`), its genre and
//! bit rate (`icy-br`).

use super::http::{Body, text};
use std::io::{self, Read};

/// A stream body without its ICY metadata.
pub(crate) struct StreamInput {
    body: Body,
    /// Audio bytes between metadata blocks (`icy-metaint`).
    metaint: Option<usize>,
    /// Audio bytes until the next metadata block.
    until_meta: usize,
    /// Bytes read ahead for format detection, replayed first.
    replay: Vec<u8>,
    replay_pos: usize,
    /// The latest `StreamTitle`, and whether it changed since [`Self::take_title`].
    title: Option<String>,
    title_changed: bool,
    /// Audio bytes delivered (for the measured bit rate).
    pub bytes: u64,
}

impl StreamInput {
    pub fn new(body: Body, metaint: Option<usize>) -> Self {
        let metaint = metaint.filter(|&n| n > 0);
        StreamInput {
            body,
            metaint,
            until_meta: metaint.unwrap_or(0),
            replay: Vec::new(),
            replay_pos: 0,
            title: None,
            title_changed: false,
            bytes: 0,
        }
    }

    /// Read ahead until `enough(bytes so far)` says so, `max` bytes are buffered or the
    /// stream ends; the bytes are replayed by later reads. Returns the bytes read ahead.
    pub fn peek(&mut self, max: usize, mut enough: impl FnMut(&[u8]) -> bool) -> io::Result<&[u8]> {
        let mut chunk = [0u8; 4096];
        while self.replay.len() - self.replay_pos < max && !enough(&self.replay[self.replay_pos..]) {
            let want = chunk.len().min(max - (self.replay.len() - self.replay_pos));
            let n = self.read_stripped(&mut chunk[..want])?;
            if n == 0 {
                break;
            }
            self.replay.extend_from_slice(&chunk[..n]);
        }
        Ok(&self.replay[self.replay_pos..])
    }

    /// Read the whole (rest of the) body as text, at most `max` bytes (playlists).
    pub fn read_text(&mut self, max: usize) -> io::Result<String> {
        let mut bytes = Vec::new();
        self.by_ref().take(max as u64).read_to_end(&mut bytes)?;
        Ok(text(&bytes))
    }

    /// The new title, if `StreamTitle` changed since the last call (`Some("")`: the
    /// server cleared it).
    pub fn take_title(&mut self) -> Option<String> {
        if !self.title_changed {
            return None;
        }
        self.title_changed = false;
        Some(self.title.clone().unwrap_or_default())
    }

    /// Read audio bytes, consuming the metadata blocks in between.
    fn read_stripped(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(metaint) = self.metaint else { return self.body.read(buf) };
        loop {
            if self.until_meta > 0 {
                let n = buf.len().min(self.until_meta);
                let got = self.body.read(&mut buf[..n])?;
                self.until_meta -= got;
                return Ok(got);
            }
            let mut len = [0u8; 1];
            if self.body.read(&mut len)? == 0 {
                return Ok(0);
            }
            let mut meta = vec![0u8; 16 * usize::from(len[0])];
            self.body.read_exact(&mut meta)?;
            if let Some(title) = stream_title(&meta)
                && self.title.as_ref() != Some(&title)
            {
                self.title = Some(title);
                self.title_changed = true;
            }
            self.until_meta = metaint;
        }
    }
}

impl Read for StreamInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = if self.replay_pos < self.replay.len() {
            let n = buf.len().min(self.replay.len() - self.replay_pos);
            buf[..n].copy_from_slice(&self.replay[self.replay_pos..self.replay_pos + n]);
            self.replay_pos += n;
            if self.replay_pos == self.replay.len() {
                self.replay.clear();
                self.replay_pos = 0;
            }
            n
        } else {
            self.read_stripped(buf)?
        };
        self.bytes += n as u64;
        Ok(n)
    }
}

/// The `StreamTitle` of an ICY metadata block, trimmed (`Some("")` when it is empty),
/// or `None` if the block has none. Titles may contain apostrophes ("Guns N' Roses"),
/// so the value ends at the `';` that is followed by the next `key='` or by the end.
pub(crate) fn stream_title(block: &[u8]) -> Option<String> {
    let end = block.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let meta = text(&block[..end]);
    let lower = meta.to_ascii_lowercase();
    let start = lower.find("streamtitle='")? + "streamtitle='".len();
    let rest = &meta[start..];
    let mut value_end = None;
    for (i, _) in rest.match_indices("';") {
        let after = rest[i + 2..].trim_start();
        let next_key = after
            .find("='")
            .is_some_and(|k| k > 0 && after[..k].chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        if after.is_empty() || next_key {
            value_end = Some(i);
            break;
        }
    }
    let value = match value_end {
        Some(i) => &rest[..i],
        // No terminator: up to the last apostrophe, else everything.
        None => rest.rfind('\'').map_or(rest, |i| &rest[..i]),
    };
    Some(value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_titles() {
        let t = |s: &str| stream_title(s.as_bytes());
        assert_eq!(t("StreamTitle='Artist - Song';StreamUrl='';").as_deref(), Some("Artist - Song"));
        assert_eq!(t("StreamTitle='Guns N' Roses - Don't Cry';").as_deref(), Some("Guns N' Roses - Don't Cry"));
        assert_eq!(t("StreamTitle='a';b';StreamUrl='http://x';").as_deref(), Some("a';b"));
        assert_eq!(t("StreamTitle='';").as_deref(), Some(""));
        assert_eq!(t("StreamUrl='x';"), None);
        assert_eq!(t("streamtitle='lower';").as_deref(), Some("lower"));
        assert_eq!(t("StreamTitle='No end").as_deref(), Some("No end"));
        let mut padded = b"StreamTitle='Pad';".to_vec();
        padded.resize(32, 0);
        assert_eq!(stream_title(&padded).as_deref(), Some("Pad"));
        assert_eq!(stream_title(b"StreamTitle='Caf\xe9';").as_deref(), Some("Café"), "Latin-1");
    }
}
