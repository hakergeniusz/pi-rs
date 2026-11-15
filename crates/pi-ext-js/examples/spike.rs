//! Runs the E0 round trip end to end and prints measurements.
//!
//! Used as `cargo run -p pi-ext-js --example spike`: prints the registered
//! surface, the emit results, and the process peak RSS (`VmHWM` from
//! `/proc/self/status`) so the spike notes can record runtime weight.

use std::collections::HashMap;

use pi_ext_js::{JsHost, SPIKE_EXTENSION_SOURCE};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = JsHost::new()?;
    let mut config = HashMap::new();
    config.insert("model".to_string(), "glm-5.3-flash".to_string());
    host.set_config(config);
    host.load_extension("spike", SPIKE_EXTENSION_SOURCE)?;

    println!("registered events: {:?}", host.registered_events()?);
    println!("registered commands: {:?}", host.registered_commands()?);
    println!(
        "config round trip: {:?}",
        host.global_string("__seenModel")?
    );
    println!("emit session_start: {:?}", host.emit("session_start", "")?);
    println!(
        "emit before_tool_call: {:?}",
        host.emit("before_tool_call", r#"{"tool":"read"}"#)?
    );
    println!("peak rss (VmHWM): {} KiB", peak_rss_kib().unwrap_or(0));
    Ok(())
}

/// Peak resident set size (`VmHWM`, KiB) from `/proc/self/status`,
/// or `None` when unavailable.
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    None
}
