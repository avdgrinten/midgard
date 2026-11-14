//! Waiting for the distribution's init to finish booting (see [`Ready`]), such that
//! sessions find its services running (e.g., D-Bus). The watcher ([`watch`]) learns it
//! in the way that the guest configuration says; sessions [`wait`] for it.

use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_channel::Sender;
use async_io::{Async, Timer};
use event_listener::Event;
use futures_lite::future;
use midgard_protocol::guest::Ready;

use super::{PATH, RUN_DIR, log, process, rt};

/// How long sessions wait for the distribution's init; then they run anyway. (And how
/// long `midgard-init ready` waits for the agent.)
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// The socket that `midgard-init ready` connects to (see [`Watcher`]).
const NOTIFY_SOCKET: &str = "/run/midgard/notify";

/// Whether the guest finished booting (or we gave up waiting).
static BOOTED: AtomicBool = AtomicBool::new(false);
static BOOTED_EVENT: Event = Event::new();

/// Waits until the guest finished booting.
pub async fn wait() {
    loop {
        if BOOTED.load(Ordering::SeqCst) {
            return;
        }
        let listener = BOOTED_EVENT.listen();
        if BOOTED.load(Ordering::SeqCst) {
            return;
        }
        listener.await;
    }
}

/// What the watcher waits for, and the socket that `midgard-init ready` connects to. The
/// socket exists in every mode, such that `midgard-init ready` does not wait for nothing
/// (e.g., if the guest runs it although the configuration does not say so).
pub struct Watcher {
    ready: Ready,
    listener: Option<UnixListener>,
}

impl Watcher {
    pub fn new(ready: Ready) -> Self {
        // Left over from an agent that is gone.
        let _ = std::fs::remove_file(NOTIFY_SOCKET);
        let listener = UnixListener::bind(NOTIFY_SOCKET)
            .inspect_err(|error| log!("listening on {NOTIFY_SOCKET}: {error}"))
            .ok();
        Self { ready, listener }
    }
}

/// Watches the guest boot (from the agent's start); see [`wait`]. `then` runs once the
/// guest booted, before sessions continue.
pub async fn watch(watcher: Watcher, then: impl Future<Output = ()>) {
    let start = Instant::now();
    let (notify, notified) = async_channel::bounded(1);
    if let Some(listener) = watcher.listener {
        rt::spawn(accept(listener, notify)).detach();
    }
    let booted = match watcher.ready {
        Ready::Immediately => None,
        Ready::Systemd => {
            log!("waiting for systemd to finish booting");
            Some(systemd_booted(start + BOOT_TIMEOUT).await)
        }
        Ready::Notify => {
            log!("waiting for the guest to run {RUN_DIR}/midgard-init ready");
            let notified = async { notified.recv().await.is_ok() };
            Some(
                future::or(notified, async {
                    Timer::after(BOOT_TIMEOUT).await;
                    false
                })
                .await,
            )
        }
    };
    match booted {
        Some(true) => log!("booted after {} ms", start.elapsed().as_millis()),
        // (systemd also reports a degraded system, i.e., failed units, as an error.)
        Some(false) => log!(
            "the guest did not finish booting within {} s, or some services failed",
            BOOT_TIMEOUT.as_secs()
        ),
        None => {}
    }
    then.await;
    BOOTED.store(true, Ordering::SeqCst);
    BOOTED_EVENT.notify(usize::MAX);
}

async fn systemd_booted(deadline: Instant) -> bool {
    // Until systemd listens on its private socket, systemctl fails instead of waiting.
    while !Path::new("/run/systemd/private").exists() {
        if Instant::now() > deadline {
            return false;
        }
        Timer::after(Duration::from_millis(50)).await;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    run_with_timeout(&["systemctl", "is-system-running", "--wait"], remaining).await
}

/// Accepts (and closes) the connections of `midgard-init ready` for as long as the agent
/// runs; the first one goes to `notify` (for [`Ready::Notify`]).
async fn accept(listener: UnixListener, notify: Sender<()>) {
    let listener = match Async::new(listener) {
        Ok(listener) => listener,
        Err(error) => {
            log!("{NOTIFY_SOCKET}: {error}");
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok(_) => {
                let _ = notify.try_send(());
            }
            Err(error) => {
                log!("{NOTIFY_SOCKET}: {error}");
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Runs a program (in PATH) and returns whether it exited with 0 within the timeout.
async fn run_with_timeout(argv: &[&str], timeout: Duration) -> bool {
    let Some(path) = process::resolve(argv[0], PATH) else {
        log!("{}: command not found", argv[0]);
        return false;
    };
    let argv = argv.iter().map(|a| a.to_string()).collect();
    let command = process::Command::new(path, argv, vec![format!("PATH={PATH}")]);
    let result = process::dev_null().and_then(|null| {
        let null = null.as_raw_fd();
        let stdio = process::Stdio::Pipes {
            stdin: null,
            stdout: null,
            stderr: null,
        };
        process::spawn(&command, &stdio)
    });
    match result {
        Ok(child) => {
            let status = future::or(async { child.wait().await.ok() }, async {
                Timer::after(timeout).await;
                None
            })
            .await;
            if status.is_none() {
                child.kill(libc::SIGKILL);
                let _ = child.wait().await;
            }
            matches!(status, Some((Some(0), _)))
        }
        Err(error) => {
            log!("{error}");
            false
        }
    }
}

/// `midgard-init ready`: tells the agent that the guest finished booting. If the agent
/// does not listen yet, we wait for it, but not forever: the guest may wait for us while
/// it boots.
pub fn notify() -> Result<(), String> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        match UnixStream::connect(NOTIFY_SOCKET) {
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("{NOTIFY_SOCKET}: {error}")),
        }
    }
}
