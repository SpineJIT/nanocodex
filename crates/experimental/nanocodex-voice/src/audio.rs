use std::{
    collections::VecDeque,
    fmt::Display,
    sync::{Arc, Mutex},
    time::Duration,
};

use cpal::{
    Device, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nanocodex::oai::realtime::{REALTIME_SAMPLE_RATE, RealtimeAudio};
use tokio::sync::mpsc;

use crate::{AudioConfig, AudioError};

const INPUT_FRAME_SAMPLES: usize = 480;

pub(crate) struct VoiceAudio {
    _input: Stream,
    _output: Stream,
    playback: Playback,
}

impl VoiceAudio {
    pub(crate) fn open(
        policy: AudioConfig,
    ) -> Result<(Self, mpsc::Receiver<RealtimeAudio>), AudioError> {
        if policy.maximum_playback_buffer().is_zero() {
            return Err(AudioError::InvalidConfig(
                "maximum playback buffer must be greater than zero",
            ));
        }
        if policy.playback_prebuffer() > policy.maximum_playback_buffer() {
            return Err(AudioError::InvalidConfig(
                "playback prebuffer cannot exceed maximum playback buffer",
            ));
        }

        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?;
        let output_device = host
            .default_output_device()
            .ok_or(AudioError::NoOutputDevice)?;

        let input_supported = input_device
            .default_input_config()
            .map_err(|error| backend("failed to read the default microphone format", error))?;
        let output_supported = output_device
            .default_output_config()
            .map_err(|error| backend("failed to read the default speaker format", error))?;
        let input_config: StreamConfig = input_supported.clone().into();
        let output_config: StreamConfig = output_supported.clone().into();

        let (microphone_tx, microphone_rx) = mpsc::channel(256);
        let input = build_input(
            &input_device,
            input_supported.sample_format(),
            input_config,
            microphone_tx,
        )?;
        let playback = Playback::new(output_config.sample_rate.0, output_config.channels, policy)?;
        let output = build_output(
            &output_device,
            output_supported.sample_format(),
            output_config,
            Arc::clone(&playback.buffer),
        )?;

        input
            .play()
            .map_err(|error| backend("failed to start the microphone", error))?;
        output
            .play()
            .map_err(|error| backend("failed to start audio output", error))?;
        Ok((
            Self {
                _input: input,
                _output: output,
                playback,
            },
            microphone_rx,
        ))
    }

    pub(crate) fn play(&mut self, audio: &RealtimeAudio) {
        self.playback.push(audio);
    }

    pub(crate) fn interrupt(&mut self) {
        self.playback.clear();
    }
}

struct Playback {
    buffer: Arc<Mutex<PlaybackBuffer>>,
    resampler: LinearResampler,
    resampled: Vec<f32>,
    channels: usize,
    maximum_samples: usize,
}

struct PlaybackBuffer {
    samples: VecDeque<f32>,
    prebuffer_samples: usize,
    buffering: bool,
}

impl Playback {
    fn new(sample_rate: u32, channels: u16, policy: AudioConfig) -> Result<Self, AudioError> {
        let channels = usize::from(channels);
        if channels == 0 {
            return Err(AudioError::InvalidConfig(
                "speaker channel count must be greater than zero",
            ));
        }
        let prebuffer_samples =
            duration_samples(sample_rate, channels, policy.playback_prebuffer());
        let maximum_samples =
            duration_samples(sample_rate, channels, policy.maximum_playback_buffer());
        if maximum_samples == 0 {
            return Err(AudioError::InvalidConfig(
                "maximum playback buffer is shorter than one device sample",
            ));
        }
        Ok(Self {
            buffer: Arc::new(Mutex::new(PlaybackBuffer {
                samples: VecDeque::with_capacity(prebuffer_samples),
                prebuffer_samples,
                buffering: true,
            })),
            resampler: LinearResampler::new(REALTIME_SAMPLE_RATE, sample_rate),
            resampled: Vec::new(),
            channels,
            maximum_samples,
        })
    }

    fn push(&mut self, audio: &RealtimeAudio) {
        let source = audio.as_bytes().chunks_exact(2).map(|sample| {
            let sample = i16::from_le_bytes([sample[0], sample[1]]);
            f32::from(sample) / f32::from(i16::MAX)
        });
        self.resampler.push_into(source, &mut self.resampled);
        let Ok(mut buffer) = self.buffer.lock() else {
            return;
        };
        let retained_frames = self
            .resampled
            .len()
            .min(self.maximum_samples / self.channels);
        let appended = retained_frames.saturating_mul(self.channels);
        let overflow = buffer
            .samples
            .len()
            .saturating_add(appended)
            .saturating_sub(self.maximum_samples);
        let discarded = overflow.min(buffer.samples.len());
        buffer.samples.drain(..discarded);
        for sample in self
            .resampled
            .iter()
            .skip(self.resampled.len().saturating_sub(retained_frames))
            .copied()
        {
            buffer
                .samples
                .extend(std::iter::repeat_n(sample, self.channels));
        }
    }

    fn clear(&mut self) {
        self.resampler.clear();
        self.resampled.clear();
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.samples.clear();
            buffer.buffering = true;
        }
    }
}

struct Microphone {
    resampler: LinearResampler,
    channels: usize,
    interleaved_tail: Vec<f32>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    pending: VecDeque<f32>,
    sender: mpsc::Sender<RealtimeAudio>,
}

impl Microphone {
    fn new(sample_rate: u32, channels: u16, sender: mpsc::Sender<RealtimeAudio>) -> Self {
        Self {
            resampler: LinearResampler::new(sample_rate, REALTIME_SAMPLE_RATE),
            channels: usize::from(channels),
            interleaved_tail: Vec::new(),
            mono: Vec::new(),
            resampled: Vec::new(),
            pending: VecDeque::with_capacity(INPUT_FRAME_SAMPLES * 2),
            sender,
        }
    }

    fn push(&mut self, input: impl IntoIterator<Item = f32>) {
        self.interleaved_tail.extend(input);
        let complete = self.interleaved_tail.len() / self.channels * self.channels;
        self.mono.clear();
        self.mono.extend(
            self.interleaved_tail[..complete]
                .chunks_exact(self.channels)
                .map(|frame| frame.iter().copied().sum::<f32>() / self.channels as f32),
        );
        self.interleaved_tail.drain(..complete);
        self.resampler
            .push_into(self.mono.iter().copied(), &mut self.resampled);
        self.pending.extend(self.resampled.iter().copied());

        while self.pending.len() >= INPUT_FRAME_SAMPLES {
            let audio = RealtimeAudio::from_samples(
                self.pending.drain(..INPUT_FRAME_SAMPLES).map(f32_to_i16),
            );
            if self.sender.try_send(audio).is_err() {
                break;
            }
        }
    }
}

struct LinearResampler {
    step: f64,
    position: f64,
    source: Vec<f32>,
}

impl LinearResampler {
    fn new(source_rate: u32, destination_rate: u32) -> Self {
        Self {
            step: f64::from(source_rate) / f64::from(destination_rate),
            position: 0.0,
            source: Vec::new(),
        }
    }

    fn push_into(&mut self, input: impl IntoIterator<Item = f32>, output: &mut Vec<f32>) {
        self.source.extend(input);
        output.clear();
        while self.position + 1.0 < self.source.len() as f64 {
            let index = self.position.floor() as usize;
            let fraction = (self.position - index as f64) as f32;
            output.push(
                self.source[index] + (self.source[index + 1] - self.source[index]) * fraction,
            );
            self.position += self.step;
        }
        let consumed = self.position.floor() as usize;
        if consumed > 0 {
            self.source.drain(..consumed.min(self.source.len()));
            self.position -= consumed as f64;
        }
    }

    fn clear(&mut self) {
        self.position = 0.0;
        self.source.clear();
    }
}

fn build_input(
    device: &Device,
    format: SampleFormat,
    config: StreamConfig,
    sender: mpsc::Sender<RealtimeAudio>,
) -> Result<Stream, AudioError> {
    let microphone = Arc::new(Mutex::new(Microphone::new(
        config.sample_rate.0,
        config.channels,
        sender,
    )));
    let error = |error| tracing::error!(%error, "microphone stream failed");
    let stream = match format {
        SampleFormat::F32 => {
            let microphone = Arc::clone(&microphone);
            device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _| push_input(&microphone, data.iter().copied()),
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the microphone stream", error))?
        }
        SampleFormat::I16 => {
            let microphone = Arc::clone(&microphone);
            device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        push_input(
                            &microphone,
                            data.iter()
                                .map(|sample| f32::from(*sample) / f32::from(i16::MAX)),
                        );
                    },
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the microphone stream", error))?
        }
        SampleFormat::U16 => {
            let microphone = Arc::clone(&microphone);
            device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        push_input(
                            &microphone,
                            data.iter()
                                .map(|sample| f32::from(*sample) / 32_767.5 - 1.0),
                        );
                    },
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the microphone stream", error))?
        }
        unsupported => {
            return Err(AudioError::Backend {
                operation: "unsupported microphone sample format",
                message: unsupported.to_string(),
            });
        }
    };
    Ok(stream)
}

fn push_input(microphone: &Mutex<Microphone>, samples: impl IntoIterator<Item = f32>) {
    if let Ok(mut microphone) = microphone.lock() {
        microphone.push(samples);
    }
}

fn build_output(
    device: &Device,
    format: SampleFormat,
    config: StreamConfig,
    samples: Arc<Mutex<PlaybackBuffer>>,
) -> Result<Stream, AudioError> {
    let error = |error| tracing::error!(%error, "speaker stream failed");
    let stream = match format {
        SampleFormat::F32 => {
            let samples = Arc::clone(&samples);
            device
                .build_output_stream(
                    &config,
                    move |output: &mut [f32], _| fill_output(output, &samples, |value| value),
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the speaker stream", error))?
        }
        SampleFormat::I16 => {
            let samples = Arc::clone(&samples);
            device
                .build_output_stream(
                    &config,
                    move |output: &mut [i16], _| fill_output(output, &samples, f32_to_i16),
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the speaker stream", error))?
        }
        SampleFormat::U16 => {
            let samples = Arc::clone(&samples);
            device
                .build_output_stream(
                    &config,
                    move |output: &mut [u16], _| {
                        fill_output(output, &samples, |value| {
                            ((value.clamp(-1.0, 1.0) + 1.0) * 32_767.5).round() as u16
                        });
                    },
                    error,
                    None,
                )
                .map_err(|error| backend("failed to build the speaker stream", error))?
        }
        unsupported => {
            return Err(AudioError::Backend {
                operation: "unsupported speaker sample format",
                message: unsupported.to_string(),
            });
        }
    };
    Ok(stream)
}

fn fill_output<T>(output: &mut [T], buffer: &Mutex<PlaybackBuffer>, convert: impl Fn(f32) -> T) {
    let Ok(mut buffer) = buffer.try_lock() else {
        for sample in output {
            *sample = convert(0.0);
        }
        return;
    };
    if buffer.buffering {
        if buffer.samples.len() < buffer.prebuffer_samples {
            for sample in output {
                *sample = convert(0.0);
            }
            return;
        }
        buffer.buffering = false;
    }
    for sample in output {
        let Some(value) = buffer.samples.pop_front() else {
            buffer.buffering = true;
            *sample = convert(0.0);
            continue;
        };
        *sample = convert(value);
    }
}

fn duration_samples(sample_rate: u32, channels: usize, duration: Duration) -> usize {
    duration
        .as_nanos()
        .saturating_mul(u128::from(sample_rate))
        .saturating_mul(channels as u128)
        .checked_div(1_000_000_000)
        .and_then(|samples| usize::try_from(samples).ok())
        .unwrap_or(usize::MAX)
}

fn backend(operation: &'static str, error: impl Display) -> AudioError {
    AudioError::Backend {
        operation,
        message: error.to_string(),
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use super::{LinearResampler, PlaybackBuffer, duration_samples, fill_output};

    #[test]
    fn resamples_across_chunk_boundaries_without_reallocating_the_destination() {
        let mut downsample = LinearResampler::new(48_000, 24_000);
        let mut output = Vec::with_capacity(4);
        downsample.push_into([0.0, 0.25], &mut output);
        assert_eq!(output, vec![0.0]);
        let capacity = output.capacity();
        downsample.push_into([0.5, 0.75, 1.0], &mut output);
        assert_eq!(output, vec![0.5]);
        assert_eq!(output.capacity(), capacity);

        let mut upsample = LinearResampler::new(24_000, 48_000);
        upsample.push_into([0.0], &mut output);
        assert!(output.is_empty());
        upsample.push_into([1.0], &mut output);
        assert_eq!(output, vec![0.0, 0.5]);
    }

    #[test]
    fn playback_waits_for_prebuffer_before_starting() {
        let buffer = Mutex::new(PlaybackBuffer {
            samples: VecDeque::from([0.25, 0.5]),
            prebuffer_samples: 3,
            buffering: true,
        });
        let mut output = [1.0; 2];

        fill_output(&mut output, &buffer, |sample| sample);

        assert_eq!(output, [0.0, 0.0]);
        assert_eq!(buffer.lock().unwrap().samples.len(), 2);
    }

    #[test]
    fn playback_rebuffers_after_an_underrun() {
        let buffer = Mutex::new(PlaybackBuffer {
            samples: VecDeque::from([0.25, 0.5, 0.75]),
            prebuffer_samples: 3,
            buffering: true,
        });
        let mut output = [1.0; 4];

        fill_output(&mut output, &buffer, |sample| sample);

        assert_eq!(output, [0.25, 0.5, 0.75, 0.0]);
        assert!(buffer.lock().unwrap().buffering);
    }

    #[test]
    fn duration_policy_maps_to_interleaved_device_samples() {
        assert_eq!(
            duration_samples(48_000, 2, Duration::from_millis(120)),
            11_520
        );
    }
}
