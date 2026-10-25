//! RSS and Atom feeds as Journaline pages (`[service.data] type = "journaline"` with
//! `feeds = [...]`): fetched on a schedule over DecDRM's own HTTP client
//! ([`crate::webstream`]'s), turned into a page tree, and loaded into the carousel as a
//! page file is ([`crate::data::JournalineWatch`]), so changes go out at once.
//!
//! The page tree is the one a recording of another DecDRM build's RSS feature shows
//! (see the milestone *RSS feeds as Journaline pages* in `docs/DESIGN.md`):
//!
//! ```text
//! 0          menu "News feeds": one entry per feed (its title) → the feed's menu
//! 1          menu <feed title>: one entry per item (its title) → the item's page
//! 2 … n+1    plain text <item title>: description, an empty line, the item's link
//! n+2        the next feed's menu, then its items, …
//! ```
//!
//! Object ids are given out in that order on every fetch, so an item's page keeps its
//! id while it keeps its place in the feed; a page whose content changed gets the next
//! revision. An item with neither description nor link is a title-only page.
//!
//! Text: the feed's characters as they are (UTF-8; a document in ISO-8859-1 or
//! windows-1252 is converted), entities decoded, HTML tags removed (`<br>` and block
//! ends become line breaks in descriptions), white space collapsed. Pages are kept
//! within NML's 4092 bytes by shortening the description (ending it with "…"), and a
//! feed's menu by leaving out its last items.

use crate::webstream::http::{self, HttpError, Request, Url};
use decdrm_data::journaline::{MenuItem, NML_MAX_LEN, NmlObject, ROOT_OBJECT_ID};
use quick_xml::events::{BytesStart, Event};
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// Default seconds between fetches.
pub const DEFAULT_REFRESH_S: u32 = 600;
/// Fewest seconds between fetches allowed in a station file.
pub const MIN_REFRESH_S: u32 = 60;
/// Default number of items per feed (the recording's feeds had 10 each).
pub const DEFAULT_ITEMS: usize = 10;
/// Most items per feed.
pub const MAX_ITEMS: usize = 50;
/// Most feeds of one application.
pub const MAX_FEEDS: usize = 20;
/// Default title of the root menu.
pub const DEFAULT_ROOT_TITLE: &str = "News feeds";
/// Largest feed document read.
const MAX_FEED_BYTES: u64 = 4 << 20;
/// Longest title or menu entry, bytes.
const MAX_TITLE_BYTES: usize = 200;

/// One feed, as parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Feed {
    /// The channel's (RSS) or feed's (Atom) title.
    pub title: String,
    pub items: Vec<FeedItem>,
}

/// One item (RSS) or entry (Atom).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeedItem {
    pub title: String,
    /// The description (RSS) or summary (Atom), else the full content; `\n` between
    /// paragraphs.
    pub description: String,
    pub link: String,
}

// ---------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------

/// The document as text: UTF-8 (a byte-order mark removed), or ISO-8859-1 or
/// windows-1252 when its XML declaration says so. Anything not UTF-8 otherwise is read
/// as windows-1252, which never fails.
fn document_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).to_ascii_lowercase();
    let declared_single_byte = head.starts_with("<?xml")
        && head.split("?>").next().is_some_and(|decl| {
            ["iso-8859-1", "latin1", "latin-1", "windows-1252", "cp1252", "us-ascii"].iter().any(|e| decl.contains(e))
        });
    match std::str::from_utf8(bytes) {
        Ok(s) if !declared_single_byte => s.to_string(),
        _ => bytes.iter().map(|&b| windows_1252(b)).collect(),
    }
}

/// A windows-1252 byte as a character (ISO-8859-1 except for 0x80–0x9F, which are
/// control characters in ISO-8859-1 and never meant as such in a feed).
fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

/// The fields of an item that are read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    FeedTitle,
    Title,
    Description,
    Content,
    Link,
    Guid,
}

/// Which field an element directly inside an item (or the channel/feed) is: plain or
/// Atom names only, so `media:title`, `dc:title`… of other vocabularies are not taken
/// for the item's own.
fn field_of(e: &BytesStart, in_item: bool) -> Option<Field> {
    let name = e.name();
    let local = name.local_name();
    let prefix = name.prefix().map(|p| p.as_ref().to_ascii_lowercase());
    let plain = matches!(prefix.as_deref(), None | Some(b"atom") | Some(b"rss"));
    let field = match (local.as_ref(), in_item) {
        (b"title", false) if plain => Field::FeedTitle,
        (b"title", true) if plain => Field::Title,
        (b"description" | b"summary", true) if plain => Field::Description,
        (b"content", true) if plain => Field::Content,
        (b"encoded", true) if prefix.as_deref() == Some(b"content") => Field::Content,
        (b"link", true) if plain => Field::Link,
        (b"guid", true) if plain => Field::Guid,
        _ => return None,
    };
    Some(field)
}

/// An attribute's value, entities decoded.
fn attribute(e: &BytesStart, name: &[u8]) -> Option<String> {
    let a = e.try_get_attribute(name).ok().flatten()?;
    Some(a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok()?.into_owned())
}

/// An Atom `<link href="…">` that is the entry's own page (no `rel`, or `alternate`).
fn atom_link(e: &BytesStart) -> Option<String> {
    let rel = attribute(e, b"rel");
    if rel.as_deref().is_some_and(|r| r != "alternate") {
        return None;
    }
    attribute(e, b"href").filter(|h| !h.trim().is_empty())
}

/// An item while it is read.
#[derive(Default)]
struct ItemParts {
    title: String,
    description: String,
    content: String,
    link: String,
    guid: String,
    guid_permalink: bool,
}

impl ItemParts {
    fn finish(self) -> Option<FeedItem> {
        let description = html_to_text(if self.description.trim().is_empty() { &self.content } else { &self.description }, true);
        let mut link = self.link.trim().to_string();
        let guid = self.guid.trim();
        if link.is_empty() && self.guid_permalink && (guid.starts_with("http://") || guid.starts_with("https://")) {
            link = guid.to_string();
        }
        let mut title = html_to_text(&self.title, false);
        if title.is_empty() {
            // No title: the start of the description, else the link.
            title = clip(description.lines().next().unwrap_or(""), 80);
            if title.is_empty() {
                title = link.clone();
            }
        }
        (!title.is_empty()).then_some(FeedItem { title, description, link })
    }
}

/// Parse an RSS 2.0, RSS 1.0 (RDF) or Atom document. At most `max_items` items are
/// kept, the first ones in the document.
pub fn parse_feed(bytes: &[u8], max_items: usize) -> Result<Feed, String> {
    let text = document_text(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    // Feeds in the wild are not always well-formed; take what can be read.
    reader.config_mut().check_end_names = false;
    let mut feed = Feed::default();
    let mut recognised = false;
    // Element depth; the depth of the open item; the field being read and its depth.
    let mut depth = 0usize;
    let mut item: Option<(usize, ItemParts)> = None;
    let mut capture: Option<(Field, usize, String)> = None;
    let mut title_taken = false;
    loop {
        let event = match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(ev) => ev,
            Err(e) => {
                if recognised {
                    break; // keep what came before the damage
                }
                return Err(format!("not a feed: {e}"));
            }
        };
        match event {
            Event::Start(e) => {
                depth += 1;
                let local = e.name().local_name().as_ref().to_ascii_lowercase();
                if depth == 1 {
                    recognised = matches!(local.as_slice(), b"rss" | b"rdf" | b"feed");
                    if !recognised {
                        return Err("not an RSS or Atom feed".into());
                    }
                }
                if capture.is_some() {
                    continue; // markup inside a field (Atom XHTML content): its text counts
                }
                let item_depth = item.as_ref().map(|(d, _)| *d);
                if item_depth.is_none() && matches!(local.as_slice(), b"item" | b"entry") {
                    item = Some((depth, ItemParts::default()));
                    continue;
                }
                let direct_child = match item_depth {
                    Some(d) => depth == d + 1,
                    // The channel's title (RSS: <rss><channel><title>, RDF the same) or
                    // the feed's (Atom: <feed><title>).
                    None => depth <= 3,
                };
                if !direct_child {
                    continue;
                }
                if let Some(field) = field_of(&e, item_depth.is_some()) {
                    if field == Field::FeedTitle && title_taken {
                        continue;
                    }
                    if field == Field::Link
                        && let Some((_, parts)) = &mut item
                        && let Some(href) = atom_link(&e)
                    {
                        if parts.link.is_empty() {
                            parts.link = href;
                        }
                        continue;
                    }
                    if field == Field::Guid
                        && let Some((_, parts)) = &mut item
                    {
                        parts.guid_permalink = attribute(&e, b"isPermaLink").is_none_or(|v| v.trim() != "false");
                    }
                    capture = Some((field, depth, String::new()));
                }
            }
            Event::Empty(e) => {
                if capture.is_none()
                    && let Some((d, parts)) = &mut item
                    && depth == *d
                    && field_of(&e, true) == Some(Field::Link)
                    && parts.link.is_empty()
                    && let Some(href) = atom_link(&e)
                {
                    parts.link = href;
                }
            }
            Event::Text(t) => {
                if let Some((_, _, s)) = &mut capture {
                    s.push_str(&t.decode().map_err(|e| e.to_string())?);
                }
            }
            Event::CData(t) => {
                if let Some((_, _, s)) = &mut capture {
                    s.push_str(&t.decode().map_err(|e| e.to_string())?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some((_, _, s)) = &mut capture {
                    match r.resolve_char_ref() {
                        Ok(Some(c)) => s.push(c),
                        _ => {
                            let name = r.decode().map_err(|e| e.to_string())?;
                            match xml_entity(&name) {
                                Some(c) => s.push(c),
                                // An HTML entity: decoded with the HTML.
                                None => {
                                    s.push('&');
                                    s.push_str(&name);
                                    s.push(';');
                                }
                            }
                        }
                    }
                }
            }
            Event::End(_) => {
                if let Some((field, _, s)) = capture.take_if(|(_, d, _)| *d == depth) {
                    match (field, &mut item) {
                        (Field::FeedTitle, _) => {
                            feed.title = html_to_text(&s, false);
                            title_taken = true;
                        }
                        (Field::Title, Some((_, p))) => p.title = s,
                        (Field::Description, Some((_, p))) => p.description = s,
                        (Field::Content, Some((_, p))) => p.content = s,
                        (Field::Link, Some((_, p))) => {
                            if p.link.is_empty() {
                                p.link = s;
                            }
                        }
                        (Field::Guid, Some((_, p))) => p.guid = s,
                        _ => {}
                    }
                }
                if item.as_ref().is_some_and(|(d, _)| *d == depth)
                    && let Some((_, parts)) = item.take()
                    && feed.items.len() < max_items
                    && let Some(done) = parts.finish()
                {
                    feed.items.push(done);
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    if !recognised {
        return Err("not an RSS or Atom feed".into());
    }
    Ok(feed)
}

/// The five entities XML defines.
fn xml_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

/// The HTML entities feeds use most (beyond XML's own and numeric ones).
fn html_entity(name: &str) -> Option<char> {
    if let Some(c) = xml_entity(name) {
        return Some(c);
    }
    if let Some(num) = name.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(n);
    }
    Some(match name {
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "hellip" => '…',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "deg" => '°',
        "times" => '×',
        "eacute" => 'é',
        "egrave" => 'è',
        "aacute" => 'á',
        "agrave" => 'à',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        _ => return None,
    })
}

/// Text from a field that may hold HTML: tags removed (in `multiline` text, `<br>` and
/// the ends of paragraphs, list items and headings become line breaks), entities
/// decoded, control characters dropped, white space collapsed to single spaces and
/// empty lines dropped.
pub fn html_to_text(s: &str, multiline: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let next = rest[1..].chars().next();
            if next.is_some_and(|n| n.is_ascii_alphabetic() || n == '/' || n == '!')
                && let Some(end) = rest.find('>')
            {
                let tag = rest[1..end].trim_start_matches('/').to_ascii_lowercase();
                let name: String = tag.chars().take_while(char::is_ascii_alphanumeric).collect();
                if matches!(name.as_str(), "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote") {
                    out.push('\n');
                }
                rest = &rest[end + 1..];
                continue;
            }
        } else if c == '&'
            && let Some(end) = rest.char_indices().take(12).find(|&(_, c)| c == ';').map(|(i, _)| i)
            && let Some(ch) = html_entity(&rest[1..end])
        {
            out.push(ch);
            rest = &rest[end + 1..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    let lines = out.split('\n').map(|line| {
        line.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
    });
    let lines: Vec<String> = lines.filter(|l| !l.is_empty()).collect();
    lines.join(if multiline { "\n" } else { " " })
}

/// `s` in at most `max` bytes: cut at a character boundary and ended with "…" when it
/// is longer.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s[..end].trim_end())
}

// ---------------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------------

/// Bytes of an NML object before its title and body: header and the title code.
const NML_OVERHEAD: usize = 4;

/// The page tree of `feeds` (those not loaded yet are left out), see the module
/// documentation.
pub fn feed_pages(root_title: &str, feeds: &[Option<Feed>]) -> Vec<NmlObject> {
    let mut pages = Vec::new();
    let mut root = Vec::new();
    let mut next_id = ROOT_OBJECT_ID + 1;
    for feed in feeds.iter().flatten() {
        let menu_id = next_id;
        let title = clip(&feed.title, MAX_TITLE_BYTES);
        // Leave out items while the menu would be too long.
        let entries: Vec<String> = feed.items.iter().map(|i| clip(&i.title, MAX_TITLE_BYTES)).collect();
        let mut count = entries.len();
        while count > 0 && NML_OVERHEAD + title.len() + entries[..count].iter().map(|e| 3 + e.len()).sum::<usize>() > NML_MAX_LEN {
            count -= 1;
        }
        let first = menu_id + 1;
        let items: Vec<MenuItem> =
            entries[..count].iter().enumerate().map(|(k, text)| MenuItem::new(first + k as u16, text.clone())).collect();
        root.push(MenuItem::new(menu_id, title.clone()));
        pages.push(NmlObject::menu(menu_id, title, items));
        for (k, item) in feed.items[..count].iter().enumerate() {
            pages.push(item_page(first + k as u16, item));
        }
        next_id = first + count as u16;
    }
    let root_title = clip(root_title, MAX_TITLE_BYTES);
    // The root's entries are at most MAX_FEEDS titles of MAX_TITLE_BYTES: it fits.
    pages.insert(0, NmlObject::menu(ROOT_OBJECT_ID, root_title, root));
    pages
}

/// An item's page: its description and link, shortened to fit NML's size limit.
fn item_page(id: u16, item: &FeedItem) -> NmlObject {
    let title = clip(&item.title, MAX_TITLE_BYTES);
    let link = if item.link.len() > 1000 { String::new() } else { item.link.clone() };
    if item.description.is_empty() && link.is_empty() {
        return NmlObject::title_only(id, title);
    }
    let tail = if link.is_empty() {
        String::new()
    } else if item.description.is_empty() {
        link
    } else {
        format!("\n\n{link}")
    };
    // Header, title, the body code, then the text.
    let room = NML_MAX_LEN - (NML_OVERHEAD + title.len() + 1 + tail.len());
    let text = format!("{}{tail}", clip(&item.description, room));
    NmlObject::plain_text(id, title, text)
}

// ---------------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------------

/// Fetch and parse the feed at `url`.
pub fn fetch_feed(url: &Url, max_items: usize, stop: &Arc<AtomicBool>) -> Result<Feed, String> {
    let req = Request {
        stop: stop.clone(),
        tls: None,
        icy_metadata: false,
        connect_timeout: Duration::from_secs(10),
        io_timeout: Duration::from_secs(20),
    };
    let resp = http::get(url, &req).map_err(|e| match e {
        HttpError::Stopped => "stopped".to_string(),
        e => e.to_string(),
    })?;
    let mut body = Vec::new();
    resp.body
        .take(MAX_FEED_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("{url}: {}", http::describe_io(&e)))?;
    if body.len() as u64 > MAX_FEED_BYTES {
        return Err(format!("{url}: the feed is larger than {} MiB", MAX_FEED_BYTES >> 20));
    }
    parse_feed(&body, max_items).map_err(|e| format!("{url}: {e}"))
}

/// The feeds of one Journaline application, fetched again every `refresh` on a thread
/// of their own (a slow server never holds up the frame loop): the first fetch happens
/// in [`FeedWatch::start`], the later ones' results are taken with
/// [`FeedWatch::poll`]. A feed that cannot be fetched keeps the items it had.
pub struct FeedWatch {
    pub urls: Vec<String>,
    /// The last good content of each feed.
    feeds: Vec<Option<Feed>>,
    /// The last fetch's problem with each feed.
    errors: Vec<Option<String>>,
    results: mpsc::Receiver<Vec<Result<Feed, String>>>,
    /// Asks the thread to fetch at once; dropping it ends the thread.
    now: Option<mpsc::Sender<()>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FeedWatch {
    /// Fetch every feed once (here, so the station starts with the pages), then start
    /// the thread that fetches them again every `refresh`. Fails if no feed could be
    /// fetched; a failed one is tried again with the others.
    pub fn start(urls: &[String], max_items: usize, refresh: Duration) -> Result<FeedWatch, String> {
        let parsed: Vec<Url> = urls.iter().map(|u| Url::parse(u.trim())).collect::<Result<_, _>>()?;
        let stop = Arc::new(AtomicBool::new(false));
        let first: Vec<Result<Feed, String>> = parsed.iter().map(|u| fetch_feed(u, max_items, &stop)).collect();
        if first.iter().all(Result::is_err) {
            let errors: Vec<String> = first.into_iter().filter_map(Result::err).collect();
            return Err(format!("no feed could be fetched: {}", errors.join("; ")));
        }
        let (results_tx, results) = mpsc::channel();
        let (now, now_rx) = mpsc::channel::<()>();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("decdrm-feeds".into())
            .spawn(move || {
                loop {
                    // A request to fetch now, the refresh time, or the watch dropped.
                    match now_rx.recv_timeout(refresh) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    while now_rx.try_recv().is_ok() {}
                    let batch: Vec<_> = parsed.iter().map(|u| fetch_feed(u, max_items, &thread_stop)).collect();
                    if thread_stop.load(Ordering::Relaxed) || results_tx.send(batch).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| format!("cannot start the feed thread: {e}"))?;
        let mut watch = FeedWatch {
            urls: urls.to_vec(),
            feeds: vec![None; urls.len()],
            errors: vec![None; urls.len()],
            results,
            now: Some(now),
            stop,
            thread: Some(thread),
        };
        watch.take(first);
        Ok(watch)
    }

    /// Keep a fetch's results: new content replaces a feed's, an error keeps it.
    fn take(&mut self, batch: Vec<Result<Feed, String>>) {
        for (k, r) in batch.into_iter().enumerate() {
            match r {
                Ok(feed) => {
                    self.feeds[k] = Some(feed);
                    self.errors[k] = None;
                }
                Err(e) => self.errors[k] = Some(e),
            }
        }
    }

    /// The results of a fetch that has finished since the last call, if any (the
    /// feeds are then up to date with it).
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok(batch) = self.results.try_recv() {
            self.take(batch);
            any = true;
        }
        any
    }

    /// Fetch every feed now (the result comes through [`Self::poll`]).
    pub fn fetch_now(&self) {
        if let Some(now) = &self.now {
            let _ = now.send(());
        }
    }

    /// The feeds as last fetched successfully (`None`: never).
    pub fn feeds(&self) -> &[Option<Feed>] {
        &self.feeds
    }

    /// The last fetch's problem with each feed.
    pub fn errors(&self) -> &[Option<String>] {
        &self.errors
    }
}

impl Drop for FeedWatch {
    fn drop(&mut self) {
        // A request in progress sees the stop flag within 100 ms; a waiting thread
        // sees its channel closed at once.
        self.stop.store(true, Ordering::Relaxed);
        self.now = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A feed server on 127.0.0.1 for tests (no internet in tests).
#[cfg(test)]
pub(crate) mod test_server {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

    /// Serves each path's status and body (404 for others), one request per
    /// connection; it runs until the test process ends.
    pub struct FeedServer {
        addr: SocketAddr,
        routes: Routes,
        hits: Arc<AtomicUsize>,
    }

    impl FeedServer {
        pub fn start() -> FeedServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let routes: Routes = Arc::default();
            let hits = Arc::new(AtomicUsize::new(0));
            let (r, h) = (routes.clone(), hits.clone());
            std::thread::spawn(move || {
                for mut conn in listener.incoming().flatten() {
                    let mut head = Vec::new();
                    let mut byte = [0u8];
                    while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).is_ok_and(|n| n == 1) {
                        head.push(byte[0]);
                    }
                    let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
                    let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                    h.fetch_add(1, Ordering::SeqCst);
                    let (code, body) = r.lock().unwrap().get(&path).cloned().unwrap_or((404, b"not here".to_vec()));
                    let head = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = conn.write_all(head.as_bytes()).and_then(|()| conn.write_all(&body));
                }
            });
            FeedServer { addr, routes, hits }
        }

        /// Serve `body` with `code` at `path`.
        pub fn set(&self, path: &str, code: u16, body: &str) {
            self.routes.lock().unwrap().insert(path.to_string(), (code, body.as_bytes().to_vec()));
        }

        pub fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        /// Requests answered so far.
        pub fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    /// An RSS 2.0 document with `items` (title, description, link).
    pub fn rss(title: &str, items: &[(&str, &str, &str)]) -> String {
        let items: String = items
            .iter()
            .map(|(t, d, l)| format!("<item><title>{t}</title><description><![CDATA[{d}]]></description><link>{l}</link></item>"))
            .collect();
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><rss version=\"2.0\"><channel><title>{title}</title>{items}</channel></rss>")
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{FeedServer, rss};
    use super::*;
    use std::time::Instant;

    /// Wait (at most 10 s) until the watch has the results of another fetch.
    fn next_fetch(w: &mut FeedWatch) {
        let t0 = Instant::now();
        while !w.poll() {
            assert!(t0.elapsed() < Duration::from_secs(10), "no fetch");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The first fetch at start, then one per refresh interval; a feed that fails
    /// keeps its items; a fetch on request; nothing to fetch fails the start.
    #[test]
    fn watch_fetches_on_a_schedule() {
        let server = FeedServer::start();
        server.set("/a.xml", 200, &rss("A", &[("a1", "first", "https://a/1")]));
        let urls = [server.url("/a.xml"), server.url("/missing.xml")];
        let mut w = FeedWatch::start(&urls, 10, Duration::from_millis(300)).unwrap();
        assert_eq!(w.feeds()[0].as_ref().unwrap().items[0].description, "first");
        assert!(w.feeds()[1].is_none());
        assert!(w.errors()[1].as_deref().is_some_and(|e| e.contains("HTTP 404")), "{:?}", w.errors());
        assert!(!w.poll(), "nothing new right after the start");

        server.set("/a.xml", 200, &rss("A", &[("a1", "second", "https://a/1"), ("a2", "", "")]));
        server.set("/missing.xml", 200, &rss("B", &[]));
        next_fetch(&mut w);
        assert_eq!(w.feeds()[0].as_ref().unwrap().items.len(), 2);
        assert_eq!(w.feeds()[1].as_ref().unwrap().title, "B");
        assert_eq!(w.errors(), [None, None]);

        server.set("/a.xml", 500, "broken");
        next_fetch(&mut w);
        assert_eq!(w.feeds()[0].as_ref().unwrap().items.len(), 2, "kept");
        assert!(w.errors()[0].is_some());
        drop(w);

        // A long refresh: only the start's fetch and the one asked for.
        server.set("/a.xml", 200, &rss("A", &[]));
        let mut w = FeedWatch::start(&urls[..1], 10, Duration::from_secs(3600)).unwrap();
        let hits = server.hits();
        w.fetch_now();
        next_fetch(&mut w);
        assert_eq!(server.hits(), hits + 1);

        let none = FeedWatch::start(&[server.url("/nothing")], 10, Duration::from_secs(60));
        assert!(none.is_err_and(|e| e.starts_with("no feed could be fetched")));
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title><![CDATA[Test News]]></title>
    <image><title>Logo title</title><url>https://example.org/logo.png</url></image>
    <item>
      <title><![CDATA[First &amp; foremost]]></title>
      <media:title>Not this one</media:title>
      <description><![CDATA[<p>Some <b>bold</b> text.</p><p>Second&nbsp;paragraph</p>]]></description>
      <link>https://example.org/1?a=1&amp;b=2</link>
    </item>
    <item>
      <title>Quotes &#8220;like this&#8221; &#x2014; and caf&#233;</title>
      <guid isPermaLink="true">https://example.org/2</guid>
    </item>
    <item>
      <title>Content only</title>
      <content:encoded>&lt;div&gt;From content&lt;/div&gt;</content:encoded>
      <guid isPermaLink="false">tag:example.org,2026:3</guid>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn rss_items() {
        let feed = parse_feed(RSS.as_bytes(), 10).unwrap();
        assert_eq!(feed.title, "Test News");
        let items: Vec<(&str, &str, &str)> =
            feed.items.iter().map(|i| (i.title.as_str(), i.description.as_str(), i.link.as_str())).collect();
        assert_eq!(
            items,
            [
                ("First & foremost", "Some bold text.\nSecond paragraph", "https://example.org/1?a=1&b=2"),
                ("Quotes “like this” — and café", "", "https://example.org/2"),
                ("Content only", "From content", ""),
            ]
        );
        assert_eq!(parse_feed(RSS.as_bytes(), 2).unwrap().items.len(), 2);
    }

    #[test]
    fn atom_and_rdf() {
        let atom = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Atom feed</title>
  <link rel="self" href="https://example.org/feed"/>
  <entry>
    <title type="html">Entry &lt;i&gt;one&lt;/i&gt;</title>
    <link rel="enclosure" href="https://example.org/a.mp3"/>
    <link href="https://example.org/e1"/>
    <summary>Short</summary>
    <content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml">Long <b>content</b></div></content>
  </entry>
  <entry>
    <title>Entry two</title>
    <content type="html">&lt;p&gt;Only content&lt;/p&gt;</content>
  </entry>
</feed>"#;
        let feed = parse_feed(atom.as_bytes(), 10).unwrap();
        assert_eq!(feed.title, "Atom feed");
        assert_eq!(feed.items[0], FeedItem { title: "Entry one".into(), description: "Short".into(), link: "https://example.org/e1".into() });
        assert_eq!(feed.items[1].description, "Only content");

        let rdf = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/">
  <channel rdf:about="x"><title>RDF feed</title></channel>
  <item rdf:about="https://example.org/r1"><title>R1</title><link>https://example.org/r1</link><description>D1</description></item>
</rdf:RDF>"#;
        let feed = parse_feed(rdf.as_bytes(), 10).unwrap();
        assert_eq!(feed.title, "RDF feed");
        assert_eq!(feed.items, [FeedItem { title: "R1".into(), description: "D1".into(), link: "https://example.org/r1".into() }]);
    }

    #[test]
    fn single_byte_encodings_and_errors() {
        let mut doc = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss><channel><title>Caf\xe9 \x93news\x94</title></channel></rss>".to_vec();
        assert_eq!(parse_feed(&doc, 10).unwrap().title, "Café “news”");
        // Not UTF-8 and nothing declared: windows-1252 as well.
        doc = b"<rss><channel><title>Caf\xe9</title></channel></rss>".to_vec();
        assert_eq!(parse_feed(&doc, 10).unwrap().title, "Café");
        assert!(parse_feed(b"<html><body>no</body></html>", 10).is_err());
        assert!(parse_feed(b"plain text", 10).is_err());
        // Damage after the first item: what came before is kept.
        let cut = "<rss><channel><title>T</title><item><title>A</title></item><item><title>B<";
        assert_eq!(parse_feed(cut.as_bytes(), 10).unwrap().items.len(), 1);
    }

    #[test]
    fn text_cleanup() {
        assert_eq!(html_to_text("  a \t b\r\n\n c ", false), "a b c");
        assert_eq!(html_to_text("a<br/>b<br>c", true), "a\nb\nc");
        assert_eq!(html_to_text("x < y &amp; z", false), "x < y & z");
        assert_eq!(html_to_text("&unknown; &#65;", false), "&unknown; A");
        assert_eq!(html_to_text("bell\u{7} char", false), "bell char");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("ééééé", 8), "éé…");
    }

    fn item(title: &str, description: &str, link: &str) -> FeedItem {
        FeedItem { title: title.into(), description: description.into(), link: link.into() }
    }

    /// The tree of the recording: root, then each feed's menu followed by its items.
    #[test]
    fn page_tree_as_in_the_recording() {
        let a = Feed { title: "A".into(), items: vec![item("a1", "d1", "https://a/1"), item("a2", "", "https://a/2")] };
        let b = Feed { title: "B".into(), items: vec![item("b1", "only text", ""), item("b2", "", "")] };
        let pages = feed_pages(DEFAULT_ROOT_TITLE, &[Some(a), None, Some(b)]);
        let ids: Vec<u16> = pages.iter().map(|p| p.object_id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(pages[0], NmlObject::menu(0, "News feeds", vec![MenuItem::new(1, "A"), MenuItem::new(4, "B")]));
        assert_eq!(pages[1], NmlObject::menu(1, "A", vec![MenuItem::new(2, "a1"), MenuItem::new(3, "a2")]));
        assert_eq!(pages[2], NmlObject::plain_text(2, "a1", "d1\n\nhttps://a/1"));
        assert_eq!(pages[3], NmlObject::plain_text(3, "a2", "https://a/2"));
        assert_eq!(pages[5], NmlObject::plain_text(5, "b1", "only text"));
        assert_eq!(pages[6], NmlObject::title_only(6, "b2"));
        // The bytes of a page are those of the recording: 0x01 title 0x03 text 0x10 0x10 link.
        let bytes = pages[2].to_bytes(false).unwrap();
        assert_eq!(&bytes[3..], b"\x01a1\x03d1\x10\x10https://a/1");
    }

    #[test]
    fn pages_fit_nml() {
        let long = "word ".repeat(2000);
        let many: Vec<FeedItem> = (0..MAX_ITEMS).map(|k| item(&format!("{k} {}", "t".repeat(300)), &long, "https://x/")).collect();
        let pages = feed_pages(&"R".repeat(500), &[Some(Feed { title: "F".repeat(500), items: many })]);
        for p in &pages {
            assert!(p.to_bytes(false).is_ok(), "page {} too long", p.object_id);
        }
        // The menu lost items to fit; their pages are not sent.
        let menu_entries = pages[1].links().len();
        assert!(menu_entries < MAX_ITEMS && pages.len() == 2 + menu_entries, "{menu_entries} entries, {} pages", pages.len());
        let decdrm_data::journaline::NmlBody::PlainText(text) = &pages[2].body else { panic!() };
        assert!(text.ends_with("…\n\nhttps://x/"), "{}", &text[text.len() - 30..]);
    }
}
