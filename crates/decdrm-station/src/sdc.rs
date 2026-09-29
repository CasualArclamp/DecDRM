//! The station's SDC: its data entities and the scheduler that fills the SDC block of
//! every super frame (ES 201 980 §6.4).
//!
//! Every block starts with the multiplex description (type 0), which a receiver needs
//! before it can decode anything. The time and date (type 8) follows when a new minute
//! has started (and in the first block). The remaining entities — audio information
//! (type 9) and application information (type 5) first, then labels (type 1) and
//! language/country (type 12) — go round robin: each block takes as many as fit,
//! starting with the first one that did not fit last time, so when the SDC is too small
//! for everything the entities cycle over several super frames and none starves.
//! Version flags are 0: the configuration never changes while the station runs.

use crate::config::StationConfig;
use crate::plan::MultiplexPlan;
use decdrm_core::mux::sdc::{
    ApplicationInfo, EntityBody, Label, LanguageCountry, SdcEntity, SdcError, TimeAndDate, encode_sdc_data,
};

/// The SDC entities of a station.
#[derive(Debug, Clone)]
pub(crate) struct SdcEntities {
    /// Type 0, sent in every block.
    pub multiplex: SdcEntity,
    /// Everything else except the time, in scheduling order.
    pub others: Vec<SdcEntity>,
}

/// Application information (type 5) of an application in the DAB application domain:
/// the application data starts with rfa (5 bits) and the user application type (11).
pub(crate) fn application_info(short_id: u8, stream_id: u8, packet_id: u8, packet_length: u8, user_app_id: u16) -> ApplicationInfo {
    ApplicationInfo {
        short_id,
        stream_id,
        packet_mode: true,
        data_unit_indicator: true,
        packet_id,
        enhancement: false,
        app_domain: decdrm_data::AppDomain::Dab.sdc_value(),
        packet_length,
        application_data: (user_app_id & 0x07FF).to_be_bytes().to_vec(),
    }
}

/// Build the entities of `plan`.
pub(crate) fn entities(cfg: &StationConfig, plan: &MultiplexPlan) -> SdcEntities {
    let e = |body| SdcEntity::new(false, body);
    let mut audio = Vec::new();
    let mut apps = Vec::new();
    let mut labels = Vec::new();
    let mut languages = Vec::new();
    for (s, sp) in cfg.services.iter().zip(&plan.services) {
        if let Some(a) = &sp.audio {
            audio.push(e(EntityBody::Audio(a.params.to_entity(sp.short_id))));
        }
        for a in &sp.apps {
            let user_app = a.kind.user_application().id();
            apps.push(e(EntityBody::Application(application_info(sp.short_id, a.stream, a.packet_id, a.packet_length, user_app))));
        }
        labels.push(e(EntityBody::Label(Label::new(sp.short_id, &s.label))));
        if s.iso_language.is_some() || s.iso_country.is_some() {
            let code = |v: &Option<String>, n: usize| -> Vec<u8> {
                match v {
                    Some(v) => v.to_ascii_lowercase().into_bytes(),
                    None => vec![b'-'; n],
                }
            };
            let (l, c) = (code(&s.iso_language, 3), code(&s.iso_country, 2));
            let mut language = [b'-'; 3];
            let mut country = [b'-'; 2];
            for (d, s) in language.iter_mut().zip(&l) {
                *d = *s;
            }
            for (d, s) in country.iter_mut().zip(&c) {
                *d = *s;
            }
            languages.push(e(EntityBody::LanguageCountry(LanguageCountry { short_id: sp.short_id, language, country })));
        }
    }
    let mut others = audio;
    others.extend(apps);
    others.extend(labels);
    others.extend(languages);
    SdcEntities { multiplex: e(EntityBody::Multiplex(plan.multiplex.clone())), others }
}

/// Audio information of a DecDRM EnCodec service.
fn is_encodec_audio(e: &SdcEntity) -> bool {
    matches!(&e.body, EntityBody::Audio(a) if decdrm_core::mux::service::is_encodec_config(&a.codec_config))
}

fn describe(e: &SdcEntity) -> String {
    match &e.body {
        EntityBody::Audio(a) => format!("audio information of service {}", a.short_id),
        EntityBody::Application(a) => format!("application information of service {}", a.short_id),
        EntityBody::Label(l) => format!("label of service {}", l.short_id),
        EntityBody::LanguageCountry(l) => format!("language and country of service {}", l.short_id),
        EntityBody::TimeDate(_) => "time and date".into(),
        other => format!("entity type {}", other.entity_type()),
    }
}

/// Problems with the SDC capacity: every entity must fit into a block together with
/// the multiplex description.
pub(crate) fn check_capacity(cfg: &StationConfig, plan: &MultiplexPlan) -> Vec<String> {
    let mut problems = Vec::new();
    let cap = plan.sdc_capacity;
    let ents = entities(cfg, plan);
    let hint = "use a 16-QAM SDC, a wider channel or fewer services";
    let mux = match ents.multiplex.encode() {
        Ok(b) => b.len(),
        Err(e) => {
            problems.push(format!("SDC multiplex description: {e}"));
            return problems;
        }
    };
    if mux > cap {
        problems.push(format!("the SDC holds {cap} bytes per super frame, the multiplex description alone needs {mux} ({hint})"));
        return problems;
    }
    let time = cfg.time.enabled.then(|| {
        SdcEntity::new(false, EntityBody::TimeDate(crate::time::time_entity(0, cfg.time.local_offset_minutes)))
    });
    for e in ents.others.iter().chain(time.as_ref()) {
        match e.encode() {
            Ok(b) if mux + b.len() > cap => problems.push(format!(
                "the SDC holds {cap} bytes per super frame, too few for the multiplex description ({mux} bytes) and the {} ({} bytes) ({hint})",
                describe(e),
                b.len()
            )),
            Ok(_) => {}
            Err(err) => problems.push(format!("SDC {}: {err}", describe(e))),
        }
    }
    problems
}

/// Fills the SDC block of each super frame (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct SdcScheduler {
    capacity: usize,
    multiplex: SdcEntity,
    mux_len: usize,
    others: Vec<(SdcEntity, usize)>,
    cursor: usize,
    time_enabled: bool,
    local_offset: Option<i32>,
    last_minute: Option<(u32, u8, u8)>,
    /// Blocks produced so far.
    pub blocks: u64,
    /// Data field bytes used by the last block.
    pub last_used: usize,
    /// The last time and date sent.
    pub last_time: Option<TimeAndDate>,
}

impl SdcScheduler {
    pub fn new(ents: SdcEntities, capacity: usize, time_enabled: bool, local_offset: Option<i32>) -> Result<Self, SdcError> {
        let mux_len = ents.multiplex.encode()?.len();
        let others = ents
            .others
            .into_iter()
            .map(|e| {
                let n = e.encode()?.len();
                Ok((e, n))
            })
            .collect::<Result<Vec<_>, SdcError>>()?;
        Ok(Self {
            capacity,
            multiplex: ents.multiplex,
            mux_len,
            others,
            cursor: 0,
            time_enabled,
            local_offset,
            last_minute: None,
            blocks: 0,
            last_used: 0,
            last_time: None,
        })
    }

    /// The SDC data field of the super frame starting at `now_unix` (UTC seconds).
    pub fn next_block(&mut self, now_unix: i64) -> Result<Vec<u8>, SdcError> {
        let mut chosen = vec![self.multiplex.clone()];
        let mut used = self.mux_len;
        if self.time_enabled {
            let t = crate::time::time_entity(now_unix, self.local_offset);
            let minute = (t.mjd, t.hour, t.minute);
            if self.last_minute != Some(minute) {
                let e = SdcEntity::new(false, EntityBody::TimeDate(t));
                let n = e.encode()?.len();
                if used + n <= self.capacity {
                    chosen.push(e);
                    used += n;
                    self.last_minute = Some(minute);
                    self.last_time = Some(t);
                }
            }
        }
        let n = self.others.len();
        let mut first_skipped = None;
        for k in 0..n {
            let i = (self.cursor + k) % n;
            let (e, len) = &self.others[i];
            if used + len <= self.capacity {
                chosen.push(e.clone());
                used += len;
            } else if first_skipped.is_none() {
                first_skipped = Some(i);
            }
        }
        if let Some(i) = first_skipped {
            self.cursor = i;
        }
        // An EnCodec audio entity goes last: receivers that do not skip its codec config
        // by the entity length (Dream) stop parsing the block there (see
        // `decdrm_core::mux::service::ENCODEC_CONFIG_MAGIC`). The sort is stable.
        chosen.sort_by_key(is_encodec_audio);
        let data = encode_sdc_data(&chosen, self.capacity)?;
        debug_assert!(data.skipped.is_empty());
        self.blocks += 1;
        self.last_used = used;
        Ok(data.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::mux::sdc::{MultiplexDescription, StreamLengths, parse_sdc};

    fn label(i: u8, text: &str) -> SdcEntity {
        SdcEntity::new(false, EntityBody::Label(Label::new(i, text)))
    }

    #[test]
    fn small_sdc_cycles_without_starvation() {
        let mux = SdcEntity::new(
            false,
            EntityBody::Multiplex(MultiplexDescription::new(0, 1, &[StreamLengths { part_a: 0, part_b: 100 }])),
        );
        // Multiplex description 5 bytes, time 5 bytes, each label 2 + 9 = 11 bytes: a
        // 37-byte data field holds two of the three labels.
        let others: Vec<SdcEntity> = (0..3).map(|i| label(i, "ABCDEFGHI")).collect();
        let mut s = SdcScheduler::new(SdcEntities { multiplex: mux, others }, 37, true, None).unwrap();
        let t0 = crate::time::parse_iso8601("2026-09-29T12:00:00Z").unwrap();
        let mut seen = [0usize; 3];
        let mut times = 0;
        for k in 0..30i64 {
            // One super frame is 1.2 s.
            let block = s.next_block(t0 + k * 12 / 10).unwrap();
            assert_eq!(block.len(), 37);
            let ents = parse_sdc(&block);
            assert!(matches!(ents[0].body, EntityBody::Multiplex(_)));
            for e in &ents {
                match &e.body {
                    EntityBody::Label(l) => seen[usize::from(l.short_id)] += 1,
                    EntityBody::TimeDate(_) => times += 1,
                    _ => {}
                }
            }
        }
        // 36 s: the time goes out in the first block of each minute (once here), and
        // the labels share the remaining space evenly.
        assert_eq!(times, 1);
        assert!(seen.iter().all(|&n| n >= 18), "{seen:?}");
    }

    /// An EnCodec audio information entity is the last entity of every block it is in,
    /// however the round robin orders the others.
    #[test]
    fn encodec_audio_information_goes_last() {
        use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams};
        let mux = SdcEntity::new(
            false,
            EntityBody::Multiplex(MultiplexDescription::new(0, 1, &[StreamLengths { part_a: 0, part_b: 381 }])),
        );
        let config = vec![0x00, b'E', b'N', b'C', b'1', 0x48];
        let audio = AudioParams::new(0, AudioCodec::Encodec, false, AudioMode::Mono, 24_000, true, config);
        let mut others = vec![SdcEntity::new(false, EntityBody::Audio(audio.to_entity(0)))];
        others.extend((0..3).map(|i| label(i, "ABCDEFGHI")));
        let mut s = SdcScheduler::new(SdcEntities { multiplex: mux, others }, 40, false, None).unwrap();
        let mut audio_blocks = 0;
        for _ in 0..12 {
            let ents = parse_sdc(&s.next_block(0).unwrap());
            if let Some(pos) = ents.iter().position(|e| matches!(e.body, EntityBody::Audio(_))) {
                assert_eq!(pos, ents.len() - 1, "{ents:?}");
                audio_blocks += 1;
            }
        }
        assert!(audio_blocks >= 4, "{audio_blocks}");
    }
}
