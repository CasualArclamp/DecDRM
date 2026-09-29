//! Lists the sound-card devices DecDRM can use.
//!
//! ```text
//! cargo run -p decdrm-io --example list_devices
//! ```

use decdrm_io::{list_input_devices, list_output_devices, DeviceInfo};

fn print_devices(title: &str, devices: decdrm_io::Result<Vec<DeviceInfo>>) {
    println!("{title}:");
    match devices {
        Ok(list) if list.is_empty() => println!("  (none)"),
        Ok(list) => {
            for d in list {
                let default = if d.is_default { "  [default]" } else { "" };
                println!("  {}{default}", d.name);
                println!("      host: {}", d.host);
                if let Some(id) = &d.id {
                    println!("      id:   {id}");
                }
                if let Some(f) = d.default_format {
                    println!("      default format: {f}");
                }
                let rates: Vec<String> = d.sample_rates().iter().map(|r| r.to_string()).collect();
                let channels: Vec<String> =
                    d.channel_counts().iter().map(|c| c.to_string()).collect();
                let mut formats: Vec<&str> = d.configs.iter().map(|c| c.sample_format.as_str()).collect();
                formats.sort_unstable();
                formats.dedup();
                println!("      rates: {} Hz", rates.join(", "));
                println!("      channels: {}   sample types: {}", channels.join(", "), formats.join(", "));
            }
        }
        Err(e) => println!("  error: {e}"),
    }
}

fn main() {
    print_devices("Input devices", list_input_devices());
    println!();
    print_devices("Output devices", list_output_devices());
}
