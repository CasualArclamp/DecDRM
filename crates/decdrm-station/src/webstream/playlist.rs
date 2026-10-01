//! Playlists that point to a stream: M3U / M3U8 (a list of URLs, `#` lines are comments
//! or `#EXTINF` information) and PLS (`[playlist]` with `File1=…`). HLS playlists — M3U8
//! files listing media *segments* (`#EXT-X-…` tags) — are recognised and refused: the
//! station plays continuous streams only.

/// Content types of playlists.
pub(crate) fn is_playlist_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "audio/x-mpegurl"
            | "audio/mpegurl"
            | "application/x-mpegurl"
            | "application/mpegurl"
            | "application/vnd.apple.mpegurl"
            | "audio/x-scpls"
            | "audio/scpls"
            | "application/pls+xml"
            | "application/pls"
            | "application/x-scpls"
    )
}

/// Whether the URL path names a playlist file.
pub(crate) fn is_playlist_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or_default().to_ascii_lowercase();
    path.ends_with(".m3u") || path.ends_with(".m3u8") || path.ends_with(".pls")
}

/// Whether the first bytes of a body look like a playlist.
pub(crate) fn looks_like_playlist(head: &[u8]) -> bool {
    let start = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start: Vec<u8> = start.iter().copied().skip_while(u8::is_ascii_whitespace).take(16).collect();
    let lower = start.to_ascii_lowercase();
    lower.starts_with(b"#extm3u")
        || lower.starts_with(b"[playlist]")
        || lower.starts_with(b"http://")
        || lower.starts_with(b"https://")
}

/// The stream URLs a playlist lists, in order (possibly relative to the playlist's URL).
pub(crate) fn entries(text: &str) -> Result<Vec<String>, String> {
    let text = text.trim_start_matches('\u{FEFF}');
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.iter().any(|l| {
        let u = l.to_ascii_uppercase();
        u.starts_with("#EXT-X-TARGETDURATION") || u.starts_with("#EXT-X-STREAM-INF") || u.starts_with("#EXT-X-MEDIA-SEQUENCE")
    }) {
        return Err("this is an HLS playlist (segmented stream), which is not supported; use the station's \
                    Icecast/SHOUTCAST stream URL"
            .into());
    }
    let list: Vec<String> = if lines.first().is_some_and(|l| l.eq_ignore_ascii_case("[playlist]")) {
        // PLS: FileN=URL, in N order.
        let mut files: Vec<(u32, String)> = lines
            .iter()
            .filter_map(|l| {
                let (key, value) = l.split_once('=')?;
                let n = key.trim().to_ascii_lowercase().strip_prefix("file")?.parse().ok()?;
                Some((n, value.trim().to_string()))
            })
            .collect();
        files.sort_by_key(|(n, _)| *n);
        files.into_iter().map(|(_, url)| url).filter(|u| !u.is_empty()).collect()
    } else {
        lines.iter().filter(|l| !l.starts_with('#')).map(|l| l.to_string()).collect()
    };
    if list.is_empty() {
        return Err("the playlist lists no stream".into());
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u_and_pls() {
        let m3u = "#EXTM3U\r\n#EXTINF:-1,Radio\r\nhttp://host:8000/live\r\nhttp://backup/live\r\n";
        assert_eq!(entries(m3u).unwrap(), ["http://host:8000/live", "http://backup/live"]);
        assert_eq!(entries("\u{FEFF}stream.mp3\n").unwrap(), ["stream.mp3"]);
        let pls = "[playlist]\nNumberOfEntries=2\nFile2=http://b/\nTitle1=One\nFile1=http://a/\nVersion=2\n";
        assert_eq!(entries(pls).unwrap(), ["http://a/", "http://b/"]);
        assert!(entries("#EXTM3U\n#EXTINF:-1,x\n").unwrap_err().contains("no stream"));
        let hls = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nseg1.aac\n";
        assert!(entries(hls).unwrap_err().contains("HLS"));
        assert!(entries("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=64000\nlow.m3u8\n").unwrap_err().contains("HLS"));
    }

    #[test]
    fn detection() {
        assert!(looks_like_playlist(b"#EXTM3U\n"));
        assert!(looks_like_playlist(b"\r\n[Playlist]\n"));
        assert!(looks_like_playlist(b"http://host/stream\n"));
        assert!(!looks_like_playlist(b"\xFF\xFB\x90\x00"));
        assert!(is_playlist_path("/listen/radio.PLS?x=1"));
        assert!(!is_playlist_path("/live.mp3"));
        assert!(is_playlist_type("audio/x-mpegurl"));
    }
}
