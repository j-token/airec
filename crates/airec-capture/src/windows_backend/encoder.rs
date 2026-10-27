use std::path::PathBuf;
use std::sync::OnceLock;

use airec_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AirecError, CaptureFrame, EncoderFactory, ErrorCode,
    PipelineEncoder, RecordingOptions, ResolvedTarget,
};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, ContainerSettingsSubType, VideoEncoder,
    VideoInputFormat, VideoSettingsBuilder, VideoSettingsSubType,
};

use crate::yuv::{even, i420_bt709};
use crate::{EncoderDiagnostics, output_error};

/// One of the rates the Media Foundation AAC encoder accepts: 96, 128, 160, or 192 kbps.
const AAC_BITRATE: u32 = 128_000;

#[derive(Default)]
pub struct WindowsEncoderFactory;

#[derive(Clone)]
enum PreferredEncoder {
    Hardware,
    Software {
        hardware_error: String,
    },
    Unavailable {
        hardware_error: String,
        software_error: String,
    },
}

static PREFERRED_ENCODER: OnceLock<PreferredEncoder> = OnceLock::new();

fn preferred_encoder() -> PreferredEncoder {
    PREFERRED_ENCODER
        .get_or_init(|| match probe_encoder_mode(true) {
            Ok(()) => PreferredEncoder::Hardware,
            Err(hardware_error) => match probe_encoder_mode(false) {
                Ok(()) => PreferredEncoder::Software {
                    hardware_error: hardware_error.to_string(),
                },
                Err(software_error) => PreferredEncoder::Unavailable {
                    hardware_error: hardware_error.to_string(),
                    software_error: software_error.to_string(),
                },
            },
        })
        .clone()
}

#[must_use]
pub fn diagnose_encoders() -> EncoderDiagnostics {
    let hardware = probe_encoder_mode(true);
    let software = probe_encoder_mode(false);
    EncoderDiagnostics {
        hardware_available: hardware.is_ok(),
        software_available: software.is_ok(),
        selected: if hardware.is_ok() {
            Some("hardware")
        } else if software.is_ok() {
            Some("software")
        } else {
            None
        },
        hardware_error: hardware.err().map(|error| error.to_string()),
        software_error: software.err().map(|error| error.to_string()),
    }
}

fn probe_encoder_mode(
    hardware_acceleration: bool,
) -> Result<(), windows_capture::encoder::VideoEncoderError> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mode = if hardware_acceleration { "hw" } else { "sw" };
    let config = EncoderConfig {
        path: std::env::temp_dir().join(format!(
            "airec-doctor-{}-{nonce}-{mode}.mp4",
            std::process::id()
        )),
        width: 1280,
        height: 720,
        fps: 30,
        bitrate: 4_000_000,
        audio: false,
    };
    let result = (|| {
        let mut encoder = create_encoder(&config, hardware_acceleration)?;
        let frame = vec![0_u8; config.width as usize * config.height as usize * 3 / 2];
        encoder.send_frame_buffer_at_timeline(&frame, 0)?;
        encoder.wait_until_ready(std::time::Duration::from_secs(2))?;
        encoder.finish()
    })();
    let _ = std::fs::remove_file(&config.path);
    result
}

impl EncoderFactory for WindowsEncoderFactory {
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
        let config = EncoderConfig {
            path: target.output.clone(),
            width,
            height,
            fps: options.fps,
            bitrate: options.quality.bitrate(width, height, options.fps),
            audio: options.audio,
        };
        match preferred_encoder() {
            PreferredEncoder::Hardware => match create_encoder(&config, true) {
                Ok(encoder) => Ok(Box::new(MfEncoder {
                    encoder: Some(encoder),
                    config,
                    hardware: true,
                    frames_written: 0,
                })),
                Err(hardware_error) => {
                    eprintln!(
                        "warning: hardware H.264 encoder unavailable ({hardware_error}); falling back to software"
                    );
                    probe_encoder_mode(false).map_err(|software_error| {
                        encoder_unavailable(&hardware_error, &software_error)
                    })?;
                    let encoder = create_encoder(&config, false).map_err(|software_error| {
                        encoder_unavailable(&hardware_error, &software_error)
                    })?;
                    Ok(Box::new(MfEncoder {
                        encoder: Some(encoder),
                        config,
                        hardware: false,
                        frames_written: 0,
                    }))
                }
            },
            PreferredEncoder::Software { hardware_error } => {
                eprintln!(
                    "warning: hardware H.264 encoder unavailable ({hardware_error}); falling back to software"
                );
                let encoder = create_encoder(&config, false).map_err(|software_error| {
                    AirecError::new(
                        ErrorCode::EncoderUnavailable,
                        "hardware and software H.264 encoders are unavailable",
                        serde_json::json!({
                            "hardware": hardware_error,
                            "software": software_error.to_string(),
                        }),
                    )
                })?;
                Ok(Box::new(MfEncoder {
                    encoder: Some(encoder),
                    config,
                    hardware: false,
                    frames_written: 0,
                }))
            }
            PreferredEncoder::Unavailable {
                hardware_error,
                software_error,
            } => Err(AirecError::new(
                ErrorCode::EncoderUnavailable,
                "hardware and software H.264 encoders are unavailable",
                serde_json::json!({
                    "hardware": hardware_error,
                    "software": software_error,
                }),
            )),
        }
    }
}

#[derive(Clone)]
struct EncoderConfig {
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    audio: bool,
}

fn create_encoder(
    config: &EncoderConfig,
    hardware_acceleration: bool,
) -> Result<VideoEncoder, windows_capture::encoder::VideoEncoderError> {
    VideoEncoder::new(
        VideoSettingsBuilder::new(config.width, config.height)
            .sub_type(VideoSettingsSubType::H264)
            .input_format(VideoInputFormat::Nv12Bt709)
            .bitrate(config.bitrate)
            .frame_rate(config.fps),
        AudioSettingsBuilder::new()
            .bitrate(AAC_BITRATE)
            .sample_rate(AUDIO_SAMPLE_RATE)
            .channel_count(AUDIO_CHANNELS as u32)
            .bit_per_sample(16)
            .disabled(!config.audio),
        ContainerSettingsBuilder::new()
            .sub_type(ContainerSettingsSubType::FMPEG4)
            .hardware_acceleration(hardware_acceleration),
        &config.path,
    )
}

struct MfEncoder {
    encoder: Option<VideoEncoder>,
    config: EncoderConfig,
    hardware: bool,
    frames_written: u64,
}

impl PipelineEncoder for MfEncoder {
    fn write_frame(&mut self, frame: &CaptureFrame) -> Result<(), AirecError> {
        let width = even(frame.width);
        let height = even(frame.height);
        let packed = nv12_bt709(frame, width, height);
        let timestamp_hns = i64::try_from(frame.t_ms.saturating_mul(10_000)).unwrap_or(i64::MAX);
        let first_result = self
            .encoder
            .as_mut()
            .expect("encoder exists until finish")
            .send_frame_buffer_at_timeline(&packed, timestamp_hns);
        match first_result {
            Ok(()) => {
                self.frames_written += 1;
                Ok(())
            }
            Err(hardware_error) if self.hardware && self.frames_written == 0 => {
                self.encoder.take();
                let _ = std::fs::remove_file(&self.config.path);
                eprintln!(
                    "warning: hardware H.264 encoder unavailable ({hardware_error}); falling back to software"
                );
                let mut software =
                    create_encoder(&self.config, false).map_err(|software_error| {
                        encoder_unavailable(&hardware_error, &software_error)
                    })?;
                software
                    .send_frame_buffer_at_timeline(&packed, timestamp_hns)
                    .and_then(|()| software.wait_until_ready(std::time::Duration::from_secs(1)))
                    .map_err(|software_error| {
                        encoder_unavailable(&hardware_error, &software_error)
                    })?;
                self.encoder = Some(software);
                self.hardware = false;
                self.frames_written = 1;
                Ok(())
            }
            Err(error) if !self.hardware && self.frames_written == 0 => Err(AirecError::new(
                ErrorCode::EncoderUnavailable,
                "software H.264 encoder failed to start",
                serde_json::json!({"software": error.to_string()}),
            )),
            Err(error) => Err(encoder_error(error)),
        }
    }

    fn write_audio(&mut self, samples: &[i16]) -> Result<(), AirecError> {
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.encoder
            .as_mut()
            .expect("encoder exists until finish")
            .send_audio_buffer(&bytes, 0)
            .map_err(encoder_error)
    }

    fn finish(&mut self) -> Result<(), AirecError> {
        self.encoder
            .take()
            .expect("encoder exists until finish")
            .finish()
            .map_err(encoder_error)
    }
}

fn encoder_unavailable(
    hardware: &windows_capture::encoder::VideoEncoderError,
    software: &windows_capture::encoder::VideoEncoderError,
) -> AirecError {
    AirecError::new(
        ErrorCode::EncoderUnavailable,
        "hardware and software H.264 encoders are unavailable",
        serde_json::json!({
            "hardware": hardware.to_string(),
            "software": software.to_string(),
        }),
    )
}

fn nv12_bt709(frame: &CaptureFrame, width: u32, height: u32) -> Vec<u8> {
    let planes = i420_bt709(frame, width, height);
    let mut nv12 = planes.y;
    nv12.reserve(planes.u.len() * 2);
    for (blue, red) in planes.u.into_iter().zip(planes.v) {
        nv12.extend([blue, red]);
    }
    nv12
}

fn encoder_error(error: windows_capture::encoder::VideoEncoderError) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"component": "encoder"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yuv::solid_frame;

    #[test]
    fn nv12_skips_row_padding_and_keeps_top_row_first() {
        let mut frame = solid_frame(2, 2, 12, [255, 255, 255]);
        frame.bgra[12..20].copy_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255]);

        let nv12 = nv12_bt709(&frame, 2, 2);

        assert_eq!(nv12, [235, 235, 16, 16, 128, 128]);
    }

    #[test]
    fn nv12_pads_odd_frames_with_black() {
        let frame = solid_frame(1, 1, 4, [255, 255, 255]);

        let nv12 = nv12_bt709(&frame, 2, 2);

        assert_eq!(nv12, [235, 16, 16, 16, 128, 128]);
    }
}
