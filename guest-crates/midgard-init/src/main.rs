//! midgard-init: PID 1 of midgard's Linux VMs.
//!
//! midgard boots the guest with an initramfs that contains this program as `/init`,
//! next to midgard-agent. It mounts the distribution's root file system, moves into it
//! and starts midgard-agent in a child process, which runs sessions (commands for
//! midgard's CLI) that arrive over vsock. Then it either stays PID 1 and reaps orphans
//! or executes the distribution's init (as the guest configuration in the initramfs
//! says).

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("midgard-init only runs in Linux guests");
    std::process::exit(1);
}
