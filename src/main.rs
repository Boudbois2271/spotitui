mod app;
mod client;
mod daemon;
mod logger;
mod protocol;
mod spotify;
mod theme;
mod ui;

use app::{App, Msg};
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{Stdout, Write, stdout},
    process::ExitCode,
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};

const HELP: &str = "\
Play Spotify in your terminal, in your Omarchy theme.

Usage:
  spotitui            open the player (logs in through your browser the first time)
  spotitui daemon     run the background service that plays the music; the player
                      starts it when needed, and systemd at login when installed
  spotitui logout     remove your Spotify login from this computer
  spotitui --version  print the version

Closing the player leaves the music playing; the service keeps going.

Set SPOTITUI_LOG=/path/to/file to write a debug log.
Playback needs Spotify Premium. Press ? in the player for keys.";

const TICK: Duration = Duration::from_millis(250);

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // stderr may be gone after a hangup; that must not turn into a panic.
            let _ = writeln!(std::io::stderr(), "spotitui: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => run(),
        ["daemon"] => daemon::run(),
        ["logout"] => {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    spotify::forget().await?;
                    // A running service lets go of the session too.
                    client::logout().await;
                    Ok::<(), spotify::Error>(())
                })?;
            println!("Logged out. Your Spotify login is removed from this computer.");
            Ok(())
        }
        ["--version" | "-V"] => {
            println!("spotitui {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["--help" | "-h"] => {
            println!("{HELP}");
            Ok(())
        }
        _ => Err("Unknown arguments; run spotitui --help for usage".into()),
    }
}

fn run() -> Result<()> {
    logger::init();
    // One thread is plenty for a window that mostly waits.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(play());
    runtime.shutdown_timeout(Duration::from_millis(200));
    result
}

async fn play() -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let service = {
        let tx = tx.clone();
        client::Service::connect(move |notification| {
            let _ = tx.send(Msg::Service(notification));
        })
    };
    let mut app = App::new(
        tx,
        spotify::http(),
        theme::ThemeSource::new(),
        service,
    );

    let mut terminal = setup_terminal()?;
    // Puts the terminal back however the loop ends, a panic included.
    let _restore = Restore;
    // The message is kept for after the terminal is back. A panic in a
    // background task doesn't end the window, so it must not touch the terminal.
    std::panic::set_hook(Box::new(|info| {
        let text = info.to_string();
        log::error!("{text}");
        if let Ok(mut last) = LAST_PANIC.lock() {
            *last = Some(text);
        }
    }));

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    let mut redraw = true;
    loop {
        if redraw {
            if let Err(error) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
                break Err(error.into());
            }
            // The frame measured the list; load the rows it can now show.
            app.load_more();
        }
        redraw = true;
        tokio::select! {
            event = events.next() => match event {
                // The terminal reports every mouse movement; nothing reacts to them.
                Some(Ok(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    ..
                }))) => redraw = false,
                Some(Ok(event)) => app.input(&event),
                Some(Err(error)) => break Err(error.into()),
                None => break Ok(()),
            },
            Some(msg) = rx.recv() => app.handle(msg),
            _ = tick.tick() => app.tick(),
            _ = terminate.recv() => app.quit = true,
            _ = hangup.recv() => app.quit = true,
        }
        if app.quit {
            break Ok(());
        }
    }
}

static LAST_PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore_terminal();
        if std::thread::panicking()
            && let Some(text) = LAST_PANIC.lock().ok().and_then(|mut last| last.take())
        {
            let _ = writeln!(std::io::stderr(), "spotitui crashed: {text}");
        }
    }
}

fn setup_terminal() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// Puts the terminal back. After a hangup the terminal is gone, so failures are ignored.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}
