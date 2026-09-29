//! Human-readable alternative-frequency (AFS) information from the SDC (entity types
//! 3, 4, 7, 11, ES 201 980 §6.4.3.4–§6.4.3.12), for status displays.

use decdrm_core::mux::sdc::{AfsOtherService, OtherFrequency, RegionSchedule, TimeAndDate};
use decdrm_core::mux::service::AltFrequencies;

/// One line per frequency list, schedule and region. `now` (the broadcast time)
/// marks lists whose schedule is currently active or inactive.
pub fn describe(afs: &AltFrequencies, now: Option<&TimeAndDate>) -> Vec<String> {
    let minute_of_week = now.map(minute_of_week);
    let mut lines = Vec::new();
    for m in afs.multiplexes.items() {
        let freqs: Vec<String> = m.frequencies.iter().map(|f| format!("{} kHz", f.khz())).collect();
        let services = match m.short_id_flags {
            None => "this multiplex".to_string(),
            Some(flags) => format!("services {}", short_ids(flags)),
        };
        lines.push(format!(
            "{services}: {}{}{}",
            freqs.join(", "),
            if m.synchronous { " (synchronous)" } else { "" },
            region_schedule(afs, m.region_schedule, minute_of_week)
        ));
    }
    for o in afs.other_services.items() {
        lines.push(other_service(afs, o, minute_of_week));
    }
    for s in afs.schedules.items() {
        lines.push(format!(
            "schedule {}: {} {:02}:{:02} UTC for {} min",
            s.schedule_id,
            days(s.day_code),
            s.start_minute / 60,
            s.start_minute % 60,
            s.duration_minutes
        ));
    }
    for r in afs.regions.items() {
        let zones: Vec<String> = r.ciraf_zones.iter().map(u8::to_string).collect();
        lines.push(format!(
            "region {}: lat {}..{}°, lon {}..{}°{}",
            r.region_id,
            r.latitude,
            i32::from(r.latitude) + i32::from(r.latitude_extent),
            r.longitude,
            i32::from(r.longitude) + i32::from(r.longitude_extent),
            if zones.is_empty() { String::new() } else { format!(", CIRAF {}", zones.join(" ")) }
        ));
    }
    lines
}

fn other_service(afs: &AltFrequencies, o: &AfsOtherService, minute_of_week: Option<u32>) -> String {
    let system = match o.system_id {
        0 => "DRM",
        1 | 2 => "AM",
        3..=8 => "FM",
        9..=11 => "DAB",
        _ => "other system",
    };
    let freqs: Vec<String> = (0..o.frequencies.len()).filter_map(|i| o.frequency(i)).map(other_frequency).collect();
    let who = if o.announcement { format!("announcement {}", o.id) } else { format!("service {}", o.id) };
    let id = o.other_service_id.map(|id| format!(" id {id:X}")).unwrap_or_default();
    format!(
        "{who} {} on {system}{id}: {}{}",
        if o.same_service { "also" } else { "alternative" },
        freqs.join(", "),
        region_schedule(afs, o.region_schedule, minute_of_week)
    )
}

fn other_frequency(f: OtherFrequency) -> String {
    match f {
        OtherFrequency::Khz(k) => format!("{k} kHz"),
        OtherFrequency::FmKhz(k) => format!("{:.1} MHz", f64::from(k) / 1000.0),
        OtherFrequency::DabChannel(c) => dab_channel(c),
        OtherFrequency::Raw(v) => format!("code {v}"),
    }
}

/// DAB channel name of a code (64..=95 = 5A..12D, 96..=101 = 13A..13F).
fn dab_channel(code: u8) -> String {
    match code {
        64..=95 => {
            let i = code - 64;
            format!("DAB {}{}", 5 + i / 4, char::from(b'A' + i % 4))
        }
        96..=101 => format!("DAB 13{}", char::from(b'A' + (code - 96))),
        _ => format!("DAB code {code}"),
    }
}

fn region_schedule(afs: &AltFrequencies, rs: Option<RegionSchedule>, minute_of_week: Option<u32>) -> String {
    let Some(rs) = rs else { return String::new() };
    let mut s = String::new();
    if rs.region_id != 0 {
        s += &format!(" · region {}", rs.region_id);
    }
    if rs.schedule_id != 0 {
        s += &format!(" · schedule {}", rs.schedule_id);
        if let Some(m) = minute_of_week {
            s += if afs.schedule_active(rs.schedule_id, m) { " (active)" } else { " (inactive)" };
        }
    }
    s
}

/// Short ids set in a 4-bit restriction mask (msb = Short Id 3).
fn short_ids(flags: u8) -> String {
    let ids: Vec<String> = (0..4).filter(|i| flags & (1 << i) != 0).map(|i: u8| i.to_string()).collect();
    ids.join(",")
}

/// Day code (bit 6 = Monday … bit 0 = Sunday) as e.g. `Mon Wed Fri`.
fn days(code: u8) -> String {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    if code & 0x7F == 0x7F {
        return "daily".into();
    }
    let d: Vec<&str> = (0..7).filter(|i| code & (0x40 >> i) != 0).map(|i| NAMES[i]).collect();
    d.join(" ")
}

/// UTC minutes since Monday 00:00 (MJD 0 was a Wednesday).
fn minute_of_week(t: &TimeAndDate) -> u32 {
    let weekday = (t.mjd + 2) % 7;
    weekday * 1440 + u32::from(t.hour) * 60 + u32::from(t.minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::mux::sdc::{AfsMultiplex, AfsSchedule, DrmFrequency};

    #[test]
    fn names() {
        assert_eq!(dab_channel(64), "DAB 5A");
        assert_eq!(dab_channel(95), "DAB 12D");
        assert_eq!(dab_channel(101), "DAB 13F");
        assert_eq!(days(0x7F), "daily");
        assert_eq!(days(0x41), "Mon Sun");
        assert_eq!(short_ids(0b0101), "0,2");
        // 2023-02-25 (MJD 60000) was a Saturday.
        let t = TimeAndDate { mjd: 60_000, hour: 1, minute: 2, local_offset: None };
        assert_eq!(minute_of_week(&t), 5 * 1440 + 62);
    }

    #[test]
    fn describes_lists_and_schedules() {
        let mut afs = AltFrequencies::default();
        afs.multiplexes.insert(
            false,
            AfsMultiplex {
                synchronous: true,
                frequencies: vec![DrmFrequency::from_khz(5975), DrmFrequency::from_khz(6000)],
                region_schedule: Some(RegionSchedule { region_id: 0, schedule_id: 1 }),
                ..Default::default()
            },
        );
        afs.schedules.insert(false, AfsSchedule { schedule_id: 1, day_code: 0x7C, start_minute: 360, duration_minutes: 120 });
        afs.other_services.insert(
            false,
            AfsOtherService { id: 0, same_service: true, system_id: 3, frequencies: vec![136], ..Default::default() },
        );
        // Monday 07:00 UTC (MJD 60002 = 2023-02-27, a Monday).
        let now = TimeAndDate { mjd: 60_002, hour: 7, minute: 0, local_offset: None };
        let lines = describe(&afs, Some(&now));
        assert_eq!(lines[0], "this multiplex: 5975 kHz, 6000 kHz (synchronous) · schedule 1 (active)");
        assert_eq!(lines[1], "service 0 also on FM: 101.1 MHz");
        assert_eq!(lines[2], "schedule 1: Mon Tue Wed Thu Fri 06:00 UTC for 120 min");
        let later = TimeAndDate { hour: 9, ..now };
        assert!(describe(&afs, Some(&later))[0].ends_with("(inactive)"));
    }
}
