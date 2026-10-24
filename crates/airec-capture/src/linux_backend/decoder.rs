//! H.264 MP4 decoding with the bundled OpenH264 decoder.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use airec_core::AirecError;
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;

use super::mp4::{SampleLocation, VideoTrack, read_video_track};
use crate::convert::{GifAssembler, SourceSample, decode_error};
use crate::yuv::{PlanarImage, bgra_from_bt709};

pub(crate) const COMPONENT: &str = "openh264_decoder";
const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];
const HNS_PER_SECOND: i128 = 10_000_000;

pub(crate) fn decode(input: &Path, gif: &mut GifAssembler) -> Result<(), AirecError> {
    let mut file = File::open(input).map_err(|error| {
        decode_error(
            input,
            "open input",
            format!("cannot open input MP4: {error}"),
        )
    })?;
    let track = read_video_track(&mut file)
        .map_err(|error| decode_error(input, "read MP4 structure", error.to_string()))?;
    let mut decoder = Decoder::with_api_config(
        OpenH264API::from_source(),
        DecoderConfig::new().flush_after_decode(Flush::NoFlush),
    )
    .map_err(|error| decode_error(input, "start OpenH264 decoder", error.to_string()))?;
    let decoder_error =
        |error: openh264::Error| decode_error(input, "decode video sample", error.to_string());

    let parameter_sets = track
        .parameter_sets
        .iter()
        .flat_map(|set| ANNEX_B_START_CODE.iter().chain(set))
        .copied()
        .collect::<Vec<_>>();
    decoder.decode(&parameter_sets).map_err(decoder_error)?;

    let mut output = OutputTimeline::new(&track);
    let mut encoded = Vec::new();
    for sample in &track.samples {
        read_sample(&mut file, sample, &mut encoded)
            .map_err(|error| decode_error(input, "read video sample", error.to_string()))?;
        let annex_b =
            annex_b_from_length_prefixed(&encoded, track.nal_length_size).ok_or_else(|| {
                decode_error(
                    input,
                    "read video sample",
                    "a sample's NAL unit lengths exceed the sample",
                )
            })?;
        if let Some(picture) = decoder.decode(&annex_b).map_err(decoder_error)? {
            push_picture(gif, &mut output, &picture)?;
        }
    }
    for picture in decoder.flush_remaining().map_err(decoder_error)? {
        push_picture(gif, &mut output, &picture)?;
    }
    Ok(())
}

fn read_sample(
    file: &mut File,
    sample: &SampleLocation,
    buffer: &mut Vec<u8>,
) -> std::io::Result<()> {
    buffer.resize(sample.size as usize, 0);
    file.seek(SeekFrom::Start(sample.offset))?;
    file.read_exact(buffer)
}

fn annex_b_from_length_prefixed(sample: &[u8], length_size: usize) -> Option<Vec<u8>> {
    let mut annex_b = Vec::with_capacity(sample.len() + 16);
    let mut position = 0;
    while position < sample.len() {
        let length_bytes = sample.get(position..position + length_size)?;
        let length = length_bytes
            .iter()
            .fold(0_usize, |length, &byte| length << 8 | usize::from(byte));
        position += length_size;
        annex_b.extend_from_slice(&ANNEX_B_START_CODE);
        annex_b.extend_from_slice(sample.get(position..position + length)?);
        position += length;
    }
    Some(annex_b)
}

/// Presentation times in display order. The decoder returns pictures in display order, so the
/// n-th picture it returns shows at the n-th smallest presentation time, whatever the decode
/// order was.
struct OutputTimeline {
    times: Vec<(i64, u64)>,
    timescale: u32,
    next: usize,
}

impl OutputTimeline {
    fn new(track: &VideoTrack) -> Self {
        let mut times: Vec<_> = track
            .samples
            .iter()
            .map(|sample| {
                let decode_time = i64::try_from(sample.decode_time).unwrap_or(i64::MAX);
                (
                    decode_time.saturating_add(sample.composition_offset),
                    u64::from(sample.duration),
                )
            })
            .collect();
        times.sort_unstable();
        Self {
            times,
            timescale: track.timescale,
            next: 0,
        }
    }

    /// The next picture's (timestamp, duration) in 100-nanosecond units.
    fn next_hns(&mut self) -> Option<(i64, u64)> {
        let (time, duration) = *self.times.get(self.next)?;
        self.next += 1;
        let to_hns = |value: i128| value * HNS_PER_SECOND / i128::from(self.timescale);
        Some((
            i64::try_from(to_hns(i128::from(time))).unwrap_or(i64::MAX),
            u64::try_from(to_hns(i128::from(duration))).unwrap_or(0),
        ))
    }
}

fn push_picture(
    gif: &mut GifAssembler,
    timeline: &mut OutputTimeline,
    picture: &DecodedYUV<'_>,
) -> Result<(), AirecError> {
    let Some((timestamp_hns, duration_hns)) = timeline.next_hns() else {
        return Ok(());
    };
    let (width, height) = picture.dimensions();
    gif.push_sample(
        SourceSample {
            timestamp_hns,
            duration_hns,
            width: width as u32,
            height: height as u32,
        },
        || {
            Ok(bgra_from_bt709(&PlanarImage {
                y: picture.y(),
                u: picture.u(),
                v: picture.v(),
                strides: picture.strides(),
                width,
                height,
            }))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_sample_becomes_annex_b() {
        let sample = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x06];

        let annex_b = annex_b_from_length_prefixed(&sample, 4).unwrap();

        assert_eq!(annex_b, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x06]);
    }

    #[test]
    fn nal_length_past_the_sample_end_is_rejected() {
        assert_eq!(annex_b_from_length_prefixed(&[0, 0, 0, 9, 0x65], 4), None);
    }

    #[test]
    fn reordered_samples_are_presented_in_display_order() {
        let sample = |decode_time, composition_offset| SampleLocation {
            offset: 0,
            size: 0,
            decode_time,
            duration: 3_000,
            composition_offset,
        };
        let track = VideoTrack {
            timescale: 90_000,
            nal_length_size: 4,
            parameter_sets: Vec::new(),
            samples: vec![sample(0, 3_000), sample(3_000, 6_000), sample(6_000, 0)],
        };
        let mut timeline = OutputTimeline::new(&track);

        let times: Vec<_> = std::iter::from_fn(|| timeline.next_hns()).collect();

        assert_eq!(
            times,
            [(333_333, 333_333), (666_666, 333_333), (1_000_000, 333_333)]
        );
    }
}
