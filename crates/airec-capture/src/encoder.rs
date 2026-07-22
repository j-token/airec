use std::path::{Path, PathBuf};

use airec_core::{
    AirecError, CaptureFrame, EncoderFactory, ErrorCode, PipelineEncoder, RecordingOptions, ResolvedTarget,
};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, ContainerSettingsSubType, VideoEncoder, VideoSettingsBuilder,
    VideoSettingsSubType,
};

#[derive(Default)]
pub struct WindowsEncoderFactory;

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
        match create_encoder(&config, true) {
            Ok(encoder) => Ok(Box::new(MfEncoder { encoder: Some(encoder), config, hardware: true })),
            Err(hardware_error) => {
                eprintln!("warning: hardware H.264 encoder unavailable ({hardware_error}); falling back to software");
                let encoder = create_encoder(&config, false).map_err(|software_error| {
                    AirecError::new(
                        ErrorCode::EncoderUnavailable,
                        "hardware and software H.264 encoders are unavailable",
                        serde_json::json!({
                            "hardware": hardware_error.to_string(),
                            "software": software_error.to_string(),
                        }),
                    )
                })?;
                Ok(Box::new(MfEncoder { encoder: Some(encoder), config, hardware: false }))
            }
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
}

impl PipelineEncoder for MfEncoder {
    fn write_frame(&mut self, frame: &CaptureFrame) -> Result<(), AirecError> {
        let packed = packed_bottom_up(frame, self.config.width, self.config.height);
        let timestamp_hns = i64::try_from(frame.t_ms.saturating_mul(10_000)).unwrap_or(i64::MAX);
        let result = self
            .encoder
            .as_mut()
            .expect("encoder exists until finish")
            .send_frame_buffer(&packed, timestamp_hns);
        if let Err(hardware_error) = result {
            if !self.hardware {
                return Err(encoder_error(hardware_error));
            }
            self.encoder.take();
            eprintln!("warning: hardware H.264 encoder failed on the first sample ({hardware_error}); falling back to software");
            let mut software = create_encoder(&self.config, false).map_err(|software_error| {
                AirecError::new(
                    ErrorCode::EncoderUnavailable,
                    "hardware encoder failed and software fallback could not start",
                    serde_json::json!({
                        "hardware": hardware_error.to_string(),
                        "software": software_error.to_string(),
                    }),
                )
            })?;
            software.send_frame_buffer(&packed, timestamp_hns).map_err(encoder_error)?;
            self.encoder = Some(software);
            self.hardware = false;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), AirecError> {
        self.encoder.take().expect("encoder exists until finish").finish().map_err(encoder_error)
    }
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
    AirecError::new(ErrorCode::OutputIoError, error.to_string(), serde_json::json!({"component": "encoder"}))
}

fn output_error(path: &Path, error: std::io::Error) -> AirecError {
    AirecError::new(ErrorCode::OutputIoError, error.to_string(), serde_json::json!({"path": path}))
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
            bgra: vec![1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 99, 99, 99, 99],
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
