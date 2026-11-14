//! Wayland forwarding: a waypipe server, running as a user, is the guest's Wayland
//! display (`$XDG_RUNTIME_DIR/wayland-0`). For each Wayland client, it connects to
//! midgard (vsock port [`WAYPIPE_PORT`]), which hands the connection to `waypipe client`
//! next to the host's compositor. `midgard-helper waypipe` runs the server (see
//! [`server`]); who runs that depends on the guest configuration (see
//! [`Display`](midgard_protocol::guest::Display)).
//! midgard-init installs waypipe (from the initramfs) as [`WAYPIPE_PATH`].

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_io::Timer;

use midgard_protocol::guest::{
    AGENT_PATH, HELPER_PATH, PRIVATE_DIR, WAYLAND_DISPLAY, WAYPIPE_DEBUG_PATH, WAYPIPE_PATH,
};
use midgard_protocol::{HOST_CID, WAYPIPE_PORT};

use super::process::{self, Command, Credentials};
use super::users::User;
use super::{PATH, log};

/// The display that the agent runs for a user (with [`Display::Agent`](midgard_protocol::guest::Display::Agent)): the user's ID and
/// the socket.
static AGENT_DISPLAY: OnceLock<(u32, String)> = OnceLock::new();

fn runtime_dir(user: &User) -> String {
    format!("/run/user/{}", user.uid)
}

/// Creates a directory that belongs to `user` (if it does not exist).
fn create_user_dir(dir: &str, user: &User) -> io::Result<()> {
    if Path::new(dir).exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    std::os::unix::fs::chown(dir, Some(user.uid), Some(user.gid))
}

/// Starts the server for `user` (with [`Display::Agent`](midgard_protocol::guest::Display::Agent), once the guest booted). Its
/// socket is not in the user's runtime directory: logind may mount a tmpfs over that
/// (when the user logs in for the first time), which would hide the socket.
pub async fn start(user: &User) {
    let dir = runtime_dir(user);
    let socket_dir = format!("{PRIVATE_DIR}/display-{}", user.uid);
    let result = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create("/run/user")
        .and_then(|()| create_user_dir(&dir, user))
        .and_then(|()| create_user_dir(&socket_dir, user));
    if let Err(error) = result {
        log!("creating the directories of {}: {error}", user.name);
        return;
    }
    let socket = format!("{socket_dir}/{WAYLAND_DISPLAY}");
    let argv = ["midgard-helper", "waypipe", &socket]
        .map(String::from)
        .into();
    let env = vec![
        format!("PATH={PATH}"),
        format!("HOME={}", user.home),
        format!("XDG_RUNTIME_DIR={dir}"),
    ];
    let mut command = Command::new(HELPER_PATH, argv, env);
    command.credentials = (user.uid != 0).then(|| Credentials {
        uid: user.uid,
        gid: user.gid,
        groups: user.groups.clone(),
    });
    if let Err(error) = process::spawn_background(&command) {
        log!("starting waypipe: {error}");
        return;
    }
    // Sessions find the socket, which waypipe creates soon after it started.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !Path::new(&socket).exists() && Instant::now() < deadline {
        Timer::after(Duration::from_millis(10)).await;
    }
    log!("started the Wayland display of {}", user.name);
    let _ = AGENT_DISPLAY.set((user.uid, socket));
}

/// The variables that sessions of `user` get for the agent's display (if it runs one
/// for them).
pub fn session_environment(user: &User) -> Vec<(String, String)> {
    let Some((_, socket)) = AGENT_DISPLAY.get().filter(|(uid, _)| *uid == user.uid) else {
        return Vec::new();
    };
    vec![
        ("XDG_RUNTIME_DIR".into(), runtime_dir(user)),
        // (Wayland clients take a path, too.)
        ("WAYLAND_DISPLAY".into(), socket.clone()),
    ]
}

/// `midgard-helper waypipe [<socket>]`: runs the server (as whoever runs us) on the socket
/// (default: `$XDG_RUNTIME_DIR/wayland-0`). It runs as long as its command, which is us
/// in a mode that waits forever (see [`pause`]).
pub fn server(socket: Option<String>) -> Result<(), String> {
    let socket = match socket {
        Some(socket) => socket,
        None => {
            let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
            format!("{dir}/{WAYLAND_DISPLAY}")
        }
    };
    if UnixStream::connect(&socket).is_ok() {
        return Err(format!("{socket} is in use"));
    }
    // Left over from a server that is gone.
    let _ = std::fs::remove_file(&socket);
    let mut command = std::process::Command::new(WAYPIPE_PATH);
    if Path::new(WAYPIPE_DEBUG_PATH).exists() {
        command.arg("--debug");
    }
    command
        .arg("--vsock")
        .arg("--socket")
        .arg(format!("{HOST_CID}:{WAYPIPE_PORT}"))
        // midgard's waypipe has no compression, and the host only takes shared memory.
        .args(["--compress", "none", "--no-gpu", "--display"])
        .arg(&socket)
        .args(["server", "--", AGENT_PATH, "pause"]);
    // (exec() restores SIGPIPE, which Rust ignores.)
    let error = command.exec();
    Err(format!("{WAYPIPE_PATH}: {error}"))
}

/// `midgard-agent pause`: the server's command.
pub fn pause() -> ! {
    loop {
        // SAFETY: pause() has no preconditions.
        unsafe { libc::pause() };
    }
}
