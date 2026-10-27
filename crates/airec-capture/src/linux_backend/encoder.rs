use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use airec_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AirecError, CaptureFrame, EncoderFactory, ErrorCode,
    PipelineEncoder, RecordingOptions, ResolvedTarget,
};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

use super::mp4::{AudioSample, FragmentedMp4Writer, OpusTrack, Sample, TIMESCALE};
use crate::yuv::{even, i420_bt709};
use crate::{EncoderDiagnostics, output_error};

const NAL_TYPE_IDR_SLICE: u8 = 5;
const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;
const OPUS_BITRATE: i32 = 128_000;
/// 20 ms, the frame length Opus is tuned for.
const OPUS_FRAME_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize / 50;
/// The largest packet libopus produces for one frame.
const OPUS_MAX_PACKET: usize = 1_275;

#[derive(Default)]
pub struct OpenH264EncoderFactory;

/// OpenH264 is compiled into the executable and has no hardware path.
#[must_use]
pub fn diagnose_encoders() -> EncoderDiagnostics {
    let software = probe_encoder();
    EncoderDiagnostics {
        hardware_available: false,
        software_available: software.is_ok(),
        selected: software.is_ok().then_some("software"),
        hardware_error: Some(
            "Linux builds encode with the bundled OpenH264 software encoder".into(),
        ),
        software_error: software.err(),
    }
}

fn probe_encoder() -> Result<(), String> {
    let (width, height) = (1280, 720);
    let mut encoder = create_encoder(30, 4_000_000).map_err(|error| error.to_string())?;
    let luma = vec![16_u8; width * height];
    let chroma = vec![128_u8; width * height / 4];
    let source = YUVSlices::new(
        (&luma, &chroma, &chroma),
        (width, height),
        (width, width / 2, width / 2),
    );
    let bitstream = encoder.encode(&source).map_err(|error| error.to_string())?;
    let access_unit = AccessUnit::from_annex_b(&bitstream.to_vec());
    if access_unit.sps.is_some() && access_unit.pps.is_some() && access_unit.is_idr {
        Ok(())
    } else {
        Err("OpenH264 did not produce an IDR frame with parameter sets".into())
    }
}

fn create_encoder(fps: u32, bitrate: u32) -> Result<Encoder, openh264::Error> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        // Screen content mode does not support either one and prints a warning when they are on.
        .adaptive_quantization(false)
        .background_detection(false)
        .vui(VuiConfig::bt709());
    Encoder::with_api_config(OpenH264API::from_source(), config)
}

impl EncoderFactory for OpenH264EncoderFactory {
    fn create(
        &self,
        target: &ResolvedTarget,
        options: &RecordingOptions,
    ) -> Result<Box<dyn PipelineEncoder>, AirecError> {
        if let Some(parent) = target.output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| output_error(&target.output, error))?;
        }
        let width = even(target.width);
        let height = even(target.height);
        let encoder = create_encoder(
            options.fps,
            options.quality.bitrate(width, height, options.fps),
        )
        .map_err(|error| {
            AirecError::new(
                ErrorCode::EncoderUnavailable,
                "OpenH264 encoder failed to start",
                serde_json::json!({"software": error.to_string()}),
            )
        })?;
        let audio = options
            .audio
            .then(OpusAudio::new)
            .transpose()
            .map_err(|error| {
                AirecError::new(
                    ErrorCode::EncoderUnavailable,
                    "Opus encoder failed to start",
                    serde_json::json!({"audio": error.to_string()}),
                )
            })?;
        let file =
            File::create(&target.output).map_err(|error| output_error(&target.output, error))?;
        Ok(Box::new(OpenH264Mp4Encoder {
            encoder,
            path: target.output.clone(),
            file: Some(file),
            writer: None,
            width,
            height,
            fps: options.fps,
            last_decode_time: None,
            audio,
        }))
    }
}

struct OpenH264Mp4Encoder {
    encoder: Encoder,
    path: PathBuf,
    /// Held until the first IDR frame supplies the parameter sets the MP4 header needs.
    file: Option<File>,
    writer: Option<FragmentedMp4Writer<BufWriter<File>>>,
    width: u32,
    height: u32,
    fps: u32,
    last_decode_time: Option<u64>,
    audio: Option<OpusAudio>,
}

struct OpusAudio {
    encoder: opus::Encoder,
    pre_skip: u16,
    /// Samples that do not fill a whole Opus frame yet.
    pcm: Vec<i16>,
    /// Packets encoded before the MP4 writer exists.
    held: Vec<AudioSample>,
}

impl OpusAudio {
    fn new() -> Result<Self, opus::Error> {
        let mut encoder = opus::Encoder::new(
            AUDIO_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        encoder.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))?;
        let pre_skip = u16::try_from(encoder.get_lookahead()?).unwrap_or(0);
        Ok(Self {
            encoder,
            pre_skip,
            pcm: Vec::new(),
            held: Vec::new(),
        })
    }

    /// Encodes every whole frame in the buffered samples.
    fn encode_frames(&mut self) -> Result<Vec<AudioSample>, opus::Error> {
        let frame_len = OPUS_FRAME_SAMPLES * AUDIO_CHANNELS;
        let mut samples = Vec::new();
        let mut packet = [0_u8; OPUS_MAX_PACKET];
        let whole_frames_len = self.pcm.len() / frame_len * frame_len;
        for frame in self.pcm[..whole_frames_len].chunks_exact(frame_len) {
            let len = self.encoder.encode(frame, &mut packet)?;
            samples.push(AudioSample {
                data: packet[..len].to_vec(),
                duration: OPUS_FRAME_SAMPLES as u32,
            });
        }
        self.pcm.drain(..whole_frames_len);
        Ok(samples)
    }

    /// Pads the last partial frame with silence so no captured sample is lost.
    fn encode_remainder(&mut self) -> Result<Vec<AudioSample>, opus::Error> {
        let frame_len = OPUS_FRAME_SAMPLES * AUDIO_CHANNELS;
        if !self.pcm.is_empty() {
            self.pcm
                .resize(self.pcm.len().div_ceil(frame_len) * frame_len, 0);
        }
        self.encode_frames()
    }
}

impl OpenH264Mp4Encoder {
    fn push_audio(&mut self, samples: Vec<AudioSample>) {
        match (&mut self.writer, &mut self.audio) {
            (Some(writer), _) => samples
                .into_iter()
                .for_each(|sample| writer.push_audio(sample)),
            (None, Some(audio)) => audio.held.extend(samples),
            (None, None) => {}
        }
    }
}

impl PipelineEncoder for OpenH264Mp4Encoder {
    fn write_frame(&mut self, frame: &CaptureFrame) -> Result<(), AirecError> {
        let planes = i420_bt709(frame, self.width, self.height);
        let width = self.width as usize;
        let height = self.height as usize;
        let source = YUVSlices::new(
            (&planes.y, &planes.u, &planes.v),
            (width, height),
            (width, width / 2, width / 2),
        );
        let bitstream = self
            .encoder
            .encode(&source)
            .map_err(|error| encoder_error(&self.path, error))?;
        if bitstream.frame_type() == FrameType::Skip {
            return Ok(());
        }
        let access_unit = AccessUnit::from_annex_b(&bitstream.to_vec());
        if access_unit.slices.is_empty() {
            return Ok(());
        }

        if self.writer.is_none() {
            let (Some(sps), Some(pps)) = (&access_unit.sps, &access_unit.pps) else {
                return Err(encoder_error(
                    &self.path,
                    "the first encoded frame has no H.264 parameter sets",
                ));
            };
            let file = self
                .file
                .take()
                .expect("file is held until the writer exists");
            let audio_track = self.audio.as_ref().map(|audio| OpusTrack {
                pre_skip: audio.pre_skip,
            });
            self.writer = Some(
                FragmentedMp4Writer::new(
                    BufWriter::new(file),
                    self.width,
                    self.height,
                    sps,
                    pps,
                    audio_track,
                )
                .map_err(|error| output_error(&self.path, error))?,
            );
            if let Some(audio) = &mut self.audio {
                let held = std::mem::take(&mut audio.held);
                self.push_audio(held);
            }
        }
        let decode_time = frame_grid_time(frame.t_ms, self.fps, self.last_decode_time);
        self.last_decode_time = Some(decode_time);
        self.writer
            .as_mut()
            .expect("writer was created above")
            .push(Sample {
                data: access_unit.slices,
                decode_time,
                sync: access_unit.is_idr,
            })
            .map_err(|error| output_error(&self.path, error))
    }

    fn write_audio(&mut self, samples: &[i16]) -> Result<(), AirecError> {
        let Some(audio) = &mut self.audio else {
            return Ok(());
        };
        audio.pcm.extend_from_slice(samples);
        let encoded = audio
            .encode_frames()
            .map_err(|error| encoder_error(&self.path, error))?;
        self.push_audio(encoded);
        Ok(())
    }

    fn finish(&mut self) -> Result<(), AirecError> {
        if let Some(audio) = &mut self.audio {
            let encoded = audio
                .encode_remainder()
                .map_err(|error| encoder_error(&self.path, error))?;
            self.push_audio(encoded);
        }
        let Some(writer) = self.writer.take() else {
            return Ok(());
        };
        let frame_ticks = TIMESCALE / self.fps.max(1);
        let output = writer
            .finish(frame_ticks)
            .map_err(|error| output_error(&self.path, error))?;
        let file = output
            .into_inner()
            .map_err(|error| output_error(&self.path, error.into_error()))?;
        file.sync_all()
            .map_err(|error| output_error(&self.path, error))
    }
}

/// Capture jitter puts some frames closer together than one frame interval. Tools that rescale
/// to a 1/fps time base, such as ffmpeg 4.4 on Ubuntu 22.04, then see repeated timestamps, so
/// each frame goes on the nearest free slot of the frame grid.
fn frame_grid_time(t_ms: u64, fps: u32, last_decode_time: Option<u64>) -> u64 {
    let fps = u64::from(fps.max(1));
    let frame_ticks = u64::from(TIMESCALE) / fps;
    let nearest_slot = (t_ms.saturating_mul(fps) + 500) / 1_000;
    let requested = nearest_slot * frame_ticks;
    last_decode_time.map_or(requested, |last| requested.max(last + frame_ticks))
}

/// One encoded picture split into parameter sets and length-prefixed slice NAL units.
#[derive(Debug, Default)]
struct AccessUnit {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    slices: Vec<u8>,
    is_idr: bool,
}

impl AccessUnit {
    fn from_annex_b(stream: &[u8]) -> Self {
        let mut access_unit = Self::default();
        for nal in annex_b_nal_units(stream) {
            match nal[0] & 0x1f {
                NAL_TYPE_SPS => access_unit.sps = Some(nal.to_vec()),
                NAL_TYPE_PPS => access_unit.pps = Some(nal.to_vec()),
                nal_type => {
                    access_unit.is_idr |= nal_type == NAL_TYPE_IDR_SLICE;
                    let len = u32::try_from(nal.len()).unwrap_or(u32::MAX);
                    access_unit.slices.extend_from_slice(&len.to_be_bytes());
                    access_unit.slices.extend_from_slice(nal);
                }
            }
        }
        access_unit
    }
}

/// NAL units between 3- or 4-byte start codes, without the start codes.
fn annex_b_nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index..index + 3] == [0, 0, 1] {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    let ends = starts
        .iter()
        .skip(1)
        .map(|&next| {
            let start_code = next - 3;
            if start_code > 0 && stream[start_code - 1] == 0 {
                start_code - 1
            } else {
                start_code
            }
        })
        .chain(std::iter::once(stream.len()))
        .collect::<Vec<_>>();
    starts
        .into_iter()
        .zip(ends)
        .map(move |(start, end)| &stream[start..end])
        .filter(|nal| !nal.is_empty())
}

fn encoder_error(path: &Path, error: impl std::fmt::Display) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"component": "encoder", "path": path}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_stream_splits_on_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];

        let nals: Vec<_> = annex_b_nal_units(&stream).collect();

        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn access_unit_keeps_parameter_sets_out_of_the_sample() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3,
        ];

        let access_unit = AccessUnit::from_annex_b(&stream);

        assert_eq!(access_unit.sps, Some(vec![0x67, 1]));
        assert_eq!(access_unit.pps, Some(vec![0x68, 2]));
        assert!(access_unit.is_idr);
        assert_eq!(access_unit.slices, [0, 0, 0, 2, 0x65, 3]);
    }

    #[test]
    fn jittered_frames_land_on_distinct_frame_slots() {
        let first = frame_grid_time(0, 30, None);
        let early = frame_grid_time(20, 30, Some(first));
        let crowded = frame_grid_time(40, 30, Some(early));
        let late = frame_grid_time(200, 30, Some(crowded));

        assert_eq!([first, early, crowded, late], [0, 3_000, 6_000, 18_000]);
    }

    #[test]
    fn audio_is_encoded_in_whole_frames_and_the_remainder_is_padded() {
        let mut audio = OpusAudio::new().unwrap();
        audio
            .pcm
            .extend(vec![0; OPUS_FRAME_SAMPLES * AUDIO_CHANNELS * 3 / 2]);

        let whole = audio.encode_frames().unwrap();
        let remainder = audio.encode_remainder().unwrap();

        assert_eq!(whole.len(), 1);
        assert_eq!(remainder.len(), 1);
        assert!(audio.pcm.is_empty());
        assert_eq!(remainder[0].duration, 960);
    }

    #[test]
    fn bundled_encoder_produces_an_idr_frame_with_parameter_sets() {
        assert_eq!(probe_encoder(), Ok(()));
    }
}
