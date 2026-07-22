//! Offline MP4-to-GIF conversion using Windows Media Foundation.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use airec_core::{AirecError, ErrorCode};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaBuffer, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup,
    MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{Interface, PCWSTR};

const HNS_PER_SECOND: u128 = 10_000_000;
const GIF_TIME_UNITS_PER_SECOND: u128 = 100;

/// Settings for converting an MP4 recording into an animated GIF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversionOptions {
    /// Requested GIF sampling rate. GIF timing supports at most 100 frames per second.
    pub fps: u32,
    /// Downscale frames wider than this value while preserving their aspect ratio.
    pub max_width: Option<u32>,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            fps: 10,
            max_width: None,
        }
    }
}

/// Metadata for a completed GIF conversion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionResult {
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub duration_ms: u64,
}

/// Decode an H.264 MP4 with Windows Media Foundation and write an animated GIF89a.
///
/// The destination is never overwritten. Data is first written to a uniquely named file in
/// the destination directory and renamed only after the GIF is complete and synchronized.
pub fn convert_mp4_to_gif(
    input: &Path,
    output: &Path,
    options: ConversionOptions,
) -> Result<ConversionResult, AirecError> {
    validate_options(options)?;
    if output
        .try_exists()
        .map_err(|error| output_error(output, "inspect destination", error))?
    {
        return Err(AirecError::new(
            ErrorCode::OutputIoError,
            "refusing to overwrite an existing output file",
            serde_json::json!({"path": output}),
        ));
    }

    let input = input.canonicalize().map_err(|error| {
        decode_error(
            input,
            "open input",
            format!("cannot open input MP4: {error}"),
        )
    })?;
    let mut temporary = TemporaryOutput::create(output)?;
    let file = temporary
        .take_file()
        .expect("a newly created temporary output owns its file");
    let conversion = decode_to_gif(&input, output, file, options)?;

    std::fs::rename(temporary.path(), output)
        .map_err(|error| output_error(output, "commit temporary GIF", error))?;
    temporary.mark_committed();
    Ok(conversion)
}

fn validate_options(options: ConversionOptions) -> Result<(), AirecError> {
    if !(1..=100).contains(&options.fps) {
        return Err(AirecError::new(
            ErrorCode::CaptureInitFailed,
            "GIF fps must be between 1 and 100",
            serde_json::json!({"fps": options.fps}),
        ));
    }
    if options.max_width == Some(0) {
        return Err(AirecError::new(
            ErrorCode::CaptureInitFailed,
            "GIF maximum width must be greater than zero",
            serde_json::json!({"max_width": options.max_width}),
        ));
    }
    Ok(())
}

struct TemporaryOutput {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl TemporaryOutput {
    fn create(output: &Path) -> Result<Self, AirecError> {
        let parent = output.parent().filter(|path| !path.as_os_str().is_empty());
        let directory = parent.unwrap_or_else(|| Path::new("."));
        let name = output
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output.gif");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for attempt in 0..100_u32 {
            let path = directory.join(format!(
                ".{name}.airec-{}-{nonce}-{attempt}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(output_error(output, "create temporary GIF", error));
                }
            }
        }

        Err(AirecError::new(
            ErrorCode::OutputIoError,
            "could not allocate a unique temporary output file",
            serde_json::json!({"path": output}),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn take_file(&mut self) -> Option<File> {
        self.file.take()
    }

    fn mark_committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn decode_to_gif(
    input: &Path,
    output: &Path,
    file: File,
    options: ConversionOptions,
) -> Result<ConversionResult, AirecError> {
    let _com = ComApartment::initialize(input)?;
    let _media_foundation = MediaFoundationSession::start(input)?;
    decode_with_media_foundation(input, output, file, options)
}

struct ComApartment {
    uninitialize: bool,
}

impl ComApartment {
    fn initialize(input: &Path) -> Result<Self, AirecError> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            Ok(Self { uninitialize: true })
        } else if result == RPC_E_CHANGED_MODE {
            // The caller already initialized this thread in another apartment. It remains valid
            // for synchronous SourceReader use and must not be uninitialized here.
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(decode_error(
                input,
                "initialize COM",
                format!("COM initialization failed: {result:?}"),
            ))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

struct MediaFoundationSession;

impl MediaFoundationSession {
    fn start(input: &Path) -> Result<Self, AirecError> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|error| {
            decode_error(
                input,
                "start Media Foundation",
                format!("Media Foundation startup failed: {error}"),
            )
        })?;
        Ok(Self)
    }
}

impl Drop for MediaFoundationSession {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameLayout {
    width: u32,
    height: u32,
    stride: i32,
}

struct TimestampSampler {
    fps: u32,
    first_timestamp: Option<i64>,
    next_sample_slot: u64,
    source_end_hns: u64,
    last_source_timestamp_hns: Option<u64>,
    last_positive_delta_hns: Option<u64>,
    last_sample_duration_hns: u64,
}

struct SampleTiming {
    relative_hns: u64,
    selected: bool,
}

impl TimestampSampler {
    fn new(fps: u32) -> Self {
        Self {
            fps,
            first_timestamp: None,
            next_sample_slot: 0,
            source_end_hns: 0,
            last_source_timestamp_hns: None,
            last_positive_delta_hns: None,
            last_sample_duration_hns: 0,
        }
    }

    fn observe(&mut self, timestamp: i64, duration_hns: u64) -> SampleTiming {
        let base = *self.first_timestamp.get_or_insert(timestamp);
        let relative_hns = timestamp.saturating_sub(base).max(0) as u64;
        if let Some(previous_hns) = self.last_source_timestamp_hns
            && relative_hns > previous_hns
        {
            self.last_positive_delta_hns = Some(relative_hns - previous_hns);
        }
        self.last_source_timestamp_hns = Some(relative_hns);
        self.last_sample_duration_hns = duration_hns;
        self.source_end_hns = self
            .source_end_hns
            .max(relative_hns.saturating_add(duration_hns));
        let selected = u128::from(relative_hns) * u128::from(self.fps)
            >= u128::from(self.next_sample_slot) * HNS_PER_SECOND;
        if selected {
            self.next_sample_slot = self.next_sample_slot.saturating_add(1);
            while u128::from(relative_hns) * u128::from(self.fps)
                >= u128::from(self.next_sample_slot) * HNS_PER_SECOND
            {
                self.next_sample_slot = self.next_sample_slot.saturating_add(1);
            }
        }
        SampleTiming {
            relative_hns,
            selected,
        }
    }

    fn end_hns(&self, last_frame_start_hns: u64) -> u64 {
        let nominal_frame_hns =
            u64::try_from(HNS_PER_SECOND.div_ceil(u128::from(self.fps))).unwrap_or(u64::MAX);
        let final_frame_duration_hns = if self.last_sample_duration_hns != 0 {
            self.last_sample_duration_hns
        } else {
            self.last_positive_delta_hns.unwrap_or(nominal_frame_hns)
        };
        let inferred_source_end_hns = self
            .last_source_timestamp_hns
            .unwrap_or(last_frame_start_hns)
            .saturating_add(final_frame_duration_hns);
        let end_hns = self.source_end_hns.max(inferred_source_end_hns);
        if end_hns > last_frame_start_hns {
            end_hns
        } else {
            last_frame_start_hns.saturating_add(nominal_frame_hns)
        }
    }
}

struct PendingFrame {
    indexed: Vec<u8>,
    start_hns: u64,
}

fn decode_with_media_foundation(
    input: &Path,
    output: &Path,
    file: File,
    options: ConversionOptions,
) -> Result<ConversionResult, AirecError> {
    let wide_path = source_reader_wide_path(input);
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1) }
        .map_err(|error| mf_error(input, "create SourceReader attributes", error))?;
    let attributes = attributes.ok_or_else(|| {
        decode_error(
            input,
            "create SourceReader attributes",
            "Media Foundation returned no SourceReader attributes",
        )
    })?;
    unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }
        .map_err(|error| mf_error(input, "enable RGB32 video processing", error))?;
    let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), &attributes) }
        .map_err(|error| mf_error(input, "create SourceReader", error))?;

    configure_reader(input, &reader)?;
    let mut layout = current_layout(input, &reader)?;
    let mut writer = Some(BufWriter::new(file));
    let mut gif = None;
    let mut output_dimensions = None;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut sampler = TimestampSampler::new(options.fps);
    let mut pending_frame: Option<PendingFrame> = None;
    let mut frames = 0_u64;
    let mut duration_ms = 0_u64;

    loop {
        let mut flags = 0_u32;
        let mut timestamp = 0_i64;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|error| mf_error(input, "decode video sample", error))?;

        if flags & (MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
            return Err(decode_error(
                input,
                "decode video sample",
                "Media Foundation reported a SourceReader error",
            ));
        }
        if flags & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
            let changed = current_layout(input, &reader)?;
            if gif.is_some() && (changed.width != layout.width || changed.height != layout.height) {
                return Err(decode_error(
                    input,
                    "handle media type change",
                    "video dimensions changed during conversion",
                ));
            }
            layout = changed;
        }

        if let Some(sample) = sample {
            let sample_duration = unsafe { sample.GetSampleDuration() }.unwrap_or(0).max(0) as u64;
            let timing = sampler.observe(timestamp, sample_duration);
            if timing.selected {
                if gif.is_none() {
                    let (output_width, output_height) =
                        scaled_dimensions(layout.width, layout.height, options.max_width)?;
                    let gif_width = u16::try_from(output_width)
                        .map_err(|_| dimension_error(output_width, output_height))?;
                    let gif_height = u16::try_from(output_height)
                        .map_err(|_| dimension_error(output_width, output_height))?;
                    gif = Some(
                        GifEncoder::new(
                            writer
                                .take()
                                .expect("GIF writer is consumed with the first frame"),
                            gif_width,
                            gif_height,
                        )
                        .map_err(|error| output_error(output, "write GIF header", error))?,
                    );
                    output_dimensions = Some((output_width, output_height));
                }
                let (output_width, output_height) =
                    output_dimensions.expect("GIF dimensions accompany its writer");
                let bgra = copy_bgra_sample(input, &sample, layout)?;
                let scaled = downscale_bgra(
                    &bgra,
                    layout.width,
                    layout.height,
                    output_width,
                    output_height,
                );
                let indexed = quantize_bgra(&scaled);
                if let Some(previous) = pending_frame.take() {
                    let (written_frames, written_duration_ms) = write_timed_frame(
                        gif.as_mut()
                            .expect("GIF writer exists after first sampled frame"),
                        &previous.indexed,
                        previous.start_hns,
                        timing.relative_hns,
                    )
                    .map_err(|error| output_error(output, "write GIF frame", error))?;
                    frames = frames.saturating_add(written_frames);
                    duration_ms = duration_ms.saturating_add(written_duration_ms);
                }
                pending_frame = Some(PendingFrame {
                    indexed,
                    start_hns: timing.relative_hns,
                });
            }
        }

        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
    }

    let pending_frame = pending_frame.ok_or_else(|| {
        decode_error(
            input,
            "decode video",
            "input contains no decodable video frames",
        )
    })?;
    let end_hns = sampler.end_hns(pending_frame.start_hns);
    let (written_frames, written_duration_ms) = write_timed_frame(
        gif.as_mut()
            .expect("a pending frame has an initialized GIF writer"),
        &pending_frame.indexed,
        pending_frame.start_hns,
        end_hns,
    )
    .map_err(|error| output_error(output, "write final GIF frame", error))?;
    frames = frames.saturating_add(written_frames);
    duration_ms = duration_ms.saturating_add(written_duration_ms);

    let mut writer = gif
        .expect("a non-empty conversion initialized its GIF writer")
        .finish()
        .map_err(|error| output_error(output, "finalize GIF", error))?;
    writer
        .flush()
        .map_err(|error| output_error(output, "flush GIF", error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| output_error(output, "synchronize GIF", error))?;
    drop(writer);
    let (output_width, output_height) =
        output_dimensions.expect("a non-empty conversion has GIF dimensions");

    Ok(ConversionResult {
        output: output.to_path_buf(),
        width: output_width,
        height: output_height,
        frames,
        duration_ms,
    })
}

fn source_reader_wide_path(path: &Path) -> Vec<u16> {
    const VERBATIM_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC_PREFIX: [u16; 8] = [
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut source_reader_path = if encoded.starts_with(&VERBATIM_UNC_PREFIX) {
        let mut ordinary_unc = vec![b'\\' as u16, b'\\' as u16];
        ordinary_unc.extend_from_slice(&encoded[VERBATIM_UNC_PREFIX.len()..]);
        ordinary_unc
    } else if encoded.starts_with(&VERBATIM_PREFIX) {
        encoded[VERBATIM_PREFIX.len()..].to_vec()
    } else {
        encoded
    };
    source_reader_path.push(0);
    source_reader_path
}

fn configure_reader(input: &Path, reader: &IMFSourceReader) -> Result<(), AirecError> {
    let all_streams = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
    let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    unsafe { reader.SetStreamSelection(all_streams, false) }
        .map_err(|error| mf_error(input, "deselect non-video streams", error))?;
    unsafe { reader.SetStreamSelection(video_stream, true) }
        .map_err(|error| mf_error(input, "select video stream", error))?;

    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|error| mf_error(input, "create RGB32 media type", error))?;
    unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
        .map_err(|error| mf_error(input, "set video major type", error))?;
    unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }
        .map_err(|error| mf_error(input, "set RGB32 subtype", error))?;
    unsafe { reader.SetCurrentMediaType(video_stream, None, &media_type) }
        .map_err(|error| mf_error(input, "enable H.264 to RGB32 decoding", error))
}

fn current_layout(input: &Path, reader: &IMFSourceReader) -> Result<FrameLayout, AirecError> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let media_type = unsafe { reader.GetCurrentMediaType(stream) }
        .map_err(|error| mf_error(input, "read decoded media type", error))?;
    let packed_size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|error| mf_error(input, "read decoded frame size", error))?;
    let width = (packed_size >> 32) as u32;
    let height = packed_size as u32;
    if width == 0 || height == 0 {
        return Err(decode_error(
            input,
            "read decoded frame size",
            "decoded video has zero width or height",
        ));
    }
    let packed_stride = width
        .checked_mul(4)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| dimension_error(width, height))?;
    let stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|value| value as i32)
        .unwrap_or(packed_stride);
    if stride == i32::MIN || stride.unsigned_abs() < width.saturating_mul(4) {
        return Err(decode_error(
            input,
            "read decoded frame stride",
            "decoded RGB32 stride is invalid",
        ));
    }
    Ok(FrameLayout {
        width,
        height,
        stride,
    })
}

fn copy_bgra_sample(
    input: &Path,
    sample: &IMFSample,
    layout: FrameLayout,
) -> Result<Vec<u8>, AirecError> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|error| mf_error(input, "make decoded frame contiguous", error))?;
    if let Ok(buffer_2d) = buffer.cast::<IMF2DBuffer>() {
        copy_locked_2d(input, &buffer_2d, layout)
    } else {
        copy_locked_contiguous(input, &buffer, layout)
    }
}

fn frame_buffer_size(layout: FrameLayout) -> Result<usize, AirecError> {
    let pixels = u64::from(layout.width)
        .checked_mul(u64::from(layout.height))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| dimension_error(layout.width, layout.height))?;
    Ok(pixels)
}

fn copy_locked_2d(
    input: &Path,
    buffer: &IMF2DBuffer,
    layout: FrameLayout,
) -> Result<Vec<u8>, AirecError> {
    let mut scanline_zero = std::ptr::null_mut();
    let mut pitch = 0_i32;
    unsafe { buffer.Lock2D(&mut scanline_zero, &mut pitch) }
        .map_err(|error| mf_error(input, "lock decoded 2D frame", error))?;
    let copy_result = (|| {
        let row_bytes = usize::try_from(layout.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| dimension_error(layout.width, layout.height))?;
        if scanline_zero.is_null()
            || pitch == i32::MIN
            || usize::try_from(pitch.unsigned_abs()).unwrap_or(0) < row_bytes
        {
            return Err(decode_error(
                input,
                "lock decoded 2D frame",
                "decoded frame returned an invalid scanline or pitch",
            ));
        }
        let mut output = vec![0_u8; frame_buffer_size(layout)?];
        for y in 0..usize::try_from(layout.height).unwrap_or(0) {
            let source = unsafe { scanline_zero.offset((y as isize) * (pitch as isize)) };
            let destination = &mut output[y * row_bytes..(y + 1) * row_bytes];
            unsafe { std::ptr::copy_nonoverlapping(source, destination.as_mut_ptr(), row_bytes) };
        }
        Ok(output)
    })();
    let unlock_result = unsafe { buffer.Unlock2D() }
        .map_err(|error| mf_error(input, "unlock decoded 2D frame", error));
    match (copy_result, unlock_result) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    }
}

fn copy_locked_contiguous(
    input: &Path,
    buffer: &IMFMediaBuffer,
    layout: FrameLayout,
) -> Result<Vec<u8>, AirecError> {
    let mut data = std::ptr::null_mut();
    let mut length = 0_u32;
    unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
        .map_err(|error| mf_error(input, "lock decoded frame", error))?;
    let copy_result = (|| {
        let row_bytes = usize::try_from(layout.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| dimension_error(layout.width, layout.height))?;
        let stride = usize::try_from(layout.stride.unsigned_abs()).unwrap_or(0);
        let required = stride
            .checked_mul(usize::try_from(layout.height).unwrap_or(usize::MAX))
            .ok_or_else(|| dimension_error(layout.width, layout.height))?;
        if data.is_null() || stride < row_bytes || required > length as usize {
            return Err(decode_error(
                input,
                "copy decoded frame",
                "decoded frame buffer is shorter than its media type requires",
            ));
        }
        let source = unsafe { std::slice::from_raw_parts(data, length as usize) };
        let mut output = vec![0_u8; frame_buffer_size(layout)?];
        let height = usize::try_from(layout.height).unwrap_or(0);
        for y in 0..height {
            let source_y = if layout.stride >= 0 {
                y
            } else {
                height - 1 - y
            };
            let source_start = source_y * stride;
            let destination_start = y * row_bytes;
            output[destination_start..destination_start + row_bytes]
                .copy_from_slice(&source[source_start..source_start + row_bytes]);
        }
        Ok(output)
    })();
    let unlock_result =
        unsafe { buffer.Unlock() }.map_err(|error| mf_error(input, "unlock decoded frame", error));
    match (copy_result, unlock_result) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    }
}

fn scaled_dimensions(
    width: u32,
    height: u32,
    max_width: Option<u32>,
) -> Result<(u32, u32), AirecError> {
    if width == 0 || height == 0 || max_width == Some(0) {
        return Err(dimension_error(width, height));
    }
    let output_width = max_width.map_or(width, |maximum| width.min(maximum));
    let output_height = if output_width == width {
        height
    } else {
        let numerator = u64::from(height) * u64::from(output_width);
        u32::try_from((numerator + u64::from(width) / 2) / u64::from(width))
            .unwrap_or(u32::MAX)
            .max(1)
    };
    if output_width > u32::from(u16::MAX) || output_height > u32::from(u16::MAX) {
        return Err(dimension_error(output_width, output_height));
    }
    Ok((output_width, output_height))
}

fn downscale_bgra(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    destination_width: u32,
    destination_height: u32,
) -> Vec<u8> {
    if source_width == destination_width && source_height == destination_height {
        return source.to_vec();
    }
    let destination_len = usize::try_from(destination_width)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(destination_height).unwrap_or(0))
        .saturating_mul(4);
    let mut destination = vec![0_u8; destination_len];
    for y in 0..destination_height {
        let source_y = u64::from(y) * u64::from(source_height) / u64::from(destination_height);
        for x in 0..destination_width {
            let source_x = u64::from(x) * u64::from(source_width) / u64::from(destination_width);
            let source_index =
                usize::try_from((source_y * u64::from(source_width) + source_x).saturating_mul(4))
                    .unwrap_or(0);
            let destination_index = usize::try_from(
                (u64::from(y) * u64::from(destination_width) + u64::from(x)).saturating_mul(4),
            )
            .unwrap_or(0);
            destination[destination_index..destination_index + 4]
                .copy_from_slice(&source[source_index..source_index + 4]);
        }
    }
    destination
}

fn quantize_bgra(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .map(|pixel| {
            let blue = pixel[0] >> 6;
            let green = pixel[1] >> 5;
            let red = pixel[2] >> 5;
            (red << 5) | (green << 2) | blue
        })
        .collect()
}

fn fixed_palette() -> [u8; 256 * 3] {
    let mut palette = [0_u8; 256 * 3];
    for index in 0_u16..=255 {
        let red = ((index >> 5) & 0x07) as u8;
        let green = ((index >> 2) & 0x07) as u8;
        let blue = (index & 0x03) as u8;
        let offset = usize::from(index) * 3;
        palette[offset] = ((u16::from(red) * 255 + 3) / 7) as u8;
        palette[offset + 1] = ((u16::from(green) * 255 + 3) / 7) as u8;
        palette[offset + 2] = ((u16::from(blue) * 255 + 1) / 3) as u8;
    }
    palette
}

fn timeline_centiseconds(timestamp_hns: u64) -> u64 {
    u64::try_from(
        (u128::from(timestamp_hns) * GIF_TIME_UNITS_PER_SECOND + HNS_PER_SECOND / 2)
            / HNS_PER_SECOND,
    )
    .unwrap_or(u64::MAX)
}

fn timeline_delay_centiseconds(start_hns: u64, end_hns: u64) -> u64 {
    timeline_centiseconds(end_hns)
        .saturating_sub(timeline_centiseconds(start_hns))
        .max(1)
}

fn write_timed_frame<W: Write>(
    gif: &mut GifEncoder<W>,
    indexed: &[u8],
    start_hns: u64,
    end_hns: u64,
) -> io::Result<(u64, u64)> {
    let mut remaining_delay = timeline_delay_centiseconds(start_hns, end_hns);
    let total_delay = remaining_delay;
    let mut frames = 0_u64;
    while remaining_delay != 0 {
        let delay = u16::try_from(remaining_delay.min(u64::from(u16::MAX)))
            .expect("GIF delay chunk is bounded by u16::MAX");
        gif.write_frame(indexed, delay)?;
        remaining_delay -= u64::from(delay);
        frames += 1;
    }
    Ok((frames, total_delay.saturating_mul(10)))
}

struct GifEncoder<W: Write> {
    writer: W,
    width: u16,
    height: u16,
}

impl<W: Write> GifEncoder<W> {
    fn new(mut writer: W, width: u16, height: u16) -> io::Result<Self> {
        writer.write_all(b"GIF89a")?;
        writer.write_all(&width.to_le_bytes())?;
        writer.write_all(&height.to_le_bytes())?;
        writer.write_all(&[0xf7, 0, 0])?;
        writer.write_all(&fixed_palette())?;
        // Netscape application extension: repeat forever (loop count zero).
        writer.write_all(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00")?;
        Ok(Self {
            writer,
            width,
            height,
        })
    }

    fn write_frame(&mut self, indexed: &[u8], delay_centiseconds: u16) -> io::Result<()> {
        let expected = usize::from(self.width) * usize::from(self.height);
        if indexed.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "indexed frame does not match the GIF dimensions",
            ));
        }
        // Graphic Control Extension: full opaque frame with no disposal requirement.
        self.writer.write_all(b"\x21\xf9\x04\x00")?;
        self.writer.write_all(&delay_centiseconds.to_le_bytes())?;
        self.writer.write_all(b"\x00\x00")?;
        // Full-screen Image Descriptor, using the global palette.
        self.writer.write_all(b"\x2c\x00\x00\x00\x00")?;
        self.writer.write_all(&self.width.to_le_bytes())?;
        self.writer.write_all(&self.height.to_le_bytes())?;
        self.writer.write_all(b"\x00")?;
        self.writer.write_all(b"\x08")?;
        let compressed = lzw_encode(indexed);
        for block in compressed.chunks(255) {
            self.writer.write_all(&[block.len() as u8])?;
            self.writer.write_all(block)?;
        }
        self.writer.write_all(b"\x00")
    }

    fn finish(mut self) -> io::Result<W> {
        self.writer.write_all(b"\x3b")?;
        self.writer.flush()?;
        Ok(self.writer)
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: u32,
    bit_count: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bits: 0,
            bit_count: 0,
        }
    }

    fn write(&mut self, code: u16, width: u8) {
        self.bits |= u32::from(code) << self.bit_count;
        self.bit_count += width;
        while self.bit_count >= 8 {
            self.bytes.push(self.bits as u8);
            self.bits >>= 8;
            self.bit_count -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count != 0 {
            self.bytes.push(self.bits as u8);
        }
        self.bytes
    }
}

fn lzw_encode(indexed: &[u8]) -> Vec<u8> {
    const CLEAR: u16 = 256;
    const END: u16 = 257;
    const FIRST_FREE: u16 = 258;
    const MAX_CODE: u16 = 4095;

    let mut output = BitWriter::new();
    let mut dictionary = HashMap::<(u16, u8), u16>::new();
    let mut next_code = FIRST_FREE;
    let mut code_width = 9_u8;
    output.write(CLEAR, code_width);

    let Some((&first, rest)) = indexed.split_first() else {
        output.write(END, code_width);
        return output.finish();
    };
    let mut prefix = u16::from(first);
    for &suffix in rest {
        if let Some(&combined) = dictionary.get(&(prefix, suffix)) {
            prefix = combined;
            continue;
        }

        output.write(prefix, code_width);
        // The decoder adds the previous dictionary entry after reading this code. Therefore a
        // pending width increase takes effect only after this code has been emitted.
        if next_code == (1_u16 << code_width) && code_width < 12 {
            code_width += 1;
        }
        if next_code <= MAX_CODE {
            dictionary.insert((prefix, suffix), next_code);
            next_code += 1;
        } else {
            output.write(CLEAR, code_width);
            dictionary.clear();
            next_code = FIRST_FREE;
            code_width = 9;
        }
        prefix = u16::from(suffix);
    }

    output.write(prefix, code_width);
    if next_code == (1_u16 << code_width) && code_width < 12 {
        code_width += 1;
    }
    output.write(END, code_width);
    output.finish()
}

fn dimension_error(width: u32, height: u32) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        "video dimensions cannot be represented by GIF",
        serde_json::json!({"width": width, "height": height}),
    )
}

fn mf_error(input: &Path, operation: &'static str, error: windows::core::Error) -> AirecError {
    decode_error(input, operation, error.to_string())
}

fn decode_error(input: &Path, operation: &'static str, message: impl Into<String>) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        message,
        serde_json::json!({
            "component": "media_foundation_decoder",
            "operation": operation,
            "input": input,
        }),
    )
}

fn output_error(
    output: &Path,
    operation: &'static str,
    error: impl std::fmt::Display,
) -> AirecError {
    AirecError::new(
        ErrorCode::OutputIoError,
        error.to_string(),
        serde_json::json!({
            "component": "gif_writer",
            "operation": operation,
            "path": output,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "airec-convert-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn fixed_palette_and_quantizer_are_stable() {
        let palette = fixed_palette();
        assert_eq!(&palette[0..3], &[0, 0, 0]);
        assert_eq!(&palette[255 * 3..], &[255, 255, 255]);
        assert_eq!(quantize_bgra(&[0, 0, 255, 255]), vec![0b1110_0000]);
        assert_eq!(quantize_bgra(&[255, 0, 0, 255]), vec![0b0000_0011]);
        assert_eq!(quantize_bgra(&[0, 255, 0, 255]), vec![0b0001_1100]);
    }

    #[test]
    fn dimensions_preserve_aspect_ratio_without_upscaling() {
        assert_eq!(
            scaled_dimensions(1920, 1080, Some(640)).unwrap(),
            (640, 360)
        );
        assert_eq!(scaled_dimensions(320, 200, Some(640)).unwrap(), (320, 200));
        assert_eq!(scaled_dimensions(3, 2, Some(2)).unwrap(), (2, 1));
    }

    #[test]
    fn source_reader_path_removes_verbatim_prefixes() {
        let drive = source_reader_wide_path(Path::new(r"\\?\C:\recordings\evidence.mp4"));
        assert_eq!(
            String::from_utf16(&drive[..drive.len() - 1]).unwrap(),
            r"C:\recordings\evidence.mp4"
        );
        let unc = source_reader_wide_path(Path::new(r"\\?\UNC\server\share\evidence.mp4"));
        assert_eq!(
            String::from_utf16(&unc[..unc.len() - 1]).unwrap(),
            r"\\server\share\evidence.mp4"
        );
    }

    #[test]
    fn sparse_source_timestamps_preserve_elapsed_timeline() {
        let mut sampler = TimestampSampler::new(10);
        let samples = [
            (0_i64, 5_000_000_u64),
            (5_000_000, 5_000_000),
            (20_000_000, 5_000_000),
        ];
        let selected = samples
            .into_iter()
            .filter_map(|(timestamp, duration)| {
                let timing = sampler.observe(timestamp, duration);
                timing.selected.then_some(timing.relative_hns)
            })
            .collect::<Vec<_>>();
        assert_eq!(selected, [0, 5_000_000, 20_000_000]);
        let end = sampler.end_hns(*selected.last().unwrap());
        let delays = selected
            .iter()
            .copied()
            .zip(selected.iter().copied().skip(1).chain(std::iter::once(end)))
            .map(|(start, end)| timeline_delay_centiseconds(start, end))
            .collect::<Vec<_>>();
        assert_eq!(delays, [50, 150, 50]);
        assert_eq!(delays.into_iter().sum::<u64>(), 250);
    }

    #[test]
    fn missing_final_duration_uses_last_source_timestamp_delta() {
        let mut sampler = TimestampSampler::new(10);
        let first = sampler.observe(0, 0);
        let second = sampler.observe(10_000_000, 0);
        assert!(first.selected);
        assert!(second.selected);
        assert_eq!(sampler.end_hns(second.relative_hns), 20_000_000);
        assert_eq!(
            timeline_delay_centiseconds(second.relative_hns, sampler.end_hns(second.relative_hns)),
            100
        );
    }

    #[test]
    fn nearest_neighbor_downscale_selects_expected_pixels() {
        let source = [
            1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255, 7,
            0, 0, 255, 8, 0, 0, 255, 9, 0, 0, 255, 10, 0, 0, 255, 11, 0, 0, 255, 12, 0, 0, 255, 13,
            0, 0, 255, 14, 0, 0, 255, 15, 0, 0, 255, 16, 0, 0, 255,
        ];
        let scaled = downscale_bgra(&source, 4, 4, 2, 2);
        assert_eq!(
            scaled,
            [1, 0, 0, 255, 3, 0, 0, 255, 9, 0, 0, 255, 11, 0, 0, 255]
        );
    }

    #[test]
    fn lzw_round_trips_across_code_width_changes() {
        let pixels = (0..20_000)
            .map(|index| ((index * 73 + index / 17) & 0xff) as u8)
            .collect::<Vec<_>>();
        let compressed = lzw_encode(&pixels);
        assert_eq!(decode_lzw(&compressed), pixels);
    }

    #[test]
    fn gif_has_loop_extension_frames_delays_and_trailer() {
        let mut gif = GifEncoder::new(Vec::new(), 2, 1).unwrap();
        gif.write_frame(&[0, 255], 8).unwrap();
        gif.write_frame(&[255, 0], 9).unwrap();
        let bytes = gif.finish().unwrap();
        assert_eq!(&bytes[..6], b"GIF89a");
        assert_eq!(&bytes[6..10], &[2, 0, 1, 0]);
        assert!(
            bytes
                .windows(19)
                .any(|window| window == b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00")
        );
        assert!(
            bytes
                .windows(8)
                .any(|window| window == b"\x21\xf9\x04\x00\x08\x00\x00\x00")
        );
        assert!(
            bytes
                .windows(8)
                .any(|window| window == b"\x21\xf9\x04\x00\x09\x00\x00\x00")
        );
        assert_eq!(bytes.last(), Some(&0x3b));
    }

    #[test]
    fn existing_output_is_not_overwritten() {
        let directory = test_directory("collision");
        let input = directory.join("invalid.mp4");
        let output = directory.join("existing.gif");
        std::fs::write(&input, b"not an mp4").unwrap();
        std::fs::write(&output, b"keep me").unwrap();
        let error = convert_mp4_to_gif(&input, &output, ConversionOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputIoError);
        assert_eq!(std::fs::read(&output).unwrap(), b"keep me");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_input_does_not_leave_output_or_temporary_file() {
        let directory = test_directory("invalid");
        let input = directory.join("invalid.mp4");
        let output = directory.join("result.gif");
        std::fs::write(&input, b"not an mp4").unwrap();
        let error = convert_mp4_to_gif(&input, &output, ConversionOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::CaptureInitFailed);
        assert!(!output.exists());
        let remaining = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(remaining, vec![input.file_name().unwrap()]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn decode_lzw(bytes: &[u8]) -> Vec<u8> {
        const CLEAR: u16 = 256;
        const END: u16 = 257;
        let mut reader = BitReader::new(bytes);
        let mut dictionary = (0_u16..=255)
            .map(|value| vec![value as u8])
            .collect::<Vec<_>>();
        dictionary.push(Vec::new());
        dictionary.push(Vec::new());
        let mut width = 9_u8;
        let mut previous: Option<Vec<u8>> = None;
        let mut output = Vec::new();
        while let Some(code) = reader.read(width) {
            if code == CLEAR {
                dictionary.truncate(258);
                width = 9;
                previous = None;
                continue;
            }
            if code == END {
                break;
            }
            let entry = if let Some(entry) = dictionary.get(usize::from(code)) {
                entry.clone()
            } else if usize::from(code) == dictionary.len() {
                let mut entry = previous.clone().unwrap();
                entry.push(entry[0]);
                entry
            } else {
                panic!("invalid LZW code {code}");
            };
            output.extend_from_slice(&entry);
            if let Some(mut prefix) = previous {
                prefix.push(entry[0]);
                if dictionary.len() < 4096 {
                    dictionary.push(prefix);
                    if dictionary.len() == (1_usize << width) && width < 12 {
                        width += 1;
                    }
                }
            }
            previous = Some(entry);
        }
        output
    }

    struct BitReader<'a> {
        bytes: &'a [u8],
        offset: usize,
    }

    impl<'a> BitReader<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, offset: 0 }
        }

        fn read(&mut self, width: u8) -> Option<u16> {
            if self.offset + usize::from(width) > self.bytes.len() * 8 {
                return None;
            }
            let mut code = 0_u16;
            for bit in 0..width {
                let offset = self.offset + usize::from(bit);
                let value = (self.bytes[offset / 8] >> (offset % 8)) & 1;
                code |= u16::from(value) << bit;
            }
            self.offset += usize::from(width);
            Some(code)
        }
    }
}
