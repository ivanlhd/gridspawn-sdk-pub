//! Audio devices through cpal (feature `cpal`), for programs without their own
//! audio engine. Godot and other engines use the PCM API directly instead.

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream, StreamConfig,
};

use crate::{error::VoiceError, session::VoiceSession};

fn device_error(error: impl std::fmt::Display) -> VoiceError {
    VoiceError::Audio(error.to_string())
}

/// Names of the input (`input = true`) or output devices.
pub fn device_names(input: bool) -> Vec<String> {
    let host = cpal::default_host();
    let devices = if input {
        host.input_devices()
    } else {
        host.output_devices()
    };
    devices
        .map(|list| {
            list.filter_map(|d| d.description().ok().map(|desc| desc.name().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Running microphone and speaker streams; dropping it stops them.
pub struct DeviceIo {
    _input: Stream,
    _output: Stream,
}

fn find(input: bool, name: Option<&str>) -> Result<cpal::Device, VoiceError> {
    let host = cpal::default_host();
    let default = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    let Some(name) = name else {
        return default.ok_or(VoiceError::Audio("no default audio device".into()));
    };
    let mut devices = if input {
        host.input_devices().map_err(device_error)?
    } else {
        host.output_devices().map_err(device_error)?
    };
    devices
        .find(|d| d.description().is_ok_and(|desc| desc.name() == name))
        .ok_or(VoiceError::Audio(format!("no audio device named {name}")))
}

/// A 48 kHz f32 configuration of the device, or an error.
fn config_48k(device: &cpal::Device, input: bool) -> Result<StreamConfig, VoiceError> {
    let configs: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(device_error)?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(device_error)?
            .collect()
    };
    configs
        .into_iter()
        .filter(|c| c.sample_format() == SampleFormat::F32)
        .find_map(|c| c.try_with_sample_rate(48_000))
        .map(|c| c.config())
        .ok_or(VoiceError::Audio(
            "the device has no 48 kHz f32 mode".into(),
        ))
}

impl DeviceIo {
    /// Starts capture and playback on the named (or default) devices.
    pub fn start(
        voice: &VoiceSession,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<Self, VoiceError> {
        let mic = find(true, input)?;
        let speakers = find(false, output)?;
        let in_config = config_48k(&mic, true)?;
        let out_config = config_48k(&speakers, false)?;
        let in_channels = usize::from(in_config.channels);
        let out_channels = usize::from(out_config.channels);

        let capture = voice.clone();
        let mut mono = Vec::new();
        let input = mic
            .build_input_stream(
                in_config,
                move |data: &[f32], _| {
                    mono.clear();
                    mono.extend(
                        data.chunks(in_channels)
                            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32),
                    );
                    capture.push_capture(&mono);
                },
                |error| tracing::warn!(%error, "microphone stream error"),
                None,
            )
            .map_err(device_error)?;
        let playback = voice.clone();
        let mut mix = Vec::new();
        let output = speakers
            .build_output_stream(
                out_config,
                move |data: &mut [f32], _| {
                    mix.resize(data.len() / out_channels, 0.0);
                    playback.pull_mix(&mut mix);
                    playback.push_playback_reference(&mix);
                    for (frame, sample) in data.chunks_mut(out_channels).zip(&mix) {
                        frame.fill(*sample);
                    }
                },
                |error| tracing::warn!(%error, "speaker stream error"),
                None,
            )
            .map_err(device_error)?;
        input.play().map_err(device_error)?;
        output.play().map_err(device_error)?;
        Ok(Self {
            _input: input,
            _output: output,
        })
    }
}
