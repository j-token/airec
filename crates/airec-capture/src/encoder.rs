use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use airec_core::{
    AirecError, CaptureFrame, EncoderFactory, ErrorCode, PipelineEncoder, RecordingOptions,
    ResolvedTarget,
};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, ContainerSettingsSubType, VideoEncoder,
    VideoInputFormat, VideoSettingsBuilder, VideoSettingsSubType,
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
            .input_format(VideoInputFormat::Nv12Bt709)
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

/// Converts a frame to top-down NV12 with the BT.709 limited-range matrix, averaging each
/// 2x2 block for chroma. Padding added to reach even dimensions is black.
fn nv12_bt709(frame: &CaptureFrame, width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let rgb_at = |x: usize, y: usize| -> [i32; 3] {
        if x >= frame.width as usize || y >= frame.height as usize {
            return [0, 0, 0];
        }
        let offset = y * frame.stride as usize + x * 4;
        match frame.bgra.get(offset..offset + 3) {
            Some(&[blue, green, red]) => [i32::from(red), i32::from(green), i32::from(blue)],
            _ => [0, 0, 0],
        }
    };

    let mut nv12 = vec![0_u8; width * height * 3 / 2];
    let (luma, chroma) = nv12.split_at_mut(width * height);
    for y in 0..height {
        for x in 0..width {
            luma[y * width + x] = bt709_luma(rgb_at(x, y));
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let block = [
                rgb_at(x, y),
                rgb_at(x + 1, y),
                rgb_at(x, y + 1),
                rgb_at(x + 1, y + 1),
            ];
            let average =
                |channel: usize| (block.iter().map(|rgb| rgb[channel]).sum::<i32>() + 2) / 4;
            let offset = y / 2 * width + x;
            chroma[offset..offset + 2].copy_from_slice(&bt709_chroma([
                average(0),
                average(1),
                average(2),
            ]));
        }
    }
    nv12
}

// BT.709 limited-range coefficients scaled by 2^16. Each chroma row sums to zero so that
// grays map exactly to 128.
fn bt709_luma([red, green, blue]: [i32; 3]) -> u8 {
    (16 + ((11_966 * red + 40_254 * green + 4_064 * blue + 32_768) >> 16)) as u8
}

fn bt709_chroma([red, green, blue]: [i32; 3]) -> [u8; 2] {
    let blue_difference = (-6_596 * red - 22_188 * green + 28_784 * blue + 32_768) >> 16;
    let red_difference = (28_784 * red - 26_145 * green - 2_639 * blue + 32_768) >> 16;
    [(128 + blue_difference) as u8, (128 + red_difference) as u8]
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

    fn solid_frame(width: u32, height: u32, stride: u32, rgb: [u8; 3]) -> CaptureFrame {
        let row = [rgb[2], rgb[1], rgb[0], 255]
            .repeat(width as usize)
            .into_iter()
            .chain(std::iter::repeat_n(99, (stride - width * 4) as usize))
            .collect::<Vec<_>>();
        CaptureFrame {
            width,
            height,
            stride,
            bgra: row.repeat(height as usize),
            t_ms: 0,
        }
    }

    #[test]
    fn bt709_limited_range_maps_reference_colors() {
        assert_eq!(bt709_luma([0, 0, 0]), 16);
        assert_eq!(bt709_luma([255, 255, 255]), 235);
        assert_eq!(bt709_chroma([128, 128, 128]), [128, 128]);
        assert_eq!(bt709_luma([255, 0, 0]), 63);
        assert_eq!(bt709_chroma([255, 0, 0]), [102, 240]);
        assert_eq!(bt709_luma([0, 0, 255]), 32);
        assert_eq!(bt709_chroma([0, 0, 255]), [240, 118]);
    }

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

    #[test]
    fn medium_quality_meets_prd_1080p_size_budget() {
        let bitrate = airec_core::Quality::Medium.bitrate(1920, 1080, 30);
        let bytes_per_minute = u64::from(bitrate) * 60 / 8;
        assert!(bytes_per_minute <= 20 * 1024 * 1024);
    }
}
