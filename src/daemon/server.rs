//! The service's Unix socket: one task per connected player window.

use super::Daemon;
use crate::protocol::{
    self, ClientMessage, Event, Incoming, ServerMessage, read_line, write_message,
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{
        Notify,
        broadcast::error::RecvError,
        mpsc::{self, error::TrySendError},
    },
};

/// Messages waiting for a window. One that stops reading (a suspended process)
/// is dropped once this many pile up; it gets the whole state on reconnecting.
const QUEUE_LIMIT: usize = 1024;
/// How long a closing window's last answers may take to write.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Queues a message for a window, or reports that it stopped reading.
fn push(outgoing: &mpsc::Sender<ServerMessage>, message: ServerMessage, overflow: &Notify) -> bool {
    match outgoing.try_send(message) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            overflow.notify_one();
            false
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

/// Binds the socket; `None` if another service already answers on it.
pub async fn bind(path: &Path) -> std::io::Result<Option<UnixListener>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(None);
        }
        // Left behind by a service that didn't stop cleanly.
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path).map(Some)
}

pub async fn accept(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve_window(daemon.clone(), stream));
            }
            Err(error) => {
                log::warn!("cannot accept a window: {error}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Answers a window's requests and pushes every event to it until it closes.
async fn serve_window(daemon: Arc<Daemon>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let (outgoing, mut queued) = mpsc::channel::<ServerMessage>(QUEUE_LIMIT);
    let overflow = Arc::new(Notify::new());
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if write_message(&mut write, &message).await.is_err() {
                return;
            }
        }
    });

    // Subscribed before the snapshot is taken, so nothing falls in between.
    let mut events = daemon.subscribe();
    push(
        &outgoing,
        ServerMessage::Event(Event::Snapshot(daemon.snapshot())),
        &overflow,
    );
    let forward = {
        let (daemon, outgoing, overflow) = (daemon.clone(), outgoing.clone(), overflow.clone());
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    // A window too slow to keep up gets the whole state again.
                    Err(RecvError::Lagged(_)) => Event::Snapshot(daemon.snapshot()),
                    Err(RecvError::Closed) => return,
                };
                if !push(&outgoing, ServerMessage::Event(event), &overflow) {
                    return;
                }
            }
        })
    };

    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let stalled = loop {
        let read = tokio::select! {
            read = read_line(&mut reader, &mut line) => read,
            () = overflow.notified() => {
                log::warn!("dropping a window that stopped reading");
                break true;
            }
        };
        match read {
            Ok(true) => {}
            Ok(false) => break false,
            Err(error) => {
                log::warn!("cannot read from a window: {error}");
                break false;
            }
        }
        let message = match protocol::parse::<ClientMessage>(&line) {
            Ok(Incoming::Message(message)) => message,
            // A newer window asking for something this service doesn't know yet:
            // answer it, and keep the connection.
            Ok(Incoming::Unknown { id, error }) => {
                log::warn!("a window sent a request this service doesn't know: {error}");
                if let Some(id) = id {
                    let reply = ServerMessage::Reply {
                        id,
                        result: Err(too_old()),
                    };
                    push(&outgoing, reply, &overflow);
                }
                continue;
            }
            Err(error) => {
                log::warn!("dropping a window that sent something unreadable: {error}");
                break false;
            }
        };
        // Requests run side by side; a slow lookup doesn't hold up a pause.
        let (daemon, outgoing, overflow) = (daemon.clone(), outgoing.clone(), overflow.clone());
        tokio::spawn(async move {
            let result = daemon.handle(message.request).await;
            let reply = ServerMessage::Reply {
                id: message.id,
                result,
            };
            push(&outgoing, reply, &overflow);
        });
    };
    forward.abort();
    drop(outgoing);
    // The writer ends once in-flight replies are written or dropped; a window
    // that stopped reading would keep it waiting forever.
    let abort = writer.abort_handle();
    if stalled || tokio::time::timeout(FLUSH_TIMEOUT, writer).await.is_err() {
        abort.abort();
    }
}

fn too_old() -> protocol::Failure {
    protocol::Failure::new(
        protocol::Kind::Unsupported,
        format!(
        "The spotitui service doesn't know this request; it is older than this window \
         (protocol {}). Restart it: systemctl --user restart spotitui",
            protocol::VERSION
        ),
    )
}
