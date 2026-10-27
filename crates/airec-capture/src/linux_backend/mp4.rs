//! Minimal ISO BMFF support for one H.264 video track and an optional Opus audio track: a
//! fragmented MP4 writer, so a killed recorder leaves a playable file up to its last fragment,
//! and a reader for the video track of both fragmented and regular files.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Media timescale of written tracks, the conventional 90 kHz video clock.
pub(crate) const TIMESCALE: u32 = 90_000;
const TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;
/// Opus always decodes at 48 kHz, so its track counts time in samples.
pub(crate) const AUDIO_TIMESCALE: u32 = 48_000;
const SYNC_SAMPLE_FLAGS: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE_FLAGS: u32 = 0x0101_0000;
const HIGH_PROFILES: [u8; 4] = [100, 110, 122, 144];

pub(crate) struct Sample {
    /// NAL units, each prefixed with its 4-byte big-endian length.
    pub data: Vec<u8>,
    pub decode_time: u64,
    pub sync: bool,
}

pub(crate) struct AudioSample {
    /// One Opus packet.
    pub data: Vec<u8>,
    /// In [`AUDIO_TIMESCALE`] units.
    pub duration: u32,
}

/// Stereo Opus track parameters.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpusTrack {
    /// Encoder lookahead in 48 kHz samples, which decoders discard from the start.
    pub pre_skip: u16,
}

pub(crate) struct FragmentedMp4Writer<W: Write> {
    output: W,
    sequence_number: u32,
    pending: Vec<Sample>,
    pending_audio: Vec<AudioSample>,
    audio_decode_time: u64,
}

impl<W: Write> FragmentedMp4Writer<W> {
    /// Writes the initialization segment. `sps` and `pps` are NAL units without start codes.
    pub(crate) fn new(
        mut output: W,
        width: u32,
        height: u32,
        sps: &[u8],
        pps: &[u8],
        audio: Option<OpusTrack>,
    ) -> io::Result<Self> {
        output.write_all(&file_type())?;
        output.write_all(&movie(width, height, sps, pps, audio)?)?;
        output.flush()?;
        Ok(Self {
            output,
            sequence_number: 0,
            pending: Vec::new(),
            pending_audio: Vec::new(),
            audio_decode_time: 0,
        })
    }

    /// Queues a sample. A fragment is written when the next sync sample arrives, because each
    /// sample's duration is only known once the following sample's time is.
    pub(crate) fn push(&mut self, sample: Sample) -> io::Result<()> {
        if sample.sync && !self.pending.is_empty() {
            self.write_fragment(sample.decode_time)?;
        }
        self.pending.push(sample);
        Ok(())
    }

    /// Queues an audio sample that follows the previous one without a gap. It is written with
    /// the next video fragment.
    pub(crate) fn push_audio(&mut self, sample: AudioSample) {
        self.pending_audio.push(sample);
    }

    pub(crate) fn finish(mut self, last_sample_duration: u32) -> io::Result<W> {
        let end = self
            .pending
            .last()
            .map_or(0, |last| last.decode_time + u64::from(last_sample_duration));
        self.write_fragment(end)?;
        self.output.flush()?;
        Ok(self.output)
    }

    fn write_fragment(&mut self, end_time: u64) -> io::Result<()> {
        let samples = std::mem::take(&mut self.pending);
        let audio = std::mem::take(&mut self.pending_audio);
        let mut runs = Vec::new();
        if let Some(first) = samples.first() {
            let next_times = samples
                .iter()
                .skip(1)
                .map(|sample| sample.decode_time)
                .chain(std::iter::once(end_time));
            let entries = samples
                .iter()
                .zip(next_times)
                .map(|(sample, next)| RunEntry {
                    duration: u32::try_from(next.saturating_sub(sample.decode_time))
                        .unwrap_or(u32::MAX),
                    size: sample.data.len(),
                    flags: Some(if sample.sync {
                        SYNC_SAMPLE_FLAGS
                    } else {
                        NON_SYNC_SAMPLE_FLAGS
                    }),
                })
                .collect();
            runs.push(TrackRun {
                track_id: TRACK_ID,
                decode_time: first.decode_time,
                entries,
            });
        }
        if !audio.is_empty() {
            runs.push(TrackRun {
                track_id: AUDIO_TRACK_ID,
                decode_time: self.audio_decode_time,
                entries: audio
                    .iter()
                    .map(|sample| RunEntry {
                        duration: sample.duration,
                        size: sample.data.len(),
                        flags: None,
                    })
                    .collect(),
            });
            self.audio_decode_time += audio
                .iter()
                .map(|sample| u64::from(sample.duration))
                .sum::<u64>();
        }
        if runs.is_empty() {
            return Ok(());
        }
        self.sequence_number += 1;
        let payload_len = samples
            .iter()
            .map(|sample| sample.data.len())
            .sum::<usize>()
            + audio.iter().map(|sample| sample.data.len()).sum::<usize>();

        let fragment = movie_fragment(self.sequence_number, &runs)?;
        self.output.write_all(&fragment)?;
        self.output.write_all(&box_header(b"mdat", payload_len)?)?;
        for sample in &samples {
            self.output.write_all(&sample.data)?;
        }
        for sample in &audio {
            self.output.write_all(&sample.data)?;
        }
        self.output.flush()
    }
}

/// One track's samples in a fragment, laid out in `mdat` in the order of the runs.
struct TrackRun {
    track_id: u32,
    decode_time: u64,
    entries: Vec<RunEntry>,
}

struct RunEntry {
    duration: u32,
    size: usize,
    /// Omitted for audio, whose samples are all sync samples by the `trex` default.
    flags: Option<u32>,
}

fn box_header(kind: &[u8; 4], body_len: usize) -> io::Result<[u8; 8]> {
    let size = u32::try_from(body_len + 8)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "MP4 box exceeds 4 GiB"))?;
    let mut header = [0_u8; 8];
    header[..4].copy_from_slice(&size.to_be_bytes());
    header[4..].copy_from_slice(kind);
    Ok(header)
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = box_header(kind, body.len())?.to_vec();
    output.extend_from_slice(body);
    Ok(output)
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> io::Result<Vec<u8>> {
    let mut content = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
    content.extend_from_slice(body);
    mp4_box(kind, &content)
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    UNITY_MATRIX
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn file_type() -> Vec<u8> {
    let body = concat(&[
        b"isom".to_vec(),
        0x200_u32.to_be_bytes().to_vec(),
        b"isomiso6avc1mp41".to_vec(),
    ]);
    mp4_box(b"ftyp", &body).expect("ftyp is small")
}

fn movie(
    width: u32,
    height: u32,
    sps: &[u8],
    pps: &[u8],
    audio: Option<OpusTrack>,
) -> io::Result<Vec<u8>> {
    let next_track_id = if audio.is_some() {
        AUDIO_TRACK_ID + 1
    } else {
        TRACK_ID + 1
    };
    let movie_header = full_box(
        b"mvhd",
        0,
        0,
        &concat(&[
            vec![0; 8],
            1_000_u32.to_be_bytes().to_vec(),
            vec![0; 4],
            0x0001_0000_u32.to_be_bytes().to_vec(),
            0x0100_u16.to_be_bytes().to_vec(),
            vec![0; 10],
            matrix(),
            vec![0; 24],
            next_track_id.to_be_bytes().to_vec(),
        ]),
    )?;
    let track_header = full_box(
        b"tkhd",
        0,
        0x3,
        &concat(&[
            vec![0; 8],
            TRACK_ID.to_be_bytes().to_vec(),
            vec![0; 4],
            vec![0; 4],
            vec![0; 8],
            vec![0; 8],
            matrix(),
            (width << 16).to_be_bytes().to_vec(),
            (height << 16).to_be_bytes().to_vec(),
        ]),
    )?;
    let media_header = full_box(
        b"mdhd",
        0,
        0,
        &concat(&[
            vec![0; 8],
            TIMESCALE.to_be_bytes().to_vec(),
            vec![0; 4],
            // Language "und", packed as three 5-bit letters offset by 0x60.
            0x55c4_u16.to_be_bytes().to_vec(),
            vec![0; 2],
        ]),
    )?;
    let handler = full_box(
        b"hdlr",
        0,
        0,
        &concat(&[
            vec![0; 4],
            b"vide".to_vec(),
            vec![0; 12],
            b"VideoHandler\0".to_vec(),
        ]),
    )?;
    let video_media_header = full_box(b"vmhd", 0, 1, &[0; 8])?;
    let media_information = mp4_box(
        b"minf",
        &concat(&[
            video_media_header,
            data_information()?,
            sample_table(sample_entry(width, height, sps, pps)?)?,
        ]),
    )?;
    let media = mp4_box(
        b"mdia",
        &concat(&[media_header, handler, media_information]),
    )?;
    let mut tracks = vec![mp4_box(b"trak", &concat(&[track_header, media]))?];
    let mut extends = vec![track_extends(TRACK_ID)?];
    if let Some(audio) = audio {
        tracks.push(opus_track(audio)?);
        extends.push(track_extends(AUDIO_TRACK_ID)?);
    }
    let movie_extends = mp4_box(b"mvex", &concat(&extends))?;
    mp4_box(
        b"moov",
        &concat(&[movie_header, concat(&tracks), movie_extends]),
    )
}

fn data_information() -> io::Result<Vec<u8>> {
    mp4_box(
        b"dinf",
        &full_box(
            b"dref",
            0,
            0,
            &concat(&[1_u32.to_be_bytes().to_vec(), full_box(b"url ", 0, 1, &[])?]),
        )?,
    )
}

/// A sample table with one description and no samples, which all live in fragments.
fn sample_table(description: Vec<u8>) -> io::Result<Vec<u8>> {
    mp4_box(
        b"stbl",
        &concat(&[
            full_box(
                b"stsd",
                0,
                0,
                &concat(&[1_u32.to_be_bytes().to_vec(), description]),
            )?,
            full_box(b"stts", 0, 0, &[0; 4])?,
            full_box(b"stsc", 0, 0, &[0; 4])?,
            full_box(b"stsz", 0, 0, &[0; 8])?,
            full_box(b"stco", 0, 0, &[0; 4])?,
        ]),
    )
}

/// Default sample flags of zero mark every fragment sample as a sync sample unless its run
/// says otherwise.
fn track_extends(track_id: u32) -> io::Result<Vec<u8>> {
    full_box(
        b"trex",
        0,
        0,
        &concat(&[
            track_id.to_be_bytes().to_vec(),
            1_u32.to_be_bytes().to_vec(),
            vec![0; 12],
        ]),
    )
}

fn opus_track(audio: OpusTrack) -> io::Result<Vec<u8>> {
    let track_header = full_box(
        b"tkhd",
        0,
        0x3,
        &concat(&[
            vec![0; 8],
            AUDIO_TRACK_ID.to_be_bytes().to_vec(),
            vec![0; 4],
            vec![0; 4],
            vec![0; 8],
            // Layer, alternate group, full volume, and reserved.
            vec![0, 0, 0, 1, 0x01, 0x00, 0, 0],
            matrix(),
            vec![0; 8],
        ]),
    )?;
    let media_header = full_box(
        b"mdhd",
        0,
        0,
        &concat(&[
            vec![0; 8],
            AUDIO_TIMESCALE.to_be_bytes().to_vec(),
            vec![0; 4],
            0x55c4_u16.to_be_bytes().to_vec(),
            vec![0; 2],
        ]),
    )?;
    let handler = full_box(
        b"hdlr",
        0,
        0,
        &concat(&[
            vec![0; 4],
            b"soun".to_vec(),
            vec![0; 12],
            b"SoundHandler\0".to_vec(),
        ]),
    )?;
    let sound_media_header = full_box(b"smhd", 0, 0, &[0; 4])?;
    let media_information = mp4_box(
        b"minf",
        &concat(&[
            sound_media_header,
            data_information()?,
            sample_table(opus_sample_entry(audio)?)?,
        ]),
    )?;
    let media = mp4_box(
        b"mdia",
        &concat(&[media_header, handler, media_information]),
    )?;
    mp4_box(b"trak", &concat(&[track_header, media]))
}

/// The `Opus` sample entry and `dOps` box from the Opus in ISO BMFF encapsulation specification.
fn opus_sample_entry(audio: OpusTrack) -> io::Result<Vec<u8>> {
    const CHANNELS: u8 = 2;
    let specific = mp4_box(
        b"dOps",
        &concat(&[
            vec![0, CHANNELS],
            audio.pre_skip.to_be_bytes().to_vec(),
            AUDIO_TIMESCALE.to_be_bytes().to_vec(),
            // Output gain and channel mapping family 0, which is mono or stereo.
            vec![0, 0, 0],
        ]),
    )?;
    mp4_box(
        b"Opus",
        &concat(&[
            vec![0; 6],
            1_u16.to_be_bytes().to_vec(),
            vec![0; 8],
            u16::from(CHANNELS).to_be_bytes().to_vec(),
            16_u16.to_be_bytes().to_vec(),
            vec![0; 4],
            (AUDIO_TIMESCALE << 16).to_be_bytes().to_vec(),
            specific,
        ]),
    )
}

fn sample_entry(width: u32, height: u32, sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    let [_, profile, compatibility, level, ..] = *sps else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "H.264 sequence parameter set is too short",
        ));
    };
    let parameter_set_len = |set: &[u8]| {
        u16::try_from(set.len())
            .map(u16::to_be_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "parameter set too large"))
    };
    let mut configuration = vec![1, profile, compatibility, level, 0xff, 0xe1];
    configuration.extend_from_slice(&parameter_set_len(sps)?);
    configuration.extend_from_slice(sps);
    configuration.push(1);
    configuration.extend_from_slice(&parameter_set_len(pps)?);
    configuration.extend_from_slice(pps);
    if HIGH_PROFILES.contains(&profile) {
        // 4:2:0, 8-bit luma and chroma, no SPS extensions.
        configuration.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    // nclx BT.709 primaries, transfer, and matrix with limited range, matching the samples.
    let colour = mp4_box(
        b"colr",
        &concat(&[
            b"nclx".to_vec(),
            1_u16.to_be_bytes().to_vec(),
            1_u16.to_be_bytes().to_vec(),
            1_u16.to_be_bytes().to_vec(),
            vec![0],
        ]),
    )?;
    let dimension = |value: u32| {
        u16::try_from(value)
            .map(u16::to_be_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large for MP4"))
    };
    let mut compressor_name = vec![0_u8; 32];
    compressor_name[0] = 8;
    compressor_name[1..9].copy_from_slice(b"OpenH264");
    mp4_box(
        b"avc1",
        &concat(&[
            vec![0; 6],
            1_u16.to_be_bytes().to_vec(),
            vec![0; 16],
            dimension(width)?.to_vec(),
            dimension(height)?.to_vec(),
            0x0048_0000_u32.to_be_bytes().to_vec(),
            0x0048_0000_u32.to_be_bytes().to_vec(),
            vec![0; 4],
            1_u16.to_be_bytes().to_vec(),
            compressor_name,
            0x0018_u16.to_be_bytes().to_vec(),
            0xffff_u16.to_be_bytes().to_vec(),
            mp4_box(b"avcC", &configuration)?,
            colour,
        ]),
    )
}

fn movie_fragment(sequence_number: u32, runs: &[TrackRun]) -> io::Result<Vec<u8>> {
    let header = full_box(b"mfhd", 0, 0, &sequence_number.to_be_bytes())?;
    let build = |data_offsets: &[usize]| -> io::Result<Vec<u8>> {
        let mut parts = vec![header.clone()];
        for (run, &data_offset) in runs.iter().zip(data_offsets) {
            parts.push(track_fragment(run, data_offset)?);
        }
        mp4_box(b"moof", &concat(&parts))
    };
    // A data offset counts from the start of moof to the run's first byte inside mdat, so it
    // depends on the size of the moof that contains it; the size does not change with it.
    let moof_len = build(&vec![0; runs.len()])?.len();
    let mut data_offsets = Vec::with_capacity(runs.len());
    let mut data_offset = moof_len + 8;
    for run in runs {
        data_offsets.push(data_offset);
        data_offset += run.entries.iter().map(|entry| entry.size).sum::<usize>();
    }
    build(&data_offsets)
}

fn track_fragment(run: &TrackRun, data_offset: usize) -> io::Result<Vec<u8>> {
    const DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;
    const DATA_OFFSET: u32 = 0x1;
    const SAMPLE_DURATION: u32 = 0x100;
    const SAMPLE_SIZE: u32 = 0x200;
    const SAMPLE_FLAGS: u32 = 0x400;

    let too_large =
        |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{what} too large"));
    let has_flags = run.entries.iter().any(|entry| entry.flags.is_some());
    let mut body = Vec::with_capacity(8 + run.entries.len() * 12);
    body.extend_from_slice(
        &u32::try_from(run.entries.len())
            .map_err(|_| too_large("sample count"))?
            .to_be_bytes(),
    );
    body.extend_from_slice(
        &i32::try_from(data_offset)
            .map_err(|_| too_large("fragment header"))?
            .to_be_bytes(),
    );
    for entry in &run.entries {
        let size = u32::try_from(entry.size).map_err(|_| too_large("sample"))?;
        body.extend_from_slice(&entry.duration.to_be_bytes());
        body.extend_from_slice(&size.to_be_bytes());
        if let Some(flags) = entry.flags {
            body.extend_from_slice(&flags.to_be_bytes());
        }
    }
    let run_flags =
        DATA_OFFSET | SAMPLE_DURATION | SAMPLE_SIZE | if has_flags { SAMPLE_FLAGS } else { 0 };
    mp4_box(
        b"traf",
        &concat(&[
            full_box(
                b"tfhd",
                0,
                DEFAULT_BASE_IS_MOOF,
                &run.track_id.to_be_bytes(),
            )?,
            full_box(b"tfdt", 1, 0, &run.decode_time.to_be_bytes())?,
            full_box(b"trun", 0, run_flags, &body)?,
        ]),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SampleLocation {
    pub offset: u64,
    pub size: u32,
    pub decode_time: u64,
    pub duration: u32,
    pub composition_offset: i64,
}

#[derive(Debug)]
pub(crate) struct VideoTrack {
    pub timescale: u32,
    pub nal_length_size: usize,
    /// SPS and PPS NAL units from the avcC record.
    pub parameter_sets: Vec<Vec<u8>>,
    pub samples: Vec<SampleLocation>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Locates the samples of the first H.264 video track in a regular or fragmented MP4 without
/// loading media data.
pub(crate) fn read_video_track(file: &mut File) -> io::Result<VideoTrack> {
    let file_len = file.metadata()?.len();
    let mut position = 0_u64;
    let mut track: Option<TrackInfo> = None;
    let mut fragment_samples = Vec::new();
    let mut next_fragment_time = 0_u64;
    while position + 8 <= file_len {
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().expect("4 bytes")));
        let kind: [u8; 4] = header[4..].try_into().expect("4 bytes");
        let mut header_len = 8;
        if size == 1 {
            let mut large = [0_u8; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header_len = 16;
        } else if size == 0 {
            size = file_len - position;
        }
        if size < header_len || position + size > file_len {
            // A recording cut off mid-write ends in a partial box; everything before it is valid.
            break;
        }
        match &kind {
            b"moov" | b"moof" => {
                let mut body = vec![
                    0_u8;
                    usize::try_from(size - header_len)
                        .map_err(|_| invalid("box too large"))?
                ];
                file.read_exact(&mut body)?;
                if &kind == b"moov" {
                    track = Some(parse_movie(&body)?);
                } else if let Some(track) = &track {
                    parse_fragment(
                        &body,
                        position,
                        track,
                        &mut next_fragment_time,
                        &mut fragment_samples,
                    )?;
                }
            }
            _ => {}
        }
        position += size;
    }
    let track = track.ok_or_else(|| invalid("MP4 has no H.264 video track"))?;
    let mut samples = track.table_samples;
    samples.extend(fragment_samples);
    // A recorder killed between a fragment's moof and the end of its mdat leaves samples that
    // the file does not contain.
    samples.retain(|sample| sample.offset + u64::from(sample.size) <= file_len);
    Ok(VideoTrack {
        timescale: track.timescale,
        nal_length_size: track.nal_length_size,
        parameter_sets: track.parameter_sets,
        samples,
    })
}

struct TrackInfo {
    track_id: u32,
    timescale: u32,
    nal_length_size: usize,
    parameter_sets: Vec<Vec<u8>>,
    table_samples: Vec<SampleLocation>,
    default_duration: u32,
    default_size: u32,
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bytes(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| invalid("MP4 box is truncated"))?;
        let bytes = &self.data[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.bytes(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.bytes(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.bytes(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn skip(&mut self, len: usize) -> io::Result<()> {
        self.bytes(len).map(drop)
    }

    fn version_and_flags(&mut self) -> io::Result<(u8, u32)> {
        let value = self.u32()?;
        Ok(((value >> 24) as u8, value & 0x00ff_ffff))
    }
}

/// Child boxes of a container body as (type, body).
fn children(data: &[u8]) -> io::Result<Vec<([u8; 4], &[u8])>> {
    let mut reader = Reader::new(data);
    let mut boxes = Vec::new();
    while reader.position + 8 <= data.len() {
        let size = reader.u32()? as usize;
        let kind: [u8; 4] = reader.bytes(4)?.try_into().expect("4 bytes");
        let body_len = match size {
            0 => data.len() - reader.position,
            1 => {
                let large = usize::try_from(reader.u64()?).map_err(|_| invalid("box too large"))?;
                large
                    .checked_sub(16)
                    .ok_or_else(|| invalid("invalid box size"))?
            }
            _ => size
                .checked_sub(8)
                .ok_or_else(|| invalid("invalid box size"))?,
        };
        boxes.push((kind, reader.bytes(body_len)?));
    }
    Ok(boxes)
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> io::Result<Option<&'a [u8]>> {
    Ok(children(data)?
        .into_iter()
        .find(|(child_kind, _)| child_kind == kind)
        .map(|(_, body)| body))
}

fn required<'a>(data: &'a [u8], kind: &[u8; 4]) -> io::Result<&'a [u8]> {
    child(data, kind)?.ok_or_else(|| {
        invalid(format!(
            "MP4 is missing the {} box",
            String::from_utf8_lossy(kind)
        ))
    })
}

fn parse_movie(moov: &[u8]) -> io::Result<TrackInfo> {
    let extends = child(moov, b"mvex")?;
    for (kind, trak) in children(moov)? {
        if &kind != b"trak" {
            continue;
        }
        let media = required(trak, b"mdia")?;
        let handler = required(media, b"hdlr")?;
        if handler.get(8..12) != Some(b"vide") {
            continue;
        }
        let mut track_header = Reader::new(required(trak, b"tkhd")?);
        let (version, _) = track_header.version_and_flags()?;
        track_header.skip(if version == 1 { 16 } else { 8 })?;
        let track_id = track_header.u32()?;

        let mut media_header = Reader::new(required(media, b"mdhd")?);
        let (version, _) = media_header.version_and_flags()?;
        media_header.skip(if version == 1 { 16 } else { 8 })?;
        let timescale = media_header.u32()?;
        if timescale == 0 {
            return Err(invalid("MP4 video track has a zero timescale"));
        }

        let table = required(required(media, b"minf")?, b"stbl")?;
        let (nal_length_size, parameter_sets) = avc_configuration(required(table, b"stsd")?)?;
        let (default_duration, default_size) = match extends {
            Some(extends) => track_defaults(extends, track_id)?,
            None => (0, 0),
        };
        return Ok(TrackInfo {
            track_id,
            timescale,
            nal_length_size,
            parameter_sets,
            table_samples: table_samples(table)?,
            default_duration,
            default_size,
        });
    }
    Err(invalid("MP4 has no video track"))
}

fn avc_configuration(sample_descriptions: &[u8]) -> io::Result<(usize, Vec<Vec<u8>>)> {
    let mut reader = Reader::new(sample_descriptions);
    reader.version_and_flags()?;
    reader.u32()?;
    let entries = children(&sample_descriptions[reader.position..])?;
    let (kind, entry) = entries
        .first()
        .ok_or_else(|| invalid("MP4 video track has no sample description"))?;
    if kind != b"avc1" && kind != b"avc3" {
        return Err(invalid(format!(
            "unsupported video codec {}; only H.264 is supported",
            String::from_utf8_lossy(kind)
        )));
    }
    // VisualSampleEntry fields before its child boxes take 78 bytes.
    let configuration = entry
        .get(78..)
        .ok_or_else(|| invalid("H.264 sample entry is truncated"))?;
    let mut reader = Reader::new(required(configuration, b"avcC")?);
    reader.skip(4)?;
    let nal_length_size = usize::from(reader.u8()? & 0x3) + 1;
    let mut parameter_sets = Vec::new();
    let sps_count = reader.u8()? & 0x1f;
    for _ in 0..sps_count {
        let len = usize::from(reader.u16()?);
        parameter_sets.push(reader.bytes(len)?.to_vec());
    }
    let pps_count = reader.u8()?;
    for _ in 0..pps_count {
        let len = usize::from(reader.u16()?);
        parameter_sets.push(reader.bytes(len)?.to_vec());
    }
    Ok((nal_length_size, parameter_sets))
}

fn track_defaults(extends: &[u8], track_id: u32) -> io::Result<(u32, u32)> {
    for (kind, body) in children(extends)? {
        if &kind != b"trex" {
            continue;
        }
        let mut reader = Reader::new(body);
        reader.version_and_flags()?;
        if reader.u32()? != track_id {
            continue;
        }
        reader.u32()?;
        let duration = reader.u32()?;
        let size = reader.u32()?;
        return Ok((duration, size));
    }
    Ok((0, 0))
}

fn table_samples(table: &[u8]) -> io::Result<Vec<SampleLocation>> {
    let Some(sizes) = child(table, b"stsz")? else {
        return Ok(Vec::new());
    };
    let mut reader = Reader::new(sizes);
    reader.version_and_flags()?;
    let uniform_size = reader.u32()?;
    let count = reader.u32()? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let sample_sizes = (0..count)
        .map(|_| {
            if uniform_size == 0 {
                reader.u32()
            } else {
                Ok(uniform_size)
            }
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut reader = Reader::new(required(table, b"stts")?);
    reader.version_and_flags()?;
    let mut durations = Vec::with_capacity(count);
    for _ in 0..reader.u32()? {
        let run = reader.u32()? as usize;
        let delta = reader.u32()?;
        durations.extend(std::iter::repeat_n(delta, run));
    }

    let mut composition_offsets = vec![0_i64; count];
    if let Some(offsets) = child(table, b"ctts")? {
        let mut reader = Reader::new(offsets);
        let (version, _) = reader.version_and_flags()?;
        let mut index = 0;
        for _ in 0..reader.u32()? {
            let run = reader.u32()? as usize;
            let raw = reader.u32()?;
            let offset = if version == 1 {
                i64::from(raw as i32)
            } else {
                i64::from(raw)
            };
            for slot in composition_offsets.iter_mut().skip(index).take(run) {
                *slot = offset;
            }
            index += run;
        }
    }

    let chunk_offsets = if let Some(offsets) = child(table, b"stco")? {
        let mut reader = Reader::new(offsets);
        reader.version_and_flags()?;
        (0..reader.u32()?)
            .map(|_| reader.u32().map(u64::from))
            .collect::<io::Result<Vec<_>>>()?
    } else {
        let mut reader = Reader::new(required(table, b"co64")?);
        reader.version_and_flags()?;
        (0..reader.u32()?)
            .map(|_| reader.u64())
            .collect::<io::Result<Vec<_>>>()?
    };

    let mut reader = Reader::new(required(table, b"stsc")?);
    reader.version_and_flags()?;
    let chunk_runs = (0..reader.u32()?)
        .map(|_| {
            let first_chunk = reader.u32()? as usize;
            let samples_per_chunk = reader.u32()? as usize;
            reader.u32()?;
            Ok((first_chunk, samples_per_chunk))
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut samples = Vec::with_capacity(count);
    let mut decode_time = 0_u64;
    for (chunk_index, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let chunk_number = chunk_index + 1;
        let samples_per_chunk = chunk_runs
            .iter()
            .rev()
            .find(|(first_chunk, _)| *first_chunk <= chunk_number)
            .map_or(0, |(_, samples)| *samples);
        let mut offset = chunk_offset;
        for _ in 0..samples_per_chunk {
            let index = samples.len();
            if index == count {
                break;
            }
            let duration = durations.get(index).copied().unwrap_or(0);
            samples.push(SampleLocation {
                offset,
                size: sample_sizes[index],
                decode_time,
                duration,
                composition_offset: composition_offsets[index],
            });
            offset += u64::from(sample_sizes[index]);
            decode_time += u64::from(duration);
        }
    }
    Ok(samples)
}

fn parse_fragment(
    moof: &[u8],
    moof_offset: u64,
    track: &TrackInfo,
    next_decode_time: &mut u64,
    samples: &mut Vec<SampleLocation>,
) -> io::Result<()> {
    const BASE_DATA_OFFSET: u32 = 0x1;
    const SAMPLE_DESCRIPTION_INDEX: u32 = 0x2;
    const DEFAULT_DURATION: u32 = 0x8;
    const DEFAULT_SIZE: u32 = 0x10;
    const DEFAULT_FLAGS: u32 = 0x20;
    const DATA_OFFSET: u32 = 0x1;
    const FIRST_SAMPLE_FLAGS: u32 = 0x4;
    const SAMPLE_DURATION: u32 = 0x100;
    const SAMPLE_SIZE: u32 = 0x200;
    const SAMPLE_FLAGS: u32 = 0x400;
    const COMPOSITION_OFFSET: u32 = 0x800;

    for (kind, traf) in children(moof)? {
        if &kind != b"traf" {
            continue;
        }
        let mut header = Reader::new(required(traf, b"tfhd")?);
        let (_, flags) = header.version_and_flags()?;
        if header.u32()? != track.track_id {
            continue;
        }
        // Without an explicit base, offsets count from the start of the enclosing moof, which
        // is also what default-base-is-moof states.
        let base_offset = if flags & BASE_DATA_OFFSET != 0 {
            header.u64()?
        } else {
            moof_offset
        };
        if flags & SAMPLE_DESCRIPTION_INDEX != 0 {
            header.u32()?;
        }
        let default_duration = if flags & DEFAULT_DURATION != 0 {
            header.u32()?
        } else {
            track.default_duration
        };
        let default_size = if flags & DEFAULT_SIZE != 0 {
            header.u32()?
        } else {
            track.default_size
        };
        if flags & DEFAULT_FLAGS != 0 {
            header.u32()?;
        }

        if let Some(decode_time) = child(traf, b"tfdt")? {
            let mut reader = Reader::new(decode_time);
            let (version, _) = reader.version_and_flags()?;
            *next_decode_time = if version == 1 {
                reader.u64()?
            } else {
                u64::from(reader.u32()?)
            };
        }

        let mut data_position = base_offset;
        for (kind, run) in children(traf)? {
            if &kind != b"trun" {
                continue;
            }
            let mut reader = Reader::new(run);
            let (version, flags) = reader.version_and_flags()?;
            let count = reader.u32()?;
            if flags & DATA_OFFSET != 0 {
                let offset = i64::from(reader.u32()? as i32);
                data_position = base_offset
                    .checked_add_signed(offset)
                    .ok_or_else(|| invalid("fragment data offset is out of range"))?;
            }
            if flags & FIRST_SAMPLE_FLAGS != 0 {
                reader.u32()?;
            }
            for _ in 0..count {
                let duration = if flags & SAMPLE_DURATION != 0 {
                    reader.u32()?
                } else {
                    default_duration
                };
                let size = if flags & SAMPLE_SIZE != 0 {
                    reader.u32()?
                } else {
                    default_size
                };
                if flags & SAMPLE_FLAGS != 0 {
                    reader.u32()?;
                }
                let composition_offset = if flags & COMPOSITION_OFFSET != 0 {
                    let raw = reader.u32()?;
                    if version == 1 {
                        i64::from(raw as i32)
                    } else {
                        i64::from(raw)
                    }
                } else {
                    0
                };
                samples.push(SampleLocation {
                    offset: data_position,
                    size,
                    decode_time: *next_decode_time,
                    duration,
                    composition_offset,
                });
                data_position += u64::from(size);
                *next_decode_time += u64::from(duration);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 66, 0xc0, 31];
    const PPS: [u8; 2] = [0x68, 0xce];

    fn test_file(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("airec-mp4-{name}-{}.mp4", std::process::id()))
    }

    fn sample(byte: u8, len: usize, decode_time: u64, sync: bool) -> Sample {
        let mut data = (len as u32).to_be_bytes().to_vec();
        data.extend(std::iter::repeat_n(byte, len));
        Sample {
            data,
            decode_time,
            sync,
        }
    }

    #[test]
    fn fragmented_file_round_trips_sample_locations_and_timeline() {
        let path = test_file("round-trip");
        let file = File::create(&path).unwrap();
        let mut writer = FragmentedMp4Writer::new(file, 640, 360, &SPS, &PPS, None).unwrap();
        writer.push(sample(1, 10, 9_000, true)).unwrap();
        writer.push(sample(2, 20, 12_000, false)).unwrap();
        writer.push(sample(3, 30, 15_000, true)).unwrap();
        writer.finish(3_000).unwrap();

        let mut file = File::open(&path).unwrap();
        let track = read_video_track(&mut file).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(track.timescale, TIMESCALE);
        assert_eq!(track.nal_length_size, 4);
        assert_eq!(track.parameter_sets, [SPS.to_vec(), PPS.to_vec()]);
        let timeline: Vec<_> = track
            .samples
            .iter()
            .map(|sample| (sample.decode_time, sample.duration, sample.size))
            .collect();
        assert_eq!(
            timeline,
            [(9_000, 3_000, 14), (12_000, 3_000, 24), (15_000, 3_000, 34)]
        );
        for (location, byte) in track.samples.iter().zip([1_u8, 2, 3]) {
            let start = location.offset as usize + 4;
            assert_eq!(bytes[start], byte);
            assert_eq!(
                bytes[location.offset as usize + location.size as usize - 1],
                byte
            );
        }
    }

    #[test]
    fn audio_samples_follow_video_in_each_fragment_without_moving_video_samples() {
        let path = test_file("audio");
        let file = File::create(&path).unwrap();
        let mut writer =
            FragmentedMp4Writer::new(file, 64, 64, &SPS, &PPS, Some(OpusTrack { pre_skip: 312 }))
                .unwrap();
        writer.push(sample(1, 10, 0, true)).unwrap();
        writer.push_audio(AudioSample {
            data: vec![0xa1; 7],
            duration: 960,
        });
        writer.push(sample(2, 10, 3_000, true)).unwrap();
        writer.push_audio(AudioSample {
            data: vec![0xa2; 5],
            duration: 960,
        });
        writer.finish(3_000).unwrap();

        let track = read_video_track(&mut File::open(&path).unwrap()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(track.samples.len(), 2);
        for (location, byte) in track.samples.iter().zip([1_u8, 2]) {
            let end = location.offset as usize + location.size as usize;
            assert_eq!(bytes[end - 1], byte);
        }
        let first_end = track.samples[0].offset as usize + track.samples[0].size as usize;
        assert_eq!(bytes[first_end..first_end + 7], [0xa1; 7]);
        let dops = bytes.windows(4).position(|w| w == b"dOps").unwrap() + 4;
        assert_eq!(bytes[dops..dops + 4], [0, 2, 0x01, 0x38]);
    }

    #[test]
    fn truncated_trailing_fragment_keeps_the_complete_ones() {
        let path = test_file("truncated");
        let file = File::create(&path).unwrap();
        let mut writer = FragmentedMp4Writer::new(file, 64, 64, &SPS, &PPS, None).unwrap();
        writer.push(sample(1, 10, 0, true)).unwrap();
        writer.push(sample(2, 10, 3_000, true)).unwrap();
        writer.finish(3_000).unwrap();
        let complete_len = std::fs::metadata(&path).unwrap().len();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(complete_len - 5)
            .unwrap();

        let track = read_video_track(&mut File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(track.samples.len(), 1);
    }

    #[test]
    fn high_profile_configuration_carries_chroma_format_fields() {
        let high_sps = [0x67, 100, 0, 40];

        let entry = sample_entry(64, 64, &high_sps, &PPS).unwrap();

        let configuration = &entry[entry.windows(4).position(|w| w == b"avcC").unwrap() + 4..];
        let pps_end = 6 + 2 + high_sps.len() + 1 + 2 + PPS.len();
        assert_eq!(&configuration[pps_end..pps_end + 4], [0xfd, 0xf8, 0xf8, 0]);
    }
}
