//! The speaker's audio output: a PulseAudio stream (PipeWire on Omarchy) that
//! changes tracks without leftovers of the one before.
//!
//! librespot's own PulseAudio output keeps its stream open from one track to
//! the next, so tracks that run into each other play without a gap. On a skip
//! the next track still has to load, the stream runs dry meanwhile and the old
//! track stops mid-sound, with a click. And PipeWire, converting librespot's
//! 44.1 kHz to the 48 kHz the device runs at, keeps the last few milliseconds
//! of the old track and plays them when sound comes again: a blip of the old
//! track just before the new one. So here the old track fades out when the next
//! one has to load (or on a pause), and the stream starts afresh after a break.

use libpulse_binding::{error::PAErr, sample, stream::Direction};
use libpulse_simple_binding::Simple;
use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long the old track takes to fade out.
const FADE: Duration = Duration::from_millis(15);
/// No sound written for this long means the track stopped, as for a skip or a
/// seek: while playing, the next sound comes within a millisecond or two.
const BREAK: Duration = Duration::from_millis(40);
/// A stream this close to playing all it was given counts as run dry; what is
/// left of it plays before a new one starts.
const NEARLY_DRY: Duration = Duration::from_millis(20);

/// The output, shared by the player, which writes the sound, and the speaker,
/// which says when the track stops for another to load.
pub struct Output {
    stream: Mutex<Stream>,
}

struct Stream {
    pulse: Option<Simple>,
    /// The last sound written, to fade out from.
    tail: Vec<i16>,
    /// The fade already went out after the last sound written.
    faded: bool,
    /// When the player last wrote sound.
    written: Option<Instant>,
    /// When the last sound written will have been heard.
    heard: Option<Instant>,
}

impl Output {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stream: Mutex::new(Stream {
                pulse: None,
                tail: Vec::new(),
                faded: false,
                written: None,
                heard: None,
            }),
        })
    }

    /// The sink the player writes to.
    pub fn sink(self: &Arc<Self>) -> Box<dyn Sink> {
        Box::new(OutputSink(self.clone()))
    }

    /// Fades out what plays now: the player stopped writing for another track
    /// to load, and the stream would otherwise run dry mid-sound.
    pub fn fade_out(&self) {
        let mut stream = self.lock();
        if let Err(error) = stream.fade_out() {
            log::debug!("cannot fade out: {error}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Stream> {
        self.stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Stream {
    fn fade_out(&mut self) -> Result<(), PAErr> {
        let Some(pulse) = &self.pulse else {
            return Ok(());
        };
        if self.faded || self.tail.is_empty() {
            return Ok(());
        }
        self.faded = true;
        pulse.write(&bytes(&fade(&self.tail)))?;
        self.wrote();
        Ok(())
    }

    fn wrote(&mut self) {
        // The latency is how long until the last sound written is heard.
        self.heard = self
            .pulse
            .as_ref()
            .and_then(|pulse| pulse.get_latency().ok())
            .map(|latency| Instant::now() + Duration::from_micros(latency.0));
    }

    /// The player stopped writing for so long that the stream played all it
    /// had. A stream that runs dry keeps the last bit of the old sound in
    /// PipeWire's converter, to play it when sound comes again.
    fn ran_dry(&self) -> bool {
        let now = Instant::now();
        self.written.is_some_and(|written| now - written >= BREAK)
            && self.heard.is_some_and(|heard| now + NEARLY_DRY >= heard)
    }

    /// A new stream in place of one that ran dry, once what is left has played.
    /// Without a new one the old one goes on.
    fn restart(&mut self) {
        if let Some(heard) = self.heard {
            std::thread::sleep(heard.saturating_duration_since(Instant::now()));
        }
        match open() {
            Ok(pulse) => {
                self.pulse = Some(pulse);
                self.heard = None;
                if let Some(written) = self.written {
                    log::debug!(
                        "audio output restarts after a {} ms break",
                        written.elapsed().as_millis()
                    );
                }
            }
            Err(error) => log::warn!("cannot restart the audio output: {error}"),
        }
    }
}

struct OutputSink(Arc<Output>);

impl Sink for OutputSink {
    fn start(&mut self) -> SinkResult<()> {
        let mut stream = self.0.lock();
        if stream.pulse.is_none() {
            stream.pulse = Some(open()?);
            stream.tail.clear();
            stream.faded = false;
            stream.written = None;
            stream.heard = None;
        }
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        let mut stream = self.0.lock();
        // A pause stops the track mid-sound too. librespot exits when stopping
        // fails, so only a stream that can't play out fails.
        if let Err(error) = stream.fade_out() {
            log::debug!("cannot fade out: {error}");
        }
        let Some(pulse) = stream.pulse.take() else {
            return Ok(());
        };
        pulse.drain().map_err(|error| {
            SinkError::OnWrite(format!("<PulseAudio> cannot play out the stream: {error}"))
        })
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let mut stream = self.0.lock();
        if stream.pulse.is_none() {
            return Err(SinkError::NotConnected("<PulseAudio> no stream".into()));
        }
        if stream.ran_dry() {
            stream.restart();
        }
        let Stream {
            pulse, tail, faded, ..
        } = &mut *stream;
        let pulse = pulse
            .as_ref()
            .ok_or_else(|| SinkError::NotConnected("<PulseAudio> no stream".into()))?;
        match packet {
            AudioPacket::Samples(samples) => {
                let samples = converter.f64_to_s16(&samples);
                pulse.write(&bytes(&samples)).map_err(on_write)?;
                keep_tail(tail, &samples);
            }
            AudioPacket::Raw(data) => {
                pulse.write(&data).map_err(on_write)?;
                tail.clear();
            }
        }
        *faded = false;
        stream.written = Some(Instant::now());
        stream.wrote();
        Ok(())
    }
}

fn open() -> SinkResult<Simple> {
    // The same names librespot's own PulseAudio output gives its stream.
    let name = std::env::var("PULSE_PROP_application.name").unwrap_or_default();
    let description = std::env::var("PULSE_PROP_stream.description").unwrap_or_default();
    let spec = sample::Spec {
        format: sample::Format::S16NE,
        channels: NUM_CHANNELS,
        rate: SAMPLE_RATE,
    };
    Simple::new(
        None,
        &name,
        Direction::Playback,
        None,
        &description,
        &spec,
        None,
        None,
    )
    .map_err(|error| SinkError::ConnectionRefused(format!("<PulseAudio> {error}")))
}

fn on_write(error: PAErr) -> SinkError {
    SinkError::OnWrite(format!("<PulseAudio> {error}"))
}

/// Samples per fade: frames times channels.
fn fade_len() -> usize {
    let frames = SAMPLE_RATE as u128 * FADE.as_millis() / 1000;
    frames as usize * usize::from(NUM_CHANNELS)
}

/// Keeps the last `fade_len` samples written.
fn keep_tail(tail: &mut Vec<i16>, samples: &[i16]) {
    let len = fade_len();
    tail.extend_from_slice(&samples[samples.len().saturating_sub(len)..]);
    let excess = tail.len().saturating_sub(len);
    tail.drain(..excess);
}

/// A fade from the last sound to silence: the tail played backwards, so the
/// sound goes on from where it stopped without a jump, getting quieter.
fn fade(tail: &[i16]) -> Vec<i16> {
    let channels = usize::from(NUM_CHANNELS);
    let frames = tail.len() / channels;
    let mut out = Vec::with_capacity(frames * channels);
    for (i, frame) in tail.chunks_exact(channels).rev().enumerate() {
        let done = (i + 1) as f64 / frames as f64;
        let gain = 0.5 * (1.0 + (std::f64::consts::PI * done).cos());
        out.extend(
            frame
                .iter()
                .map(|&sample| (f64::from(sample) * gain) as i16),
        );
    }
    out
}

fn bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_ne_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{NUM_CHANNELS, fade, fade_len, keep_tail};

    #[test]
    fn the_fade_starts_where_the_sound_stopped_and_ends_silent() {
        let channels = usize::from(NUM_CHANNELS);
        // A loud tone, different on each side.
        let tail: Vec<i16> = (0..fade_len() / channels)
            .flat_map(|i| {
                let left = (f64::from(i as u32) * 0.3).sin() * 20_000.0;
                [left as i16, (-left) as i16]
            })
            .collect();
        let out = fade(&tail);
        assert_eq!(out.len(), tail.len());
        let last = &tail[tail.len() - channels..];
        let first = &out[..channels];
        for (a, b) in last.iter().zip(first) {
            assert!((a - b).abs() < 50, "no jump: {a} then {b}");
        }
        assert!(out[out.len() - channels..].iter().all(|&s| s == 0));
    }

    #[test]
    fn the_tail_keeps_only_the_last_samples() {
        let mut tail = Vec::new();
        keep_tail(&mut tail, &[1, 2, 3, 4]);
        assert_eq!(tail, [1, 2, 3, 4]);
        let long: Vec<i16> = (0..fade_len() as i16 + 10).collect();
        keep_tail(&mut tail, &long);
        assert_eq!(tail.len(), fade_len());
        assert_eq!(tail.last(), long.last());
        keep_tail(&mut tail, &[-1, -2]);
        assert_eq!(tail.len(), fade_len());
        assert_eq!(&tail[tail.len() - 2..], [-1, -2]);
    }
}
