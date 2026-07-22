use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use airec_core::{
    AirecError, CaptureFrame, EncoderFactory, ErrorCode, PipelineEncoder, RecordingOptions,
    ResolvedTarget,
};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, ContainerSettingsSubType, VideoEncoder,
    VideoSettingsBuilder, VideoSettingsSubType,
};

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

#[derive(Clone, Debug)]
pub struct EncoderDiagnostics {
    pub hardware_available: bool,
    pub software_available: bool,
    pub selected: Option<&'static str>,
    pub hardware_error: Option<String>,
    pub software_error: Option<String>,
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
    };
    let result = (|| {
        let mut encoder = create_encoder(&config, hardware_acceleration)?;
        let frame = vec![0_u8; config.width as usize * config.height as usize * 4];
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

fn even(value: u32) -> u32 {
    value.max(2).next_multiple_of(2)
}

#[derive(Clone)]
struct EncoderConfig {
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

fn create_encoder(
    config: &EncoderConfig,
    hardware_acceleration: bool,
) -> Result<VideoEncoder, windows_capture::encoder::VideoEncoderError> {
    VideoEncoder::new(
        VideoSettingsBuilder::new(config.width, config.height)
            .sub_type(VideoSettingsSubType::H264)
            .bitrate(config.bitrate)
            .frame_rate(config.fps),
        AudioSettingsBuilder::new().disabled(true),
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
        let packed = packed_bottom_up(frame, width, height);
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

fn packed_bottom_up(frame: &CaptureFrame, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut output = vec![0_u8; row_bytes * height as usize];
    let copy_width = width.min(frame.width) as usize * 4;
    let copy_height = height.min(frame.height);
    for y in 0..copy_height {
        let source = y as usize * frame.stride as usize;
        let destination_y = height - 1 - y;
        let destination = destination_y as usize * row_bytes;
        if source + copy_width <= frame.bgra.len() {
            output[destination..destination + copy_width]
                .copy_from_slice(&frame.bgra[source..source + copy_width]);
        }
    }
    output
}

fn encoder_error(error: windows_capture::encoder::VideoEncoderError) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"component": "encoder"}),
    )
}

fn output_error(path: &Path, error: std::io::Error) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({"path": path}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_sizes_are_padded_for_yuv420_without_cropping() {
        assert_eq!(even(1919), 1920);
        assert_eq!(even(1081), 1082);
    }

    #[test]
    fn padded_rows_are_removed_and_frame_is_flipped_for_media_foundation() {
        let frame = CaptureFrame {
            width: 2,
            height: 2,
            stride: 12,
            bgra: vec![
                1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 99, 99, 99,
                99,
            ],
            t_ms: 0,
        };
        assert_eq!(
            packed_bottom_up(&frame, 2, 2),
            vec![9, 10, 11, 12, 13, 14, 15, 16, 1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn medium_quality_meets_prd_1080p_size_budget() {
        let bitrate = airec_core::Quality::Medium.bitrate(1920, 1080, 30);
        let bytes_per_minute = u64::from(bitrate) * 60 / 8;
        assert!(bytes_per_minute <= 20 * 1024 * 1024);
    }
}
