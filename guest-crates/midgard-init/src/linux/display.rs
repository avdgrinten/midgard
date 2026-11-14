//! Wayland forwarding (see [`Display`]): midgard puts its static waypipe into the
//! initramfs; we copy it into /run/midgard/private for the agent, and set up what the
//! configuration's [`Display`] needs before the distribution's init starts.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use midgard_protocol::guest::{
    Display, GuestConfig, HELPER_PATH, WAYLAND_DISPLAY, WAYPIPE_DEBUG_PATH, WAYPIPE_PATH,
};

use super::log;

/// The systemd user unit that runs the server (with [`Display::Systemd`]).
const UNIT: &str = "midgard-waypipe.service";

/// The unit's file.
fn unit_file() -> String {
    format!(
        "\
[Unit]
Description=Wayland forwarding to midgard's host

[Service]
ExecStart={HELPER_PATH} waypipe
Restart=on-failure

[Install]
WantedBy=default.target
"
    )
}

/// Reads the initramfs's waypipe (before we leave the initramfs).
pub fn load() -> Option<Vec<u8>> {
    let waypipe = std::fs::read("/waypipe").ok();
    // The initramfs stays in memory.
    let _ = std::fs::remove_file("/waypipe");
    waypipe
}

/// Installs waypipe as [`WAYPIPE_PATH`] (once /run is mounted, before the distribution's
/// init starts) and what the configuration's [`Display`] needs.
pub fn install(waypipe: &[u8], config: &GuestConfig) {
    let result = std::fs::write(WAYPIPE_PATH, waypipe)
        .and_then(|()| std::fs::set_permissions(WAYPIPE_PATH, PermissionsExt::from_mode(0o755)));
    if let Err(error) = result {
        log!("installing {WAYPIPE_PATH}: {error}");
        return;
    }
    if config.waypipe_debug {
        let _ = std::fs::write(WAYPIPE_DEBUG_PATH, "");
    }
    if config.display == Display::Systemd
        && let Err(error) = install_systemd_unit()
    {
        log!("installing the systemd user unit {UNIT}: {error}");
    }
}

/// Every user's systemd instance runs the server, and its environment names the display
/// (for the session's services and the commands of sessions).
fn install_systemd_unit() -> io::Result<()> {
    let units = Path::new("/run/systemd/user");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(units.join("default.target.wants"))?;
    std::fs::write(units.join(UNIT), unit_file())?;
    let link = units.join("default.target.wants").join(UNIT);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(Path::new("..").join(UNIT), link)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create("/run/environment.d")?;
    std::fs::write(
        "/run/environment.d/50-midgard.conf",
        format!("WAYLAND_DISPLAY={WAYLAND_DISPLAY}\n"),
    )
}
