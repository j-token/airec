//! H.264 MP4 decoding through Windows Media Foundation's SourceReader.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use airec_core::AirecError;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown,
    MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{Interface, PCWSTR};

use crate::convert::{GifAssembler, SourceSample, decode_error, dimension_error};

pub(crate) const COMPONENT: &str = "media_foundation_decoder";

/// The visible part of a decoded frame. H.264 frames are coded in 16-pixel macroblocks, and
/// Media Foundation returns the padded frame with the visible area described separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DisplayArea {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn crop_bgra(bgra: &[u8], frame_width: u32, display: DisplayArea) -> Vec<u8> {
    let frame_row_bytes = frame_width as usize * 4;
    let visible_row_bytes = display.width as usize * 4;
    let left_bytes = display.x as usize * 4;
    bgra.chunks_exact(frame_row_bytes)
        .skip(display.y as usize)
        .take(display.height as usize)
        .flat_map(|row| &row[left_bytes..left_bytes + visible_row_bytes])
        .copied()
        .collect()
}

pub(crate) fn decode(input: &Path, gif: &mut GifAssembler) -> Result<(), AirecError> {
    let _com = ComApartment::initialize(input)?;
    let _media_foundation = MediaFoundationSession::start(input)?;

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
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
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
            layout = current_layout(input, &reader)?;
        }

        if let Some(sample) = sample {
            let duration_hns = unsafe { sample.GetSampleDuration() }.unwrap_or(0).max(0) as u64;
            gif.push_sample(
                SourceSample {
                    timestamp_hns: timestamp,
                    duration_hns,
                    width: layout.display.width,
                    height: layout.display.height,
                },
                || {
                    let bgra = copy_bgra_sample(input, &sample, layout)?;
                    Ok(crop_bgra(&bgra, layout.width, layout.display))
                },
            )?;
        }

        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Ok(());
        }
    }
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
    display: DisplayArea,
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
    let full_frame = DisplayArea {
        x: 0,
        y: 0,
        width,
        height,
    };
    Ok(FrameLayout {
        width,
        height,
        stride,
        display: display_aperture(&media_type, width, height).unwrap_or(full_frame),
    })
}

fn display_aperture(media_type: &IMFMediaType, width: u32, height: u32) -> Option<DisplayArea> {
    // MFVideoArea: OffsetX and OffsetY are MFOffset { fract: u16, value: i16 }, followed by
    // SIZE { cx: i32, cy: i32 }. Fractional offsets never occur in decoded H.264 frames.
    let mut area = [0_u8; 16];
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None) }.ok()?;
    let x = u32::try_from(i16::from_le_bytes([area[2], area[3]])).ok()?;
    let y = u32::try_from(i16::from_le_bytes([area[6], area[7]])).ok()?;
    let visible_width =
        u32::try_from(i32::from_le_bytes([area[8], area[9], area[10], area[11]])).ok()?;
    let visible_height =
        u32::try_from(i32::from_le_bytes([area[12], area[13], area[14], area[15]])).ok()?;
    let fits_frame = visible_width != 0
        && visible_height != 0
        && x.checked_add(visible_width)? <= width
        && y.checked_add(visible_height)? <= height;
    fits_frame.then_some(DisplayArea {
        x,
        y,
        width: visible_width,
        height: visible_height,
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

fn mf_error(input: &Path, operation: &'static str, error: windows::core::Error) -> AirecError {
    decode_error(input, operation, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_keeps_only_the_display_aperture() {
        let frame = (0..12_u8)
            .flat_map(|pixel| [pixel, 0, 0, 255])
            .collect::<Vec<_>>();
        let display = DisplayArea {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };

        let visible = crop_bgra(&frame, 4, display);

        assert_eq!(
            visible,
            [5, 0, 0, 255, 6, 0, 0, 255, 9, 0, 0, 255, 10, 0, 0, 255]
        );
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
}
