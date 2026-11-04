//! Hardware test: a transmission shorter than the prebuffer must still finish.

#![cfg(feature = "hackrf")]

use decdrm_station::{Station, StationConfig};

/// A tone service straight to a HackRF One, the default `buffer_s` of 1 s.
fn config() -> StationConfig {
    StationConfig::from_toml_str(
        r#"
[output]
format = "iq"
[output.hackrf]
frequency_hz = 1_530_000
tx_gain_db = 0
amp_enable = false
[time]
[[service]]
label = "Test"
id = 0x42
[service.audio]
codec = "he-aac"
input = { tone_hz = 440.0 }
"#,
    )
    .unwrap()
}

/// One frame is 0.4 s, less than the 1 s prebuffer: the writer used to wait for
/// the prebuffer for ever, so the radio never started and `finish` hung. It must
/// take the tail and switch the radio off.
#[test]
#[ignore = "needs a HackRF One on the bus"]
fn one_frame_transmission_finishes() {
    let mut station = Station::new(config()).expect("the radio opens");
    station.transmit_frame().expect("a frame is queued");
    let status = station.finish().expect("the radio takes the tail");
    assert!(status.hackrf.is_some(), "the status describes the radio");
}
