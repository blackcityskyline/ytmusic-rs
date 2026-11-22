//! The real sound card, through `cpal` (ALSA, PulseAudio/PipeWire's ALSA plugin, CoreAudio,
//! WASAPI). Everything above it is tested with a fake; this file is the part that needs
//! hardware, and is checked on a real machine (docs/testing-on-device.md).

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::native::{Device, Pull};

#[derive(Default)]
pub struct CpalDevice {
    stream: Option<Stream>,
}

fn default_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let device = cpal::default_host().default_output_device().context("no audio output device")?;
    let config = device.default_output_config().context("the audio device has no usable configuration")?;
    Ok((device, config))
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut pull: Pull) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                scratch.resize(out.len(), 0.0);
                pull.fill(&mut scratch);
                for (dst, src) in out.iter_mut().zip(&scratch) {
                    *dst = T::from_sample(*src);
                }
            },
            |error| eprintln!("ytmusicd: audio stream: {error}"),
            None,
        )
        .context("opening the audio stream")?;
    Ok(stream)
}

impl Device for CpalDevice {
    fn format(&mut self) -> Result<(u32, usize)> {
        let (_, config) = default_device()?;
        Ok((config.sample_rate(), config.channels() as usize))
    }

    fn start(&mut self, pull: Pull, playing: bool) -> Result<()> {
        let (device, supported) = default_device()?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, pull)?,
            SampleFormat::I16 => build::<i16>(&device, config, pull)?,
            SampleFormat::I32 => build::<i32>(&device, config, pull)?,
            SampleFormat::U16 => build::<u16>(&device, config, pull)?,
            other => return Err(anyhow!("unsupported sample format {other}")),
        };
        if playing {
            stream.play().context("starting the audio stream")?;
        } else {
            // Some backends start on creation; make the paused state certain.
            let _ = stream.pause();
        }
        self.stream = Some(stream);
        Ok(())
    }

    fn pause(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.pause();
        }
    }

    fn resume(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.play();
        }
    }

    fn stop(&mut self) {
        self.stream = None;
    }
}
