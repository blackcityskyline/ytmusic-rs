//! Turns a spool into interleaved `f32` samples. Blocking: it runs on the output's own
//! thread, never on the control runtime, so a slow download cannot delay a command.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::common::Tier;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;
use tokio::runtime::Handle;

use crate::spool::Spool;

/// A [`MediaSource`] over a spool. Reads wait for the download exactly as a client of the
/// loopback endpoint would, so the decoder never sees a short file that is still growing.
pub struct SpoolSource {
    spool: Arc<Spool>,
    runtime: Handle,
    at: u64,
    total: Option<u64>,
}

impl SpoolSource {
    /// Call from a thread that is not a runtime worker (the reads block on it).
    pub fn open(spool: Arc<Spool>, runtime: Handle) -> Result<Self> {
        let total = runtime.block_on(spool.total()).map_err(|e| anyhow!(e)).context("opening the track")?;
        Ok(Self { spool, runtime, at: 0, total: Some(total) })
    }
}

impl Read for SpoolSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.runtime.block_on(self.spool.read_at(self.at, buf)).map_err(std::io::Error::other)?;
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for SpoolSource {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let total = self.total.unwrap_or(0) as i128;
        let target = match to {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(d) => self.at as i128 + d as i128,
            SeekFrom::End(d) => total + d as i128,
        };
        if target < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.at = target as u64;
        Ok(self.at)
    }
}

impl MediaSource for SpoolSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.total
    }
}

fn registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    // YouTube's usual audio is Opus in WebM; symphonia has no Opus decoder of its own yet.
    registry.register_audio_decoder_at_tier::<symphonia_opus::OpusDecoder>(Tier::Fallback);
    registry
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub rate: u32,
    pub channels: usize,
    samples: Vec<f32>,
}

impl Decoder {
    pub fn open(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<Self> {
        let mut hint = Hint::new();
        if let Some(extension) = extension {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(source, Default::default());
        let format = symphonia::default::get_probe()
            .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
            .context("unrecognised audio format")?;
        let track = format.default_track(TrackType::Audio).context("no audio track")?;
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).context("no audio parameters")?;
        let decoder = registry().make_audio_decoder(params, &AudioDecoderOptions::default()).context("no decoder for this codec")?;
        let track_id = track.id;
        let rate = params.sample_rate.context("unknown sample rate")?;
        let channels = params.channels.as_ref().map(|c| c.count()).context("unknown channel layout")?;
        Ok(Self { format, decoder, track_id, rate, channels, samples: Vec::new() })
    }

    /// The next block of interleaved samples, or `None` at the end of the track. A packet that
    /// fails to decode is skipped: one bad frame is a glitch, not the end of the song.
    pub fn next_block(&mut self) -> Result<Option<&[f32]>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buffer) => {
                    self.samples.resize(buffer.samples_interleaved(), 0.0);
                    buffer.copy_to_slice_interleaved(&mut self.samples);
                    return Ok(Some(&self.samples));
                }
                Err(Error::DecodeError(_)) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub fn seek(&mut self, ms: u64) -> Result<()> {
        let time = Time::try_from_secs_f64(ms as f64 / 1000.0).context("seek time out of range")?;
        self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) }).context("seek failed")?;
        self.decoder.reset();
        Ok(())
    }
}
