//! The player window's connection to the background service. Requests are
//! answered asynchronously; events arrive through the callback given to
//! [`Service::connect`]. If the service isn't running, it is started, and if
//! it restarts, the connection is made again.

use crate::{
    protocol::{
        self, ClientMessage, Event, Failure, Incoming, Kind, Request, Response, ServerMessage,
        Snapshot, read_line, write_message,
    },
    spotify::{Lyrics, NowTrack, Playlist, Track},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::UnixStream,
    sync::{mpsc, oneshot},
};

const UNIT: &str = "spotitui.service";
/// How often to try the socket while the service starts.
const RETRY: Duration = Duration::from_millis(250);
/// Attempts before telling the window the service won't come up (it keeps trying).
const PATIENCE: u32 = 40;
/// How long a one-off request from the command line waits for its answer.
const ONCE_TIMEOUT: Duration = Duration::from_secs(3);

pub enum Notification {
    Connected,
    Disconnected(String),
    Event(Event),
}

type Reply = Result<Response, Failure>;

struct Outgoing {
    request: Request,
    reply: oneshot::Sender<Reply>,
}

#[derive(Clone)]
pub struct Service {
    requests: mpsc::UnboundedSender<Outgoing>,
}

impl Service {
    /// Starts connecting in the background. Must run inside the Tokio runtime.
    pub fn connect(notify: impl Fn(Notification) + Send + Sync + 'static) -> Self {
        let (requests, queue) = mpsc::unbounded_channel();
        tokio::spawn(maintain(queue, notify));
        Self { requests }
    }

    pub async fn request(&self, request: Request) -> Reply {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Outgoing { request, reply })
            .map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }

    pub async fn done(&self, request: Request) -> Result<(), Failure> {
        match self.request(request).await? {
            Response::Done => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn playlists(&self) -> Result<Vec<Playlist>, Failure> {
        match self.request(Request::Playlists).await? {
            Response::Playlists(playlists) => Ok(playlists),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn uris(&self, request: Request) -> Result<Vec<String>, Failure> {
        match self.request(request).await? {
            Response::Uris(uris) => Ok(uris),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn tracks(&self, uris: Vec<String>) -> Result<Vec<Track>, Failure> {
        match self.request(Request::Tracks(uris)).await? {
            Response::Tracks(tracks) => Ok(tracks),
            other => Err(unexpected(&other)),
        }
    }

    /// Saves a track to Liked Songs; false when it was already there.
    pub async fn like(&self, uri: String) -> Result<bool, Failure> {
        match self.request(Request::Like(uri)).await? {
            Response::Liked { added } => Ok(added),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn lyrics(&self, track: NowTrack) -> Result<Option<Lyrics>, Failure> {
        match self.request(Request::Lyrics(track)).await? {
            Response::Lyrics(lyrics) => Ok(lyrics),
            other => Err(unexpected(&other)),
        }
    }
}

fn stopped() -> Failure {
    Failure::new(
        Kind::ServiceDown,
        "The spotitui service stopped; reconnecting…",
    )
}

fn not_running() -> Failure {
    Failure::new(Kind::ServiceDown, "The spotitui service isn't running yet")
}

fn unreadable() -> Failure {
    Failure::new(
        Kind::Unsupported,
        "The spotitui service answered in a newer format; restart this window",
    )
}

fn unexpected(response: &Response) -> Failure {
    Failure::new(
        Kind::Other,
        format!("The spotitui service answered unexpectedly ({response:?})"),
    )
}

/// Keeps a connection to the service for as long as the window lives.
async fn maintain(
    mut queue: mpsc::UnboundedReceiver<Outgoing>,
    notify: impl Fn(Notification) + Send + Sync + 'static,
) {
    let Some(path) = protocol::socket_path() else {
        notify(Notification::Disconnected("XDG_RUNTIME_DIR is not set".into()));
        return;
    };
    let mut started = false;
    let mut attempts = 0;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                attempts = 0;
                notify(Notification::Connected);
                if !session(stream, &mut queue, &notify).await {
                    return;
                }
                notify(Notification::Disconnected(stopped().message));
            }
            Err(_) => {
                if !started {
                    start_service();
                    started = true;
                }
                attempts += 1;
                if attempts == PATIENCE {
                    notify(Notification::Disconnected(
                        "The spotitui service doesn't start; see `journalctl --user -u spotitui`"
                            .into(),
                    ));
                }
                // Requests can't wait for a service that may never come.
                while let Ok(pending) = queue.try_recv() {
                    let _ = pending.reply.send(Err(not_running()));
                }
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Serves one connection; returns false once the window is gone.
async fn session(
    stream: UnixStream,
    queue: &mut mpsc::UnboundedReceiver<Outgoing>,
    notify: &(impl Fn(Notification) + Send + Sync),
) -> bool {
    let (read, mut write) = stream.into_split();
    // Reading runs in its own task: a line read cut short by select! would lose data.
    let (incoming_tx, mut incoming) = mpsc::unbounded_channel::<ServerMessage>();
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        while let Ok(true) = read_line(&mut reader, &mut line).await {
            let message = match protocol::parse::<ServerMessage>(&line) {
                Ok(Incoming::Message(message)) => message,
                // From a newer service: an answer this window can't read fails its
                // request; an unknown event is skipped.
                Ok(Incoming::Unknown { id: Some(id), error }) => {
                    log::warn!("unreadable answer from the service: {error}");
                    ServerMessage::Reply {
                        id,
                        result: Err(unreadable()),
                    }
                }
                Ok(Incoming::Unknown { id: None, error }) => {
                    log::warn!("skipping an event from the service: {error}");
                    continue;
                }
                Err(error) => {
                    log::warn!("the service sent something unreadable: {error}");
                    return;
                }
            };
            if incoming_tx.send(message).is_err() {
                return;
            }
        }
    });
    let mut waiting: HashMap<u64, oneshot::Sender<Reply>> = HashMap::new();
    let mut next_id = 0;
    let window_alive = loop {
        tokio::select! {
            pending = queue.recv() => {
                let Some(Outgoing { request, reply }) = pending else {
                    break false;
                };
                next_id += 1;
                let message = ClientMessage { id: next_id, request };
                if write_message(&mut write, &message).await.is_err() {
                    let _ = reply.send(Err(stopped()));
                    break true;
                }
                waiting.insert(next_id, reply);
            }
            message = incoming.recv() => match message {
                Some(ServerMessage::Reply { id, result }) => {
                    if let Some(reply) = waiting.remove(&id) {
                        let _ = reply.send(result);
                    }
                }
                Some(ServerMessage::Event(event)) => notify(Notification::Event(event)),
                None => break true,
            },
        }
    };
    reader.abort();
    for (_, reply) in waiting {
        let _ = reply.send(Err(stopped()));
    }
    window_alive
}

/// Starts the service: through systemd when its unit is installed, else directly.
fn start_service() {
    let quiet = |command: &mut Command| {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    };
    if unit_installed() && quiet(Command::new("systemctl").args(["--user", "start", UNIT])) {
        return;
    }
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    // setsid detaches it, so closing this terminal doesn't stop the music.
    quiet(Command::new("setsid").arg("-f").arg(program).arg("daemon"));
}

fn unit_installed() -> bool {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")));
    config.is_some_and(|config| config.join("systemd/user").join(UNIT).exists())
}

/// Tells a running service to forget the login; quiet if none runs.
pub async fn logout() {
    let _ = once(|_| Request::Logout).await;
}

/// Sends one request to a running service and waits for its answer, for the
/// command line (`spotitui next` from a desktop shortcut). The request is made
/// from the snapshot the service sends first. A service that isn't running is
/// not started: there's nothing playing to control.
pub async fn once(request: impl FnOnce(&Snapshot) -> Request) -> Result<(), Failure> {
    let gone = || Failure::new(Kind::ServiceDown, "The spotitui service isn't running");
    let path = protocol::socket_path().ok_or_else(gone)?;
    let stream = UnixStream::connect(&path).await.map_err(|_| gone())?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let exchange = async {
        let snapshot = loop {
            if !read_line(&mut reader, &mut line).await.unwrap_or(false) {
                return Err(gone());
            }
            if let Ok(Incoming::Message(ServerMessage::Event(Event::Snapshot(snapshot)))) =
                protocol::parse::<ServerMessage>(&line)
            {
                break snapshot;
            }
        };
        let message = ClientMessage {
            id: 1,
            request: request(&snapshot),
        };
        write_message(&mut write, &message)
            .await
            .map_err(|_| gone())?;
        while let Ok(true) = read_line(&mut reader, &mut line).await {
            match protocol::parse::<ServerMessage>(&line) {
                Ok(Incoming::Message(ServerMessage::Reply { id: 1, result })) => {
                    return result.map(|_| ());
                }
                Ok(Incoming::Unknown { id: Some(1), .. }) => {
                    return Err(Failure::new(
                        Kind::Unsupported,
                        "The spotitui service answered in a newer format",
                    ));
                }
                _ => {}
            }
        }
        Err(gone())
    };
    tokio::time::timeout(ONCE_TIMEOUT, exchange)
        .await
        .map_err(|_| {
            Failure::new(
                Kind::Other,
                "The spotitui service didn't answer; see `journalctl --user -u spotitui`",
            )
        })?
}
