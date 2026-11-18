//! What the agent sets up in the guest as the guest configuration says.

use std::os::fd::AsRawFd;
use std::time::Duration;

use async_io::Timer;
use futures_lite::future;
use midgard_protocol::guest::GuestConfig;

use super::{PATH, log, process};

/// How long the user setup command may take; then sessions run anyway.
const USER_SETUP_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs the user setup command (see [`GuestConfig::user_setup`]) once the guest booted,
/// before sessions run. Its output goes to the console.
pub async fn user_setup(config: &GuestConfig) {
    let (Some(user), Some(setup)) = (&config.user, &config.user_setup) else {
        return;
    };
    let Some(program) = setup.first() else {
        return;
    };
    let Some(path) = process::resolve(program, PATH) else {
        log!("{program}: command not found");
        return;
    };
    log!("setting up user {user}");
    let mut argv = setup.clone();
    argv.push(user.clone());
    let env = vec![format!("PATH={PATH}"), "HOME=/root".to_string()];
    let command = process::Command::new(path, argv, env);
    let child = process::dev_null().and_then(|null| {
        let stdio = process::Stdio::Pipes {
            stdin: null.as_raw_fd(),
            stdout: libc::STDERR_FILENO,
            stderr: libc::STDERR_FILENO,
        };
        process::spawn(&command, &stdio)
    });
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            log!("running {program}: {error}");
            return;
        }
    };
    let status = future::or(async { Some(child.wait().await) }, async {
        Timer::after(USER_SETUP_TIMEOUT).await;
        None
    });
    match status.await {
        Some(Ok((Some(0), _))) => {}
        Some(Ok((Some(code), _))) => log!("{program} failed (exit status {code})"),
        Some(Ok((None, signal))) => log!("{program} was killed (signal {signal:?})"),
        Some(Err(error)) => log!("waiting for {program}: {error}"),
        None => {
            log!(
                "{program} did not finish within {} s",
                USER_SETUP_TIMEOUT.as_secs()
            );
            // (With what it started: the command is the leader of its own session.)
            // SAFETY: kill() only takes numbers.
            unsafe { libc::kill(-child.pid, libc::SIGKILL) };
            let _ = child.wait().await;
        }
    }
}
