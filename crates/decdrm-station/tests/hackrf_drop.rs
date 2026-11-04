//! Hardware test: a station dropped without `finish` must give the radio back.

#![cfg(feature = "hackrf")]

use decdrm_station::{Station, StationConfig};

/// A tone service straight to a HackRF One.
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

#[test]
#[ignore = "needs a HackRF One on the bus"]
fn dropped_station_releases_the_radio() {
    // Open the radio, queue a frame, drop the station without finishing it: the
    // writer thread must abandon the queue, switch the radio off and exit.
    {
        let mut station = Station::new(config()).expect("first open");
        station.transmit_frame().expect("a frame is queued for the radio");
    }
    // With the writer leaked the radio would still be claimed and this would fail.
    let mut station = Station::new(config()).expect("the radio is free again");
    station.transmit_frame().expect("and usable");
}
