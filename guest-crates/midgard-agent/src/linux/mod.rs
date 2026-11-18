//! The agent: serves sessions, watches the guest boot and runs the Wayland display; and
//! the helper, i.e., the modes in which the guest runs us (under another name).

mod display;
mod launcher;
mod process;
mod ready;
mod rt;
mod session;
mod setup;
mod sys;
mod users;

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_io::{Async, Timer};
use clap::{Parser, Subcommand};
use midgard_protocol::guest::{self, Display, GuestConfig, HELPER_PATH, WAYPIPE_PATH};
use midgard_protocol::version::{OLDEST_PROTOCOL, PROTOCOL};
use midgard_protocol::{AGENT_PORT, CONFIG_PORT, HOST_CID};

/// The default PATH of sessions.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Logs a line to the console, in one write: the distribution's init writes there, too.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!("midgard-agent: {}\n", format_args!($($arg)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}
pub(crate) use log;

/// How long we try to reach midgard for the guest configuration (the guest's vsock
/// driver may not be up yet when we start).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The guest configuration (see [`fetch_config`]).
static CONFIG: OnceLock<GuestConfig> = OnceLock::new();

/// The guest configuration, which the agent fetches before anything else.
pub fn config() -> &'static GuestConfig {
    CONFIG.get().expect("the agent fetched the configuration")
}

/// Fetches the guest configuration from midgard. Fails if midgard speaks no version of
/// the guest protocol that we speak, or if we do not get its configuration.
fn fetch_config() -> Result<GuestConfig> {
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let mut stream = loop {
        match sys::vsock_connect(HOST_CID, CONFIG_PORT) {
            Ok(stream) => break stream,
            // (A midgard from before the agent fetched the configuration does not answer.)
            Err(error) if Instant::now() > deadline => {
                bail!("cannot reach midgard for the guest configuration ({error})")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let (version, config) = guest::fetch_config(&mut stream)?;
    if version != PROTOCOL {
        log!("midgard is older: speaking version {version} of the guest protocol");
    }
    Ok(config)
}

/// midgard's guest agent, as midgard-init runs it. The guest runs it as the helper
/// instead (see [`Helper`]).
#[derive(Debug, Parser)]
#[command(name = "midgard-agent")]
struct Agent {
    #[command(subcommand)]
    mode: AgentMode,
}

#[derive(Debug, Subcommand)]
enum AgentMode {
    /// Serve sessions, watch the guest boot and run the Wayland display
    Serve {
        /// midgard-init installed the agent in /run/midgard (else, in a guest whose image
        /// has the agent, the agent installs itself first)
        #[arg(long)]
        from_midgard_init: bool,
    },
    /// Wait forever (the waypipe server's command)
    Pause,
    /// Print the oldest and the newest version of the guest protocol that we speak (for
    /// guest/build.py)
    #[command(hide = true)]
    Protocol,
}

/// What the guest runs to reach midgard's guest agent (as /run/midgard/midgard-helper).
/// Templates rely on these modes: they must keep working as they are.
#[derive(Debug, Parser)]
#[command(name = "midgard-helper")]
struct Helper {
    /// Without a mode, it runs as `exec` (a wrapper runs it as the user's login shell,
    /// i.e., without arguments).
    #[command(subcommand)]
    mode: Option<HelperMode>,
}

#[derive(Debug, Subcommand)]
enum HelperMode {
    /// Run a session's command (as the wrapper's helper, in the user's session)
    Exec,
    /// Tell the agent that the guest finished booting
    Ready,
    /// Run the Wayland display's waypipe server [default socket: $XDG_RUNTIME_DIR/wayland-0]
    Waypipe { socket: Option<String> },
}

/// Whether we run as the helper, going by the name that we were run as: one executable
/// is both, such that the guest's modes are not part of what midgard-init runs (and the
/// other way around). A login shell's name starts with a dash.
fn is_helper(arg0: &OsStr) -> bool {
    let Some(name) = Path::new(arg0).file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let name = name.strip_prefix('-').unwrap_or(name);
    Path::new(HELPER_PATH).file_name() == Some(OsStr::new(name))
}

pub fn main() {
    let helper = std::env::args_os()
        .next()
        .is_some_and(|arg0| is_helper(&arg0));
    let (name, result) = if helper {
        let result = match Helper::parse().mode {
            None | Some(HelperMode::Exec) => launcher::helper(),
            Some(HelperMode::Ready) => ready::notify(),
            Some(HelperMode::Waypipe { socket }) => display::server(socket),
        };
        ("midgard-helper", result)
    } else {
        let result = match Agent::parse().mode {
            AgentMode::Serve { from_midgard_init } => {
                agent(!from_midgard_init).map_err(|error| format!("{error:#}"))
            }
            AgentMode::Pause => display::pause(),
            AgentMode::Protocol => {
                println!("{OLDEST_PROTOCOL} {PROTOCOL}");
                Ok(())
            }
        };
        ("midgard-agent", result)
    };
    if let Err(error) = result {
        eprintln!("{name}: {error}");
        std::process::exit(1);
    }
}

/// `midgard-agent serve`: serves sessions and runs the Wayland display for the default
/// user (with midgard's waypipe and [`Display::Agent`]). One thread runs everything (see
/// [`rt`]).
fn agent(install: bool) -> Result<()> {
    let config = fetch_config()?;
    if install {
        setup::install(&config).context("installing the agent in /run/midgard")?;
    }
    let config = CONFIG.get_or_init(|| config);
    let agent_display = config.display == Display::Agent && Path::new(WAYPIPE_PATH).exists();
    let watcher = ready::Watcher::new(config.ready);
    rt::block_on(async {
        // Sessions find the user set up and the display running.
        let booted = async move {
            setup::user_setup(config).await;
            if agent_display {
                let name = config.user.as_deref().unwrap_or("root");
                match users::lookup(name) {
                    Ok(user) => display::start(&user).await,
                    Err(error) => log!("{error}"),
                }
            }
        };
        rt::spawn(ready::watch(watcher, booted)).detach();
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
    use clap::CommandFactory;
    use midgard_protocol::guest::AGENT_PATH;

    use super::*;

    fn agent(args: &[&str]) -> Result<AgentMode, clap::Error> {
        Agent::try_parse_from(args).map(|cli| cli.mode)
    }

    fn helper(args: &[&str]) -> Result<Option<HelperMode>, clap::Error> {
        Helper::try_parse_from(args).map(|cli| cli.mode)
    }

    /// What templates run: these paths must keep working.
    #[test]
    fn names() {
        assert_eq!(HELPER_PATH, "/run/midgard/midgard-helper");
        assert!(is_helper(OsStr::new(HELPER_PATH)));
        assert!(is_helper(OsStr::new("midgard-helper")));
        // A wrapper runs us as a login shell.
        assert!(is_helper(OsStr::new("-midgard-helper")));
        // midgard-init and waypipe run us as the agent.
        assert!(!is_helper(OsStr::new(AGENT_PATH)));
        assert!(!is_helper(OsStr::new("midgard-agent")));
        assert!(!is_helper(OsStr::new("")));
    }

    #[test]
    fn agent_modes() {
        Agent::command().debug_assert();
        assert!(matches!(
            agent(&["midgard-agent", "serve", "--from-midgard-init"]),
            Ok(AgentMode::Serve {
                from_midgard_init: true
            })
        ));
        // A guest whose image has the agent.
        assert!(matches!(
            agent(&["midgard-agent", "serve"]),
            Ok(AgentMode::Serve {
                from_midgard_init: false
            })
        ));
        assert!(matches!(
            agent(&["midgard-agent", "pause"]),
            Ok(AgentMode::Pause)
        ));
        assert!(matches!(
            agent(&["midgard-agent", "protocol"]),
            Ok(AgentMode::Protocol)
        ));
        assert!(agent(&["midgard-agent"]).is_err());
        // The helper's modes.
        assert!(agent(&["midgard-agent", "exec"]).is_err());
        assert!(agent(&["midgard-agent", "ready"]).is_err());
        assert!(agent(&["midgard-agent", "waypipe"]).is_err());
    }

    /// What templates run: these must keep working.
    #[test]
    fn helper_modes() {
        Helper::command().debug_assert();
        assert!(matches!(helper(&["-midgard-helper"]), Ok(None)));
        assert!(matches!(
            helper(&["midgard-helper", "exec"]),
            Ok(Some(HelperMode::Exec))
        ));
        assert!(matches!(
            helper(&["midgard-helper", "ready"]),
            Ok(Some(HelperMode::Ready))
        ));
        assert!(matches!(
            helper(&["midgard-helper", "waypipe"]),
            Ok(Some(HelperMode::Waypipe { socket: None }))
        ));
        assert!(matches!(
            helper(&["midgard-helper", "waypipe", "/run/x"]),
            Ok(Some(HelperMode::Waypipe { socket: Some(socket) })) if socket == "/run/x"
        ));
        // The agent's modes.
        assert!(helper(&["midgard-helper", "serve"]).is_err());
        assert!(helper(&["midgard-helper", "pause"]).is_err());
        assert!(helper(&["midgard-helper", "protocol"]).is_err());
        assert!(helper(&["midgard-helper", "bogus"]).is_err());
    }
}
