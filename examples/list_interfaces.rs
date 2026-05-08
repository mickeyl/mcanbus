//! Print every CAN interface visible to the kernel, with state and bitrate.
//!
//! Read-only: no frames are sent and no netlink-write operations happen, so
//! it is safe to run against live buses.

fn main() -> std::io::Result<()> {
    let ifaces = mcanbus::iface::list_can_interfaces()?;
    if ifaces.is_empty() {
        println!("(no CAN interfaces found)");
        return Ok(());
    }
    println!("{:<12} {:<6} {:<14} bitrate", "name", "up", "state");
    for iface in &ifaces {
        let up = iface
            .is_up()?
            .map(|b| if b { "yes" } else { "no" })
            .unwrap_or("?");
        let state = iface
            .state()
            .ok()
            .flatten()
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| "?".to_string());
        let bitrate = iface
            .bitrate()
            .ok()
            .flatten()
            .map(|b| format!("{b}"))
            .unwrap_or_else(|| "?".to_string());
        println!("{:<12} {:<6} {:<14} {bitrate}", iface.name, up, state);
    }
    Ok(())
}
