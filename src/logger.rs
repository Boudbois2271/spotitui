//! Logging. The player window owns its terminal, so it logs only to a file, and
//! only when `SPOTITUI_LOG` names one. The service logs to stderr, which systemd
//! keeps in the journal (`journalctl --user -u spotitui`).

use std::{fs::File, io::Write, sync::Mutex, time::SystemTime};

/// Libraries that only say something useful at error level. symphonia, which
/// decodes the audio, warns for every frame it can't read: thousands a minute
/// while Spotify withholds decryption keys and librespot plays the encrypted file.
const NOISY: &[&str] = &["symphonia"];

struct Logger {
    output: Mutex<Box<dyn Write + Send>>,
    /// The level for spotitui's own messages.
    level: log::LevelFilter,
    /// The level for libraries (librespot is chatty at info).
    others: log::LevelFilter,
    timestamps: bool,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let target = metadata.target();
        let limit = if target.starts_with("spotitui") {
            self.level
        } else if NOISY.iter().any(|noisy| target.starts_with(noisy)) {
            self.others.min(log::LevelFilter::Error)
        } else {
            self.others
        };
        metadata.level() <= limit
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let Ok(mut output) = self.output.lock() else {
            return;
        };
        let _ = if self.timestamps {
            let seconds = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64());
            writeln!(
                output,
                "{seconds:.3} {:<5} {}: {}",
                record.level(),
                record.target(),
                record.args()
            )
        } else {
            // The journal adds its own timestamps.
            writeln!(
                output,
                "{:<5} {}: {}",
                record.level(),
                record.target(),
                record.args()
            )
        };
    }

    fn flush(&self) {
        if let Ok(mut output) = self.output.lock() {
            let _ = output.flush();
        }
    }
}

/// `SPOTITUI_LOG_LEVEL`, if set, applies to every message.
fn configured_level() -> Option<log::LevelFilter> {
    std::env::var("SPOTITUI_LOG_LEVEL")
        .ok()
        .and_then(|level| level.parse().ok())
}

fn log_file() -> Option<File> {
    let path = std::env::var_os("SPOTITUI_LOG")?;
    File::options().create(true).append(true).open(path).ok()
}

fn install(logger: Logger) {
    let max = logger.level.max(logger.others);
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(max);
    }
}

/// For the player window: a file log when `SPOTITUI_LOG` is set, else nothing.
pub fn init() {
    let Some(file) = log_file() else {
        return;
    };
    let level = configured_level().unwrap_or(log::LevelFilter::Info);
    install(Logger {
        output: Mutex::new(Box::new(file)),
        level,
        others: level,
        timestamps: true,
    });
}

/// For the service: stderr (the journal), or the `SPOTITUI_LOG` file if set.
pub fn init_daemon() {
    let configured = configured_level();
    let level = configured.unwrap_or(log::LevelFilter::Info);
    let others = configured.unwrap_or(log::LevelFilter::Warn);
    let (output, timestamps): (Box<dyn Write + Send>, bool) = match log_file() {
        Some(file) => (Box::new(file), true),
        None => (Box::new(std::io::stderr()), false),
    };
    install(Logger {
        output: Mutex::new(output),
        level,
        others,
        timestamps,
    });
}

#[cfg(test)]
mod tests {
    use super::Logger;
    use log::{Level, LevelFilter, Log};
    use std::sync::Mutex;

    #[test]
    fn symphonia_only_speaks_up_for_errors() {
        let logger = Logger {
            output: Mutex::new(Box::new(std::io::sink())),
            level: LevelFilter::Debug,
            others: LevelFilter::Debug,
            timestamps: false,
        };
        let enabled = |target, level| {
            logger.enabled(&log::Metadata::builder().target(target).level(level).build())
        };
        assert!(enabled("spotitui::daemon", Level::Debug));
        assert!(enabled("librespot_playback::player", Level::Warn));
        assert!(!enabled("symphonia_bundle_mp3::demuxer", Level::Warn));
        assert!(enabled("symphonia_core::probe", Level::Error));
    }
}
