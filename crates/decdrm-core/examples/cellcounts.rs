use decdrm_core::cellmap::CellMap;
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
fn main() {
    for m in RobustnessMode::ALL {
        for so in SpectrumOccupancy::ALL {
            if let Some(map) = CellMap::new(m, so) {
                println!("{m} SO{}: N_MUX={} dummy={} N_SDC={} avgP/sym={:.1}", so.value(), map.msc_cells_per_frame, map.msc_dummy_cells, map.sdc_cells_per_superframe, map.avg_power_per_symbol);
            }
        }
    }
}
