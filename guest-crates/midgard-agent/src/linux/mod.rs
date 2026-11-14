//! The agent: serves sessions, watches the guest boot and runs the Wayland display; and
//! the helper, i.e., the modes in which the guest runs us (under another name).

mod display;
mod launcher;
mod process;
mod ready;
mod rt;
mod session;
mod sys;
mod users;

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use async_io::{Async, Timer};
use midgard_protocol::AGENT_PORT;
use midgard_protocol::guest::{
    CONFIG_PATH, Display, GuestConfig, HELPER_PATH, OLD_HELPER_PATH, WAYPIPE_PATH, read_config,
};
use midgard_protocol::version::{OLDEST_PROTOCOL, PROTOCOL};

/// The default PATH of sessions.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The names under which we are the helper (see [`is_helper`]).
const HELPER_PATHS: [&str; 2] = [HELPER_PATH, OLD_HELPER_PATH];

/// Logs a line to the console, in one write: the distribution's init writes there, too.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!("midgard-agent: {}\n", format_args!($($arg)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}
pub(crate) use log;

/// The guest configuration (from midgard-init).
static CONFIG: OnceLock<GuestConfig> = OnceLock::new();

pub fn config() -> &'static GuestConfig {
    CONFIG.get_or_init(load_config)
}

fn load_config() -> GuestConfig {
    // (midgard-init read it before, so it does not fail but for a broken guest.)
    let result = File::open(CONFIG_PATH)
        .map_err(|error| error.to_string())
        .and_then(|mut file| read_config(&mut file).map_err(|error| error.to_string()));
    match result {
        Ok((_version, config)) => config,
        Err(error) => {
            log!("reading {CONFIG_PATH}: {error}; using the default configuration");
            GuestConfig::default()
        }
    }
}

/// Whether we run as the helper, going by the name that we were run as: one executable
/// is both, such that the guest's modes are not part of what midgard-init runs (and the
/// other way around). A login shell's name starts with a dash.
fn is_helper(arg0: &OsStr) -> bool {
    let Some(name) = Path::new(arg0).file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let name = name.strip_prefix('-').unwrap_or(name);
    HELPER_PATHS
        .iter()
        .any(|path| Path::new(path).file_name() == Some(OsStr::new(name)))
}

pub fn main() {
    // midgard-init runs us as the agent. The guest runs us as the helper, in other modes
    // (templates rely on these: they must keep working as they are); a wrapper runs the
    // helper as the user's login shell (see launcher::helper), i.e., without arguments.
    let mut args = std::env::args();
    let helper = args.next().is_some_and(|arg0| is_helper(OsStr::new(&arg0)));
    let mode = args.next();
    let result = match (helper, mode.as_deref()) {
        (false, Some("serve")) => agent().map_err(|error| format!("{error:#}")),
        (false, Some("pause")) => display::pause(),
        (false, Some("protocol")) => {
            // For guest/build.py: the versions of the guest protocol that we speak.
            println!("{OLDEST_PROTOCOL} {PROTOCOL}");
            Ok(())
        }
        (true, None | Some("exec")) => launcher::helper(),
        (true, Some("ready")) => ready::notify(),
        (true, Some("waypipe")) => display::server(args.next()),
        (false, None) => Err("no mode".into()),
        (_, Some(mode)) => Err(format!("unknown mode {mode:?}")),
    };
    if let Err(error) = result {
        let name = if helper {
            "midgard-helper"
        } else {
            "midgard-agent"
        };
        eprintln!("{name}: {error}");
        std::process::exit(1);
    }
}

/// `midgard-agent serve`: serves sessions and runs the Wayland display for the default
/// user (with midgard's waypipe and [`Display::Agent`]). One thread runs everything (see
/// [`rt`]).
fn agent() -> Result<()> {
    let config = config();
    let agent_display = config.display == Display::Agent && Path::new(WAYPIPE_PATH).exists();
    let watcher = ready::Watcher::new(config.ready);
    rt::block_on(async {
        // Sessions find the display running.
        let display = async move {
            if agent_display {
                let name = config.user.as_deref().unwrap_or("root");
                match users::lookup(name) {
                    Ok(user) => display::start(&user).await,
                    Err(error) => log!("{error}"),
                }
            }
        };
        rt::spawn(ready::watch(watcher, display)).detach();
        serve().await
    })
}

/// Accepts sessions on the agent port; each one is a task.
async fn serve() -> Result<()> {
    let listener = sys::vsock_listen(AGENT_PORT)?;
    // SAFETY: We own the new socket.
    let listener = Async::new(unsafe { OwnedFd::from_raw_fd(listener) })
        .context("registering the vsock listener")?;
    log!("ready");
    loop {
        match listener
            .read_with(|listener| sys::accept(listener.as_raw_fd()))
            .await
        {
            Ok(fd) => {
                // SAFETY: accept() returned a new socket that we own.
                let stream = unsafe { File::from_raw_fd(fd) };
                match Async::new(stream) {
                    Ok(stream) => rt::spawn(session::handle(stream)).detach(),
                    Err(error) => log!("vsock stream: {error}"),
                }
            }
            Err(error) => {
                log!("accept(): {error}");
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use midgard_protocol::guest::AGENT_PATH;

    use super::*;

    /// What templates run: these paths must keep working.
    #[test]
    fn names() {
        assert_eq!(HELPER_PATH, "/run/midgard/midgard-helper");
        assert!(is_helper(OsStr::new(HELPER_PATH)));
        assert!(is_helper(OsStr::new("midgard-helper")));
        // A wrapper runs us as a login shell.
        assert!(is_helper(OsStr::new("-midgard-helper")));
        // Older templates.
        assert_eq!(OLD_HELPER_PATH, "/run/midgard/midgard-init");
        assert!(is_helper(OsStr::new(OLD_HELPER_PATH)));
        // midgard-init and waypipe run us as the agent.
        assert!(!is_helper(OsStr::new(AGENT_PATH)));
        assert!(!is_helper(OsStr::new("midgard-agent")));
        assert!(!is_helper(OsStr::new("")));
    }
}
