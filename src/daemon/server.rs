//! The service's Unix socket: one task per connected player window.

use super::Daemon;
use crate::protocol::{
    self, ClientMessage, Event, Incoming, ServerMessage, read_line, write_message,
};
use std::{path::Path, sync::Arc};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{broadcast::error::RecvError, mpsc},
};

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
    let (outgoing, mut queued) = mpsc::unbounded_channel::<ServerMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if write_message(&mut write, &message).await.is_err() {
                return;
            }
        }
    });

    // Subscribed before the snapshot is taken, so nothing falls in between.
    let mut events = daemon.subscribe();
    let _ = outgoing.send(ServerMessage::Event(Event::Snapshot(daemon.snapshot())));
    let forward = {
        let (daemon, outgoing) = (daemon.clone(), outgoing.clone());
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    // A window too slow to keep up gets the whole state again.
                    Err(RecvError::Lagged(_)) => Event::Snapshot(daemon.snapshot()),
                    Err(RecvError::Closed) => return,
                };
                if outgoing.send(ServerMessage::Event(event)).is_err() {
                    return;
                }
            }
        })
    };

    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        match read_line(&mut reader, &mut line).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                log::warn!("cannot read from a window: {error}");
                break;
            }
        }
        let message = match protocol::parse::<ClientMessage>(&line) {
            Ok(Incoming::Message(message)) => message,
            // A newer window asking for something this service doesn't know yet:
            // answer it, and keep the connection.
            Ok(Incoming::Unknown { id, error }) => {
                log::warn!("a window sent a request this service doesn't know: {error}");
                if let Some(id) = id {
                    let _ = outgoing.send(ServerMessage::Reply {
                        id,
                        result: Err(too_old()),
                    });
                }
                continue;
            }
            Err(error) => {
                log::warn!("dropping a window that sent something unreadable: {error}");
                break;
            }
        };
        // Requests run side by side; a slow lookup doesn't hold up a pause.
        let (daemon, outgoing) = (daemon.clone(), outgoing.clone());
        tokio::spawn(async move {
            let result = daemon.handle(message.request).await;
            let _ = outgoing.send(ServerMessage::Reply {
                id: message.id,
                result,
            });
        });
    }
    forward.abort();
    drop(outgoing);
    // The writer ends once in-flight replies are written or dropped.
    let _ = writer.await;
}

fn too_old() -> String {
    format!(
        "The spotitui service doesn't know this request; it is older than this window \
         (protocol {}). Restart it: systemctl --user restart spotitui",
        protocol::VERSION
    )
}
