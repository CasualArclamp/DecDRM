//! Object cache and menu navigation for a Journaline viewer.
//!
//! Plays the role of the Fraunhofer news service decoder's object store plus Dream's
//! `CJournaline::GetNews` link resolution: pages are stored by object id, navigation
//! starts at the root menu (0x0000), and menu links report whether their target has
//! been received yet (Dream's `JOURNALINE_LINK_NOT_ACTIVE`). When the cache is full,
//! the least recently received page is evicted, except pages on the current
//! navigation path and the children of the current menu (the Fraunhofer
//! "keep in cache" list).

use super::decoder::{JournalineUpdate, ObjectStatus};
use super::nml::{NmlBody, NmlObject, ROOT_OBJECT_ID};
use std::collections::{HashMap, HashSet};

/// Default maximum number of cached pages.
pub const DEFAULT_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
struct CachedPage {
    object: NmlObject,
    received: u64,
    updated: bool,
}

/// A resolved menu entry for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    /// Entry text.
    pub text: String,
    /// Target object id.
    pub link: u16,
    /// `true` if the target page is in the cache (the entry can be followed).
    pub available: bool,
}

/// Journaline page cache with a navigation stack.
#[derive(Debug, Clone)]
pub struct JournalineBrowser {
    pages: HashMap<u16, CachedPage>,
    path: Vec<u16>,
    capacity: usize,
    clock: u64,
}

impl Default for JournalineBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalineBrowser {
    /// Empty browser positioned at the root menu.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Empty browser holding at most `capacity` pages (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            pages: HashMap::new(),
            path: vec![ROOT_OBJECT_ID],
            capacity: capacity.max(1),
            clock: 0,
        }
    }

    /// Store an update from [`super::JournalineDecoder`]. Returns `true` if the page
    /// currently shown changed.
    pub fn apply(&mut self, update: &JournalineUpdate) -> bool {
        let changed = self.insert(update.object.clone());
        if update.status == ObjectStatus::Updated {
            if let Some(p) = self.pages.get_mut(&update.object.object_id) {
                p.updated = true;
            }
        }
        changed
    }

    /// Store a page. Returns `true` if it is the page currently shown.
    pub fn insert(&mut self, object: NmlObject) -> bool {
        self.clock += 1;
        let id = object.object_id;
        let updated = self.pages.contains_key(&id);
        self.pages.insert(
            id,
            CachedPage {
                object,
                received: self.clock,
                updated,
            },
        );
        self.evict();
        id == self.current_id()
    }

    /// Cached page `id`.
    pub fn get(&self, id: u16) -> Option<&NmlObject> {
        self.pages.get(&id).map(|p| &p.object)
    }

    /// `true` if page `id` is cached.
    pub fn contains(&self, id: u16) -> bool {
        self.pages.contains_key(&id)
    }

    /// `true` if page `id` has been replaced by a newer version since first reception.
    pub fn was_updated(&self, id: u16) -> bool {
        self.pages.get(&id).is_some_and(|p| p.updated)
    }

    /// The root menu, once received.
    pub fn root(&self) -> Option<&NmlObject> {
        self.get(ROOT_OBJECT_ID)
    }

    /// Object id of the page currently shown.
    pub fn current_id(&self) -> u16 {
        *self.path.last().expect("path always holds the root")
    }

    /// The page currently shown (if received).
    pub fn current(&self) -> Option<&NmlObject> {
        self.get(self.current_id())
    }

    /// Navigation path from the root to the current page.
    pub fn path(&self) -> &[u16] {
        &self.path
    }

    /// Menu entries of page `id` with link availability (empty if `id` is not a menu).
    pub fn menu_entries(&self, id: u16) -> Vec<MenuEntry> {
        match self.get(id).map(|o| &o.body) {
            Some(NmlBody::Menu(items)) => items
                .iter()
                .map(|i| MenuEntry {
                    text: i.text.clone(),
                    link: i.link,
                    available: self.contains(i.link),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Follow entry `index` of the current menu. Returns `false` (and stays) if the
    /// current page is not a menu, the index is out of range or the target has not
    /// been received.
    pub fn follow(&mut self, index: usize) -> bool {
        let target = match self.current().map(|o| &o.body) {
            Some(NmlBody::Menu(items)) => items.get(index).map(|i| i.link),
            _ => None,
        };
        match target {
            Some(link) if self.contains(link) => {
                self.path.push(link);
                true
            }
            _ => false,
        }
    }

    /// Jump to page `id` (pushed on the path even if not yet received).
    pub fn open(&mut self, id: u16) {
        if self.current_id() != id {
            self.path.push(id);
        }
    }

    /// Go back one level. Returns `false` at the root.
    pub fn back(&mut self) -> bool {
        if self.path.len() > 1 {
            self.path.pop();
            true
        } else {
            false
        }
    }

    /// Return to the root menu.
    pub fn home(&mut self) {
        self.path.truncate(1);
    }

    /// Number of cached pages.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// `true` if no page is cached.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Drop all pages and return to the root (e.g. on a service change).
    pub fn clear(&mut self) {
        self.pages.clear();
        self.home();
    }

    fn evict(&mut self) {
        while self.pages.len() > self.capacity {
            let mut keep: HashSet<u16> = self.path.iter().copied().collect();
            if let Some(cur) = self.current() {
                keep.extend(cur.links());
            }
            let victim = self
                .pages
                .iter()
                .filter(|(id, _)| !keep.contains(id))
                .min_by_key(|(_, p)| p.received)
                .map(|(id, _)| *id);
            match victim {
                Some(id) => {
                    self.pages.remove(&id);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journaline::nml::MenuItem;

    fn menu(id: u16, links: &[u16]) -> NmlObject {
        NmlObject::menu(
            id,
            format!("Menu {id}"),
            links
                .iter()
                .map(|&l| MenuItem::new(l, format!("-> {l}")))
                .collect(),
        )
    }

    #[test]
    fn navigation() {
        let mut b = JournalineBrowser::new();
        assert!(b.current().is_none());
        assert!(b.insert(menu(0, &[1, 2])));
        let entries = b.menu_entries(0);
        assert!(!entries[0].available);
        assert!(!b.follow(0));
        assert!(!b.insert(NmlObject::plain_text(1, "One", "text")));
        assert!(b.menu_entries(0)[0].available);
        assert!(b.follow(0));
        assert_eq!(b.current().unwrap().title, "One");
        assert_eq!(b.path(), &[0, 1]);
        assert!(!b.follow(0)); // plain text has no links
        assert!(b.back());
        assert!(!b.back());
        b.open(2);
        assert!(b.current().is_none());
        b.home();
        assert_eq!(b.current_id(), 0);
    }

    #[test]
    fn eviction_keeps_path_and_children() {
        let mut b = JournalineBrowser::with_capacity(3);
        b.insert(menu(0, &[1]));
        b.insert(menu(1, &[5, 6]));
        assert!(b.follow(0));
        b.insert(NmlObject::title_only(5, "five"));
        b.insert(NmlObject::title_only(9, "nine"));
        b.insert(NmlObject::title_only(6, "six"));
        // Capacity 3 but root, current (1) and its children 5 and 6 are protected; 9 went.
        assert!(b.contains(0) && b.contains(1) && b.contains(5) && b.contains(6));
        assert!(!b.contains(9));
    }

    #[test]
    fn updates_are_flagged() {
        let mut b = JournalineBrowser::new();
        let obj = NmlObject::title_only(3, "v1");
        b.apply(&JournalineUpdate {
            object: obj.clone(),
            status: ObjectStatus::New,
        });
        assert!(!b.was_updated(3));
        b.apply(&JournalineUpdate {
            object: NmlObject {
                title: "v2".into(),
                ..obj
            },
            status: ObjectStatus::Updated,
        });
        assert!(b.was_updated(3));
        assert_eq!(b.get(3).unwrap().title, "v2");
    }
}
