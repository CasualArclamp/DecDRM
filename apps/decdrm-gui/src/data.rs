//! GUI-side models of the data services, fed with [`EngineEvent::Data`] events:
//! one [`SlideShow`], [`JournalineBrowser`] and [`Website`] per data service
//! (keyed by the service's short id), plus counters for everything else.
//!
//! [`EngineEvent::Data`]: decdrm_engine::EngineEvent::Data

use decdrm_data::journaline::{JournalineBrowser, ListItem};
use decdrm_data::slideshow::SlideShow;
use decdrm_data::website::Website;
use decdrm_data::{DataEvent, DataStats};
use std::collections::BTreeMap;

/// Decoded content of one data service.
#[derive(Debug, Clone, Default)]
pub struct DataService {
    pub slideshow: SlideShow,
    pub journaline: JournalineBrowser,
    pub website: Website,
    /// EPG objects received (XML is not shown yet).
    pub epg_objects: u64,
    /// Other MOT objects (e.g. EPG logos).
    pub mot_objects: u64,
    /// Data units of applications that are not interpreted (TPEG, unknown).
    pub raw_units: u64,
    /// Bytes of a synchronous stream-mode service.
    pub stream_bytes: u64,
    /// Latest decoder statistics.
    pub stats: Option<DataStats>,
}

/// All data services seen since the engine started.
#[derive(Debug, Clone, Default)]
pub struct DataServices {
    services: BTreeMap<u8, DataService>,
}

impl DataServices {
    /// Feed one decoder event of service `short_id`. `now_unix` is the UTC time for
    /// slideshow trigger times; pass `None` for recordings, where "now" is meaningless
    /// and every slide should be shown as it arrives.
    pub fn apply(&mut self, short_id: u8, event: &DataEvent, now_unix: Option<i64>) {
        let s = self.services.entry(short_id).or_default();
        match event {
            DataEvent::SlideShowImage { .. } => {
                s.slideshow.apply(event, now_unix);
            }
            DataEvent::WebsiteFile { .. } | DataEvent::WebsiteIndex { .. } => {
                s.website.apply(event);
            }
            DataEvent::Journaline(update) => {
                s.journaline.apply(update);
            }
            DataEvent::Epg { .. } => s.epg_objects += 1,
            DataEvent::MotObject { .. } => s.mot_objects += 1,
            DataEvent::Raw { .. } => s.raw_units += 1,
            DataEvent::StreamData { data, .. } => s.stream_bytes += data.len() as u64,
            DataEvent::Stats(stats) => s.stats = Some(stats.clone()),
        }
    }

    /// Advance slideshow trigger times (live reception only).
    pub fn tick(&mut self, now_unix: i64) {
        for s in self.services.values_mut() {
            s.slideshow.tick(now_unix);
        }
    }

    pub fn clear(&mut self) {
        self.services.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    pub fn get_mut(&mut self, short_id: u8) -> Option<&mut DataService> {
        self.services.get_mut(&short_id)
    }

    /// All services with their content, by short id.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &DataService)> {
        self.services.iter().map(|(&id, s)| (id, s))
    }

    /// Short ids of the services that have shown at least one slide (or have one
    /// pending).
    pub fn slideshow_ids(&self) -> Vec<u8> {
        self.ids_where(|s| !s.slideshow.is_empty() || !s.slideshow.pending().is_empty())
    }

    /// Short ids of the services that delivered Journaline pages.
    pub fn journaline_ids(&self) -> Vec<u8> {
        self.ids_where(|s| !s.journaline.is_empty())
    }

    fn ids_where(&self, pred: impl Fn(&DataService) -> bool) -> Vec<u8> {
        self.services
            .iter()
            .filter(|(_, s)| pred(s))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Summed (good, CRC-failed) packet counters of all services, for the MSC
    /// indicator.
    pub fn packet_counters(&self) -> (u64, u64) {
        self.services
            .values()
            .filter_map(|s| s.stats.as_ref())
            .fold((0, 0), |(ok, bad), st| {
                (ok + st.packets_ok, bad + st.packets_crc_error)
            })
    }
}

/// Pick which service a data view shows: keep the user's choice while it still has
/// content, otherwise the first candidate.
pub fn choose_service(current: Option<u8>, candidates: &[u8]) -> Option<u8> {
    match current {
        Some(id) if candidates.contains(&id) => Some(id),
        _ => candidates.first().copied(),
    }
}

/// Group a Journaline list page into table rows: an item with `new_row` starts a row,
/// the following items without it are further cells of that row.
pub fn list_rows(items: &[ListItem]) -> Vec<Vec<&str>> {
    let mut rows: Vec<Vec<&str>> = Vec::new();
    for item in items {
        match rows.last_mut() {
            Some(row) if !item.new_row => row.push(&item.text),
            _ => rows.push(vec![&item.text]),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_data::JournalineUpdate;
    use decdrm_data::journaline::{NmlObject, ObjectStatus};
    use decdrm_data::mot::{MotHeader, content_type};

    fn slide_event(name: &str) -> DataEvent {
        let mut header = MotHeader::new(content_type::IMAGE, content_type::IMAGE_PNG, 3);
        header.set_content_name(name);
        DataEvent::SlideShowImage {
            transport_id: 7,
            name: name.into(),
            mime: "image/png".into(),
            data: vec![1, 2, 3],
            header,
        }
    }

    #[test]
    fn events_reach_the_right_models() {
        let mut d = DataServices::default();
        assert!(d.is_empty());
        d.apply(1, &slide_event("a.png"), None);
        d.apply(
            2,
            &DataEvent::Journaline(JournalineUpdate {
                object: NmlObject::title_only(0, "News"),
                status: ObjectStatus::New,
            }),
            None,
        );
        d.apply(
            2,
            &DataEvent::Raw {
                user_app_id: 4,
                data_group: vec![0; 5],
            },
            None,
        );
        d.apply(
            3,
            &DataEvent::StreamData {
                user_app_id: 9,
                data: vec![0; 10],
            },
            None,
        );
        d.apply(
            3,
            &DataEvent::StreamData {
                user_app_id: 9,
                data: vec![0; 6],
            },
            None,
        );

        assert_eq!(d.slideshow_ids(), vec![1]);
        assert_eq!(d.journaline_ids(), vec![2]);
        assert_eq!(
            d.get_mut(1).unwrap().slideshow.current().unwrap().name,
            "a.png"
        );
        assert_eq!(
            d.get_mut(2).unwrap().journaline.root().unwrap().title,
            "News"
        );
        assert_eq!(d.get_mut(2).unwrap().raw_units, 1);
        assert_eq!(d.get_mut(3).unwrap().stream_bytes, 16);
        assert_eq!(d.iter().map(|(id, _)| id).collect::<Vec<_>>(), [1, 2, 3]);
        d.clear();
        assert!(d.slideshow_ids().is_empty());
    }

    #[test]
    fn packet_counters_sum_over_services() {
        let mut d = DataServices::default();
        assert_eq!(d.packet_counters(), (0, 0));
        let stats = |ok, bad| {
            DataEvent::Stats(DataStats {
                packets_ok: ok,
                packets_crc_error: bad,
                ..Default::default()
            })
        };
        d.apply(1, &stats(10, 1), None);
        d.apply(2, &stats(5, 0), None);
        d.apply(1, &stats(12, 2), None); // cumulative: replaces the earlier value
        assert_eq!(d.packet_counters(), (17, 2));
    }

    #[test]
    fn service_choice() {
        assert_eq!(choose_service(None, &[]), None);
        assert_eq!(choose_service(None, &[2, 3]), Some(2));
        assert_eq!(choose_service(Some(3), &[2, 3]), Some(3));
        assert_eq!(
            choose_service(Some(1), &[2, 3]),
            Some(2),
            "a vanished choice falls back"
        );
    }

    #[test]
    fn list_pages_become_rows() {
        let items = vec![
            ListItem::row("a"),
            ListItem::cell("b"),
            ListItem::row("c"),
            ListItem::cell("d"),
        ];
        assert_eq!(list_rows(&items), vec![vec!["a", "b"], vec!["c", "d"]]);
        // A leading continuation cell still opens a row.
        assert_eq!(
            list_rows(&[ListItem::cell("x"), ListItem::cell("y")]),
            vec![vec!["x", "y"]]
        );
        assert!(list_rows(&[]).is_empty());
    }
}
