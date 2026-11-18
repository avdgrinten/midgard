//! The guest configuration's host side: the guest fetches it from us over vsock (see
//! [`midgard_protocol::guest::fetch_config`]), as often as it wants (e.g., again when
//! its agent starts again).

use std::io;
use std::time::Duration;

use async_executor::Executor;
use async_io::Timer;
use futures_lite::{AsyncWriteExt, future};
use midgard_protocol::guest::{GuestConfig, config_answer};
use midgard_protocol::nonblocking::read_message;
use midgard_protocol::version::{Hello, Versions};

use crate::logging::log;
use crate::virtio::vsock::{VsockListener, VsockStream};

/// How long a guest may take to say what it wants once it connected (others wait
/// meanwhile).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Answers the guest's requests for `config` on `listener`.
pub(crate) fn start(executor: &Executor<'static>, listener: VsockListener, config: GuestConfig) {
    let serve = async move {
        loop {
            let stream = listener.accept().await;
            let answered = future::or(answer(stream, &config), async {
                Timer::after(REQUEST_TIMEOUT).await;
                Err(io::ErrorKind::TimedOut.into())
            });
            if let Err(error) = answered.await {
                log!("failed to pass the guest configuration to the guest: {error}");
            }
        }
    };
    executor.spawn(serve).detach();
}

async fn answer(mut stream: VsockStream, config: &GuestConfig) -> io::Result<()> {
    let guest: Hello = read_message(&mut stream)
        .await?
        .ok_or(io::ErrorKind::UnexpectedEof)?;
    // (The guest says so on its console, too, and gives up.)
    if let Err(mismatch) = Versions::OURS.negotiate(guest.protocol) {
        log!("the guest gets no guest configuration: its agent speaks {mismatch}");
    }
    stream.write_all(&config_answer(guest, config)?).await?;
    stream.flush().await?;
    stream.shutdown_write();
    Ok(())
}
