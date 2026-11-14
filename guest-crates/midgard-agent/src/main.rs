//! midgard-agent: midgard's guest agent in Linux VMs.
//!
//! midgard-init (PID 1) installs it in the guest and starts it (`midgard-agent serve`)
//! before the distribution's init. It runs sessions (commands for midgard's CLI) that
//! arrive over vsock, watches the guest boot and runs the Wayland display. The guest
//! runs it under another name, `/run/midgard/midgard-helper`, and in other modes, e.g.,
//! to tell it that the guest booted.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("midgard-agent only runs in Linux guests");
    std::process::exit(1);
}
