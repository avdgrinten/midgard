//! How the agent sets up the guest and runs sessions. midgard decides it (it is part of
//! a VM instance's settings); the agent fetches it from midgard when it starts (see
//! [`fetch_config`]).

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::version::{Hello, Mismatch, Versions};

/// The name of the guest configuration in the initramfs.
pub const CONFIG_FILE: &str = "guest-config";

/// The agent's files (in the directory of [`crate::machine::MachineConfig::agent`]):
/// midgard-init, which the initramfs runs as /init (PID 1), and midgard-agent, which the
/// initramfs has under the same name.
pub const INIT_FILE: &str = "midgard-init";
pub const AGENT_FILE: &str = "midgard-agent";

/// The contents of [`CONFIG_FILE`].
pub fn encode_config(config: &GuestConfig) -> io::Result<Vec<u8>> {
    let mut file = Vec::new();
    crate::write_message(&mut file, &Hello::OURS)?;
    crate::write_message(&mut file, config)?;
    Ok(file)
}

/// Why the agent cannot use midgard's guest configuration (it does not guess what midgard
/// wants).
#[derive(Debug)]
pub enum ConfigError {
    /// midgard speaks no version of the guest protocol that the agent speaks.
    Mismatch(Mismatch),
    Io(io::Error),
}

/// What the guest's console says (with what helps).
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Mismatch(mismatch) => {
                let advice = match mismatch {
                    Mismatch::PeerTooOld { .. } => {
                        "midgard is too old for this agent: update midgard"
                    }
                    Mismatch::PeerTooNew { .. } => {
                        "this agent is too old for midgard: bifrost pull updates the boot files"
                    }
                };
                write!(f, "{advice} (midgard speaks {mismatch})")
            }
            Self::Io(error) => write!(
                f,
                "cannot fetch midgard's guest configuration ({error}): midgard and this \
                 agent do not fit together"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads midgard's [`Hello`] and the guest configuration, which follows it; returns the
/// latter and the version of the guest protocol to speak with midgard.
pub fn read_config(reader: &mut impl Read) -> Result<(u32, GuestConfig), ConfigError> {
    let eof = || ConfigError::Io(io::ErrorKind::UnexpectedEof.into());
    let hello: Hello = crate::read_message(reader)
        .map_err(ConfigError::Io)?
        .ok_or_else(eof)?;
    let version = Versions::OURS
        .negotiate(hello.protocol)
        .map_err(ConfigError::Mismatch)?;
    let config = crate::read_message(reader)
        .map_err(ConfigError::Io)?
        .ok_or_else(eof)?;
    Ok((version, config))
}

/// Fetches the guest configuration from midgard, on a connection to
/// [`crate::CONFIG_PORT`] of the host ([`crate::HOST_CID`]): the guest sends its
/// [`Hello`]; midgard answers with its own and, if both speak a common version of the
/// guest protocol, the [`GuestConfig`] (which another version may encode differently),
/// each in a frame (see [`crate::write_message`]). Returns the configuration and the
/// version of the guest protocol to speak with midgard.
pub fn fetch_config(stream: &mut (impl Read + Write)) -> Result<(u32, GuestConfig), ConfigError> {
    crate::write_message(stream, &Hello::OURS).map_err(ConfigError::Io)?;
    read_config(stream)
}

/// What midgard answers to a guest that fetches the guest configuration and sent
/// `guest` (see [`fetch_config`]).
pub fn config_answer(guest: Hello, config: &GuestConfig) -> io::Result<Vec<u8>> {
    let mut answer = Vec::new();
    crate::write_message(&mut answer, &Hello::OURS)?;
    if Versions::OURS.negotiate(guest.protocol).is_ok() {
        crate::write_message(&mut answer, config)?;
    }
    Ok(answer)
}

// What midgard-init puts into /run/midgard in the guest (/run is a tmpfs). The files of
// templates (and of the instances created from them) name the helper, and they stay as
// they are when the agent changes: the helper's path and its modes must keep working.
// The rest is in a directory of its own and may change with every version of the agent.

/// What the guest runs to reach the agent: midgard-agent under another name, under which
/// it only has the guest's modes (e.g., `/run/midgard/midgard-helper ready`).
pub const HELPER_PATH: &str = "/run/midgard/midgard-helper";
/// The helper under the name that it had before midgard-agent existed (midgard-init had
/// the guest's modes): the files of older templates run it.
pub const OLD_HELPER_PATH: &str = "/run/midgard/midgard-init";
/// The files that only midgard-init and the agent use. The guest must not rely on them.
pub const PRIVATE_DIR: &str = "/run/midgard/private";
/// midgard-agent in the guest (from the initramfs).
pub const AGENT_PATH: &str = "/run/midgard/private/midgard-agent";
/// The socket that the helper's `ready` connects to (the agent listens on it).
pub const NOTIFY_SOCKET: &str = "/run/midgard/private/notify";
/// The guest's waypipe in the guest (from the initramfs, if midgard offers one).
pub const WAYPIPE_PATH: &str = "/run/midgard/private/waypipe";
/// Exists if the guest's waypipe logs debug messages (see [`GuestConfig::waypipe_debug`]).
pub const WAYPIPE_DEBUG_PATH: &str = "/run/midgard/private/waypipe-debug";
/// The Wayland display's name in a user's runtime directory.
pub const WAYLAND_DISPLAY: &str = "wayland-0";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestConfig {
    /// The distribution's init (an absolute path in the guest), which midgard-init
    /// executes as PID 1 once it set up the guest and started the agent. Without it,
    /// midgard-init stays PID 1 and no services run.
    pub init: Option<String>,
    /// How sessions wait for the distribution's init to finish booting.
    pub ready: Ready,
    /// The user that sessions run as unless they name one (root if absent).
    pub user: Option<String>,
    /// A command that midgard-init runs on every boot (as root, before the distribution's
    /// init) with [`GuestConfig::user`] as an additional argument, e.g., to create the
    /// user. It must do nothing if there is nothing to do.
    pub user_setup: Option<Vec<String>>,
    /// How the agent runs the commands of sessions.
    pub launcher: Launcher,
    /// Who starts the Wayland display that forwards the guest's Wayland clients to the
    /// host (if midgard gives the guest a waypipe).
    pub display: Display,
    /// The guest's waypipe logs debug messages.
    pub waypipe_debug: bool,
}

/// Who starts the guest's Wayland display: a waypipe server that runs as a user and
/// listens on `$XDG_RUNTIME_DIR/wayland-0` (or another socket).
/// `/run/midgard/midgard-helper waypipe [<socket>]` runs it (with the right options; it
/// connects to [`crate::WAYPIPE_PORT`] for each client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Display {
    /// The agent, for the default user (on a socket in [`PRIVATE_DIR`]). Sessions of that
    /// user get `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
    #[default]
    Agent,
    /// Every user's systemd instance, through a user unit that the agent installs in
    /// /run/systemd/user; /run/environment.d sets `WAYLAND_DISPLAY` for the instance's
    /// services (and thus for the commands of [`Launcher::Systemd`]; sessions of other
    /// launchers need the guest to set it).
    Systemd,
    /// The guest itself (e.g., a user service of its init system); the agent only
    /// installs waypipe.
    Guest,
}

/// How the agent learns that the guest finished booting (sessions wait for it, such
/// that they find the distribution's services running).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Ready {
    /// Sessions do not wait.
    #[default]
    Immediately,
    /// Until `systemctl is-system-running --wait` returns.
    Systemd,
    /// Until root in the guest runs `/run/midgard/midgard-helper ready` (e.g., from its
    /// init's configuration, after the services started).
    Notify,
}

/// How the agent runs a session's command as its user.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Launcher {
    /// The agent runs the command itself with the user's credentials and a minimal
    /// environment (outside of the distribution's sessions).
    #[default]
    Direct,
    /// As a transient service of the user's systemd instance (`systemd-run --user`),
    /// such that it gets the environment of the user's session. The instance must run
    /// (e.g., because the user lingers).
    Systemd,
    /// Through a command of the guest that opens a session for the user and runs a
    /// program as the user (e.g., `runuser -l {user} -s {helper}`). `{user}` stands for
    /// the user's name and `{helper}` for the helper ([`HELPER_PATH`]), which the
    /// command must run with the file descriptors that it got (see [`Launch`]); commands
    /// that close them (e.g., sudo) do not work.
    Wrapper(Vec<String>),
}

/// What the helper runs when [`Launcher::Wrapper`] runs it in the user's
/// session. The agent passes it as one frame (see [`crate::write_message`]) in file
/// descriptor [`LAUNCH_FD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    /// The command; empty means the user's login shell.
    pub argv: Vec<String>,
    /// Environment variables in addition to the session's.
    pub env: Vec<(String, String)>,
    /// The working directory.
    pub cwd: String,
}

/// The file descriptor with the [`Launch`] request.
pub const LAUNCH_FD: i32 = 3;
