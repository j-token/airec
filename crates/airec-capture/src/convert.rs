//! Offline MP4-to-GIF conversion. Each platform decodes H.264 with a decoder it ships with and
//! hands visible BGRA frames to the shared GIF writer.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use airec_core::{AirecError, ErrorCode};

#[cfg(target_os = "linux")]
use crate::linux_backend::decoder as platform_decoder;
#[cfg(windows)]
use crate::windows_backend::decoder as platform_decoder;

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

/// Decode an H.264 MP4 and write an animated GIF89a.
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
    let mut assembler = GifAssembler::new(&input, output, file, options);
    platform_decoder::decode(&input, &mut assembler)?;
    let conversion = assembler.finish()?;

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

/// Timing and visible size of one decoded source sample, in 100-nanosecond units.
pub(crate) struct SourceSample {
    pub timestamp_hns: i64,
    pub duration_hns: u64,
    pub width: u32,
    pub height: u32,
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
    image: GifFrame,
    start_hns: u64,
}

/// Samples decoded frames at the requested rate and writes them as a differenced GIF.
pub(crate) struct GifAssembler {
    input: PathBuf,
    output: PathBuf,
    options: ConversionOptions,
    writer: Option<BufWriter<File>>,
    gif: Option<GifEncoder<BufWriter<File>>>,
    source_dimensions: Option<(u32, u32)>,
    output_dimensions: Option<(u32, u32)>,
    sampler: TimestampSampler,
    pending_frame: Option<PendingFrame>,
    frame_differ: Option<FrameDiffer>,
    frames: u64,
    duration_ms: u64,
}

impl GifAssembler {
    fn new(input: &Path, output: &Path, file: File, options: ConversionOptions) -> Self {
        Self {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            options,
            writer: Some(BufWriter::new(file)),
            gif: None,
            source_dimensions: None,
            output_dimensions: None,
            sampler: TimestampSampler::new(options.fps),
            pending_frame: None,
            frame_differ: None,
            frames: 0,
            duration_ms: 0,
        }
    }

    /// `visible_bgra` is called only for samples the GIF keeps, and returns the visible area as
    /// tightly packed top-down BGRA.
    pub(crate) fn push_sample(
        &mut self,
        sample: SourceSample,
        visible_bgra: impl FnOnce() -> Result<Vec<u8>, AirecError>,
    ) -> Result<(), AirecError> {
        if self
            .source_dimensions
            .is_some_and(|dimensions| dimensions != (sample.width, sample.height))
        {
            return Err(decode_error(
                &self.input,
                "handle media type change",
                "video dimensions changed during conversion",
            ));
        }
        let timing = self
            .sampler
            .observe(sample.timestamp_hns, sample.duration_hns);
        if !timing.selected {
            return Ok(());
        }
        let (output_width, output_height) = match self.output_dimensions {
            Some(dimensions) => dimensions,
            None => {
                let dimensions =
                    scaled_dimensions(sample.width, sample.height, self.options.max_width)?;
                self.output_dimensions = Some(dimensions);
                dimensions
            }
        };
        let visible = visible_bgra()?;
        let scaled = downscale_bgra(
            &visible,
            sample.width,
            sample.height,
            output_width,
            output_height,
        );

        let Some(gif) = self.gif.as_mut() else {
            let gif_width = u16::try_from(output_width)
                .map_err(|_| dimension_error(output_width, output_height))?;
            let gif_height = u16::try_from(output_height)
                .map_err(|_| dimension_error(output_width, output_height))?;
            let (image, differ) = FrameDiffer::first(&scaled, gif_width, gif_height);
            self.gif = Some(
                GifEncoder::new(
                    self.writer
                        .take()
                        .expect("GIF writer is consumed with the first frame"),
                    gif_width,
                    gif_height,
                    &image.palette,
                )
                .map_err(|error| output_error(&self.output, "write GIF header", error))?,
            );
            self.source_dimensions = Some((sample.width, sample.height));
            self.pending_frame = Some(PendingFrame {
                image,
                start_hns: timing.relative_hns,
            });
            self.frame_differ = Some(differ);
            return Ok(());
        };
        let difference = self
            .frame_differ
            .as_mut()
            .expect("GIF writer and frame differ are initialized together")
            .difference(&scaled);
        if let Some(image) = difference {
            let previous = self
                .pending_frame
                .take()
                .expect("a changed frame follows a pending frame");
            let (written_frames, written_duration_ms) = write_timed_frame(
                gif,
                &previous.image,
                previous.start_hns,
                timing.relative_hns,
            )
            .map_err(|error| output_error(&self.output, "write GIF frame", error))?;
            self.frames = self.frames.saturating_add(written_frames);
            self.duration_ms = self.duration_ms.saturating_add(written_duration_ms);
            self.pending_frame = Some(PendingFrame {
                image,
                start_hns: timing.relative_hns,
            });
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ConversionResult, AirecError> {
        let pending_frame = self.pending_frame.take().ok_or_else(|| {
            decode_error(
                &self.input,
                "decode video",
                "input contains no decodable video frames",
            )
        })?;
        let end_hns = self.sampler.end_hns(pending_frame.start_hns);
        let mut gif = self
            .gif
            .take()
            .expect("a pending frame has an initialized GIF writer");
        let (written_frames, written_duration_ms) = write_timed_frame(
            &mut gif,
            &pending_frame.image,
            pending_frame.start_hns,
            end_hns,
        )
        .map_err(|error| output_error(&self.output, "write final GIF frame", error))?;
        let frames = self.frames.saturating_add(written_frames);
        let duration_ms = self.duration_ms.saturating_add(written_duration_ms);

        let mut writer = gif
            .finish()
            .map_err(|error| output_error(&self.output, "finalize GIF", error))?;
        writer
            .flush()
            .map_err(|error| output_error(&self.output, "flush GIF", error))?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| output_error(&self.output, "synchronize GIF", error))?;
        drop(writer);
        let (width, height) = self
            .output_dimensions
            .expect("a non-empty conversion has GIF dimensions");

        Ok(ConversionResult {
            output: self.output,
            width,
            height,
            frames,
            duration_ms,
        })
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

const HISTOGRAM_BITS: usize = 6;
const HISTOGRAM_LEVELS: usize = 1 << HISTOGRAM_BITS;
const COLOR_HISTOGRAM_SIZE: usize = HISTOGRAM_LEVELS * HISTOGRAM_LEVELS * HISTOGRAM_LEVELS;
const INITIAL_PALETTE_COLORS: usize = 255;
const DIFFERENCE_PALETTE_COLORS: usize = 255;

#[derive(Clone, Copy, Default)]
struct ColorBin {
    count: u64,
    red: u64,
    green: u64,
    blue: u64,
}

struct ColorBox {
    bins: Vec<usize>,
    population: u64,
}

impl ColorBox {
    fn range_and_axis(&self) -> (u8, usize) {
        let mut minimum = [u8::MAX; 3];
        let mut maximum = [0_u8; 3];
        for &key in &self.bins {
            for axis in 0..3 {
                let component = histogram_component(key, axis);
                minimum[axis] = minimum[axis].min(component);
                maximum[axis] = maximum[axis].max(component);
            }
        }
        let ranges = [
            maximum[0] - minimum[0],
            maximum[1] - minimum[1],
            maximum[2] - minimum[2],
        ];
        let axis = (0..3).max_by_key(|&axis| ranges[axis]).unwrap_or(0);
        (ranges[axis], axis)
    }
}

#[derive(Clone)]
struct Palette {
    colors: Vec<[u8; 3]>,
    table_size: usize,
}

impl Palette {
    fn new(mut colors: Vec<[u8; 3]>) -> Self {
        if colors.is_empty() {
            colors.push([0, 0, 0]);
        }
        let table_size = colors.len().next_power_of_two().clamp(2, 256);
        Self { colors, table_size }
    }

    fn size_code(&self) -> u8 {
        u8::try_from(self.table_size.ilog2() - 1).expect("GIF palette size code fits in u8")
    }

    fn minimum_code_size(&self) -> u8 {
        u8::try_from(self.table_size.ilog2())
            .expect("GIF LZW code size fits in u8")
            .max(2)
    }

    fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        for color in &self.colors {
            writer.write_all(color)?;
        }
        for _ in self.colors.len()..self.table_size {
            writer.write_all(&[0, 0, 0])?;
        }
        Ok(())
    }
}

fn rgb_pixels(bgra: &[u8]) -> Vec<[u8; 3]> {
    bgra.chunks_exact(4)
        .map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect()
}

fn histogram_key(color: [u8; 3]) -> usize {
    color.into_iter().fold(0, |key, channel| {
        (key << HISTOGRAM_BITS) | usize::from(channel >> (8 - HISTOGRAM_BITS))
    })
}

fn histogram_component(key: usize, axis: usize) -> u8 {
    ((key >> ((2 - axis) * HISTOGRAM_BITS)) & (HISTOGRAM_LEVELS - 1)) as u8
}

fn adaptive_palette(pixels: &[[u8; 3]], maximum_colors: usize) -> Vec<[u8; 3]> {
    let mut histogram = vec![ColorBin::default(); COLOR_HISTOGRAM_SIZE];
    for &color in pixels {
        let bin = &mut histogram[histogram_key(color)];
        bin.count += 1;
        bin.red += u64::from(color[0]);
        bin.green += u64::from(color[1]);
        bin.blue += u64::from(color[2]);
    }
    let occupied = histogram
        .iter()
        .enumerate()
        .filter_map(|(index, bin)| (bin.count != 0).then_some(index))
        .collect::<Vec<_>>();
    if occupied.is_empty() {
        return vec![[0, 0, 0]];
    }
    let mut boxes = vec![ColorBox {
        bins: occupied,
        population: pixels.len() as u64,
    }];
    while boxes.len() < maximum_colors {
        let selected = boxes
            .iter()
            .enumerate()
            .filter_map(|(index, color_box)| {
                let (range, _) = color_box.range_and_axis();
                (color_box.bins.len() > 1 && range != 0)
                    .then_some((index, color_box.population * u64::from(range)))
            })
            .max_by_key(|&(_, score)| score)
            .map(|(index, _)| index);
        let Some(selected) = selected else {
            break;
        };
        let mut color_box = boxes.swap_remove(selected);
        let (_, axis) = color_box.range_and_axis();
        color_box
            .bins
            .sort_unstable_by_key(|&key| histogram_component(key, axis));
        let target = color_box.population.div_ceil(2);
        let mut accumulated = 0_u64;
        let mut split = 1_usize;
        for (position, &key) in color_box.bins.iter().enumerate() {
            accumulated += histogram[key].count;
            if accumulated >= target {
                split = (position + 1).min(color_box.bins.len() - 1);
                break;
            }
        }
        let right_bins = color_box.bins.split_off(split);
        let left_population = color_box.bins.iter().map(|&key| histogram[key].count).sum();
        let right_population = color_box.population - left_population;
        boxes.push(ColorBox {
            bins: color_box.bins,
            population: left_population,
        });
        boxes.push(ColorBox {
            bins: right_bins,
            population: right_population,
        });
    }
    boxes
        .into_iter()
        .map(|color_box| {
            let mut total = ColorBin::default();
            for key in color_box.bins {
                let bin = histogram[key];
                total.count += bin.count;
                total.red += bin.red;
                total.green += bin.green;
                total.blue += bin.blue;
            }
            [
                (total.red / total.count) as u8,
                (total.green / total.count) as u8,
                (total.blue / total.count) as u8,
            ]
        })
        .collect()
}

fn color_error(left: [u8; 3], right: [u8; 3]) -> u32 {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| {
            let difference = i32::from(left) - i32::from(right);
            (difference * difference) as u32
        })
        .sum()
}

fn nearest_palette_index(color: [u8; 3], palette: &[[u8; 3]]) -> usize {
    palette
        .iter()
        .enumerate()
        .min_by_key(|&(_, &candidate)| color_error(color, candidate))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

struct PaletteLookup {
    indices: Vec<u8>,
}

impl PaletteLookup {
    fn new(pixels: &[[u8; 3]], palette: &[[u8; 3]]) -> Self {
        let mut bins = vec![ColorBin::default(); COLOR_HISTOGRAM_SIZE];
        for &color in pixels {
            let bin = &mut bins[histogram_key(color)];
            bin.count += 1;
            bin.red += u64::from(color[0]);
            bin.green += u64::from(color[1]);
            bin.blue += u64::from(color[2]);
        }
        let mut indices = vec![0_u8; COLOR_HISTOGRAM_SIZE];
        for (key, bin) in bins
            .into_iter()
            .enumerate()
            .filter(|(_, bin)| bin.count != 0)
        {
            let representative = [
                (bin.red / bin.count) as u8,
                (bin.green / bin.count) as u8,
                (bin.blue / bin.count) as u8,
            ];
            indices[key] = nearest_palette_index(representative, palette) as u8;
        }
        Self { indices }
    }

    fn index(&self, color: [u8; 3]) -> usize {
        usize::from(self.indices[histogram_key(color)])
    }
}

#[derive(Clone)]
struct GifFrame {
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    indexed: Vec<u8>,
    palette: Palette,
    local_palette: bool,
    transparent: bool,
}

struct FrameDiffer {
    width: u16,
    height: u16,
    previous_source: Vec<[u8; 3]>,
    rendered: Vec<[u8; 3]>,
    global_palette: Palette,
}

struct PaletteUpdate {
    changed: Vec<(usize, [u8; 3], u8)>,
    error: u64,
}

impl FrameDiffer {
    fn first(bgra: &[u8], width: u16, height: u16) -> (GifFrame, Self) {
        let source = rgb_pixels(bgra);
        let opaque_colors = adaptive_palette(&source, INITIAL_PALETTE_COLORS);
        let lookup = PaletteLookup::new(&source, &opaque_colors);
        let mut colors = Vec::with_capacity(opaque_colors.len() + 1);
        colors.push([0, 0, 0]);
        colors.extend_from_slice(&opaque_colors);
        let palette = Palette::new(colors);
        let mut rendered = Vec::with_capacity(source.len());
        let indexed = source
            .iter()
            .map(|&color| {
                let index = lookup.index(color);
                rendered.push(opaque_colors[index]);
                (index + 1) as u8
            })
            .collect();
        (
            GifFrame {
                left: 0,
                top: 0,
                width,
                height,
                indexed,
                palette: palette.clone(),
                local_palette: false,
                transparent: false,
            },
            Self {
                width,
                height,
                previous_source: source,
                rendered,
                global_palette: palette,
            },
        )
    }

    fn difference(&mut self, bgra: &[u8]) -> Option<GifFrame> {
        let source = rgb_pixels(bgra);
        debug_assert_eq!(source.len(), self.previous_source.len());
        let candidates = source
            .iter()
            .copied()
            .zip(&self.previous_source)
            .enumerate()
            .filter_map(|(index, (current, &previous))| {
                (current != previous).then_some((index, current))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            self.previous_source = source;
            return None;
        }
        let candidate_colors = candidates
            .iter()
            .map(|&(_, color)| color)
            .collect::<Vec<_>>();
        let global_update = self.palette_update(
            &candidates,
            &candidate_colors,
            &self.global_palette.colors[1..],
        );
        let adaptive_colors = adaptive_palette(&candidate_colors, DIFFERENCE_PALETTE_COLORS);
        let adaptive_update = self.palette_update(&candidates, &candidate_colors, &adaptive_colors);
        self.previous_source = source;

        let (update, palette, local_palette) = if global_update.error <= adaptive_update.error {
            (global_update, self.global_palette.clone(), false)
        } else {
            let mut colors = Vec::with_capacity(adaptive_colors.len() + 1);
            colors.push([0, 0, 0]);
            colors.extend(adaptive_colors);
            (adaptive_update, Palette::new(colors), true)
        };
        let changed = update
            .changed
            .into_iter()
            .map(|(index, color, palette_index)| {
                self.rendered[index] = color;
                (index, palette_index)
            })
            .collect();
        self.difference_image(changed, palette, local_palette)
    }

    /// Maps changed pixels onto `opaque_colors`, keeping only replacements that bring the
    /// displayed canvas closer to the source. `error` covers every changed pixel, replaced or not.
    fn palette_update(
        &self,
        candidates: &[(usize, [u8; 3])],
        candidate_colors: &[[u8; 3]],
        opaque_colors: &[[u8; 3]],
    ) -> PaletteUpdate {
        let lookup = PaletteLookup::new(candidate_colors, opaque_colors);
        let mut changed = Vec::new();
        let mut error = 0_u64;
        for &(index, color) in candidates {
            let palette_index = lookup.index(color);
            let replacement = opaque_colors[palette_index];
            let replacement_error = color_error(color, replacement);
            let current_error = color_error(color, self.rendered[index]);
            if replacement_error < current_error {
                changed.push((index, replacement, (palette_index + 1) as u8));
                error += u64::from(replacement_error);
            } else {
                error += u64::from(current_error);
            }
        }
        PaletteUpdate { changed, error }
    }

    fn difference_image(
        &self,
        changed: Vec<(usize, u8)>,
        palette: Palette,
        local_palette: bool,
    ) -> Option<GifFrame> {
        if changed.is_empty() {
            return None;
        }
        let mut minimum_x = usize::from(self.width);
        let mut minimum_y = usize::from(self.height);
        let mut maximum_x = 0_usize;
        let mut maximum_y = 0_usize;
        let frame_width = usize::from(self.width);
        for &(index, _) in &changed {
            let x = index % frame_width;
            let y = index / frame_width;
            minimum_x = minimum_x.min(x);
            minimum_y = minimum_y.min(y);
            maximum_x = maximum_x.max(x);
            maximum_y = maximum_y.max(y);
        }
        let width = maximum_x - minimum_x + 1;
        let height = maximum_y - minimum_y + 1;
        let mut indexed = vec![0_u8; width * height];
        for (index, palette_index) in changed {
            let x = index % frame_width;
            let y = index / frame_width;
            indexed[(y - minimum_y) * width + x - minimum_x] = palette_index;
        }
        Some(GifFrame {
            left: minimum_x as u16,
            top: minimum_y as u16,
            width: width as u16,
            height: height as u16,
            indexed,
            palette,
            local_palette,
            transparent: true,
        })
    }
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
    image: &GifFrame,
    start_hns: u64,
    end_hns: u64,
) -> io::Result<(u64, u64)> {
    let mut remaining_delay = timeline_delay_centiseconds(start_hns, end_hns);
    let total_delay = remaining_delay;
    let mut frames = 0_u64;
    while remaining_delay != 0 {
        let delay = u16::try_from(remaining_delay.min(u64::from(u16::MAX)))
            .expect("GIF delay chunk is bounded by u16::MAX");
        gif.write_frame(image, delay)?;
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
    fn new(mut writer: W, width: u16, height: u16, global_palette: &Palette) -> io::Result<Self> {
        writer.write_all(b"GIF89a")?;
        writer.write_all(&width.to_le_bytes())?;
        writer.write_all(&height.to_le_bytes())?;
        writer.write_all(&[0xf0 | global_palette.size_code(), 0, 0])?;
        global_palette.write_to(&mut writer)?;
        // Netscape application extension: repeat forever (loop count zero).
        writer.write_all(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00")?;
        Ok(Self {
            writer,
            width,
            height,
        })
    }

    fn write_frame(&mut self, image: &GifFrame, delay_centiseconds: u16) -> io::Result<()> {
        let expected = usize::from(image.width) * usize::from(image.height);
        if image.indexed.len() != expected
            || u32::from(image.left) + u32::from(image.width) > u32::from(self.width)
            || u32::from(image.top) + u32::from(image.height) > u32::from(self.height)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "indexed frame lies outside the GIF canvas",
            ));
        }
        // Disposal method 1 preserves the composited canvas for transparent difference frames.
        let control = if image.transparent { 0x05 } else { 0x04 };
        self.writer.write_all(&[0x21, 0xf9, 0x04, control])?;
        self.writer.write_all(&delay_centiseconds.to_le_bytes())?;
        self.writer.write_all(b"\x00\x00")?;
        self.writer.write_all(b"\x2c")?;
        self.writer.write_all(&image.left.to_le_bytes())?;
        self.writer.write_all(&image.top.to_le_bytes())?;
        self.writer.write_all(&image.width.to_le_bytes())?;
        self.writer.write_all(&image.height.to_le_bytes())?;
        let descriptor = if image.local_palette {
            0x80 | image.palette.size_code()
        } else {
            0
        };
        self.writer.write_all(&[descriptor])?;
        if image.local_palette {
            image.palette.write_to(&mut self.writer)?;
        }
        let minimum_code_size = image.palette.minimum_code_size();
        self.writer.write_all(&[minimum_code_size])?;
        let compressed = lzw_encode(&image.indexed, minimum_code_size);
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

fn lzw_encode(indexed: &[u8], minimum_code_size: u8) -> Vec<u8> {
    const MAX_CODE: u16 = 4095;

    debug_assert!((2..=8).contains(&minimum_code_size));
    let clear = 1_u16 << minimum_code_size;
    let end = clear + 1;
    let first_free = end + 1;
    let mut output = BitWriter::new();
    let mut dictionary = HashMap::<(u16, u8), u16>::new();
    let mut next_code = first_free;
    let mut code_width = minimum_code_size + 1;
    output.write(clear, code_width);

    let Some((&first, rest)) = indexed.split_first() else {
        output.write(end, code_width);
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
            output.write(clear, code_width);
            dictionary.clear();
            next_code = first_free;
            code_width = minimum_code_size + 1;
        }
        prefix = u16::from(suffix);
    }

    output.write(prefix, code_width);
    if next_code == (1_u16 << code_width) && code_width < 12 {
        code_width += 1;
    }
    output.write(end, code_width);
    output.finish()
}

pub(crate) fn dimension_error(width: u32, height: u32) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        "video dimensions cannot be represented by GIF",
        serde_json::json!({"width": width, "height": height}),
    )
}

pub(crate) fn decode_error(
    input: &Path,
    operation: &'static str,
    message: impl Into<String>,
) -> AirecError {
    AirecError::new(
        ErrorCode::CaptureInitFailed,
        message,
        serde_json::json!({
            "component": platform_decoder::COMPONENT,
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

    /// The 3-3-2 palette earlier releases used, kept as the benchmark baseline.
    fn legacy_fixed_palette() -> Vec<[u8; 3]> {
        (0_u16..=255)
            .map(|index| {
                let red = (index >> 5) & 0x07;
                let green = (index >> 2) & 0x07;
                let blue = index & 0x03;
                [
                    ((red * 255 + 3) / 7) as u8,
                    ((green * 255 + 3) / 7) as u8,
                    ((blue * 255 + 1) / 3) as u8,
                ]
            })
            .collect()
    }

    fn legacy_quantize_bgra(bgra: &[u8]) -> Vec<u8> {
        bgra.chunks_exact(4)
            .map(|pixel| (pixel[2] >> 5) << 5 | (pixel[1] >> 5) << 2 | pixel[0] >> 6)
            .collect()
    }

    fn solid_bgra(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
        [rgb[2], rgb[1], rgb[0], 255].repeat(width * height)
    }

    #[test]
    fn dark_gray_first_frame_is_not_crushed_to_black() {
        let frame = solid_bgra(8, 4, [20, 20, 20]);

        let (_, differ) = FrameDiffer::first(&frame, 8, 4);

        assert!(differ.rendered.iter().all(|&color| color == [20, 20, 20]));
    }

    #[test]
    fn gray_ramp_first_frame_stays_within_histogram_precision() {
        let frame = (0..=255_u8)
            .flat_map(|level| [level, level, level, 255])
            .collect::<Vec<_>>();

        let (_, differ) = FrameDiffer::first(&frame, 256, 1);

        let worst_channel_error = differ
            .rendered
            .iter()
            .zip(0..=255_u8)
            .map(|(color, level)| color[0].abs_diff(level))
            .max()
            .unwrap();
        assert!(worst_channel_error <= 2, "error {worst_channel_error}");
    }

    #[test]
    fn color_missing_from_first_palette_is_reproduced_by_local_palette() {
        let first = solid_bgra(8, 4, [240, 240, 240]);
        let mut second = first.clone();
        second[..16].copy_from_slice(&solid_bgra(4, 1, [124, 58, 237]));
        let (_, mut differ) = FrameDiffer::first(&first, 8, 4);

        let difference = differ.difference(&second).unwrap();

        assert!(difference.local_palette);
        assert_eq!(&differ.rendered[..4], &[[124, 58, 237]; 4]);
        assert_eq!(differ.rendered[4], [240, 240, 240]);
    }

    #[test]
    fn color_already_in_first_palette_reuses_global_palette() {
        let mut first = solid_bgra(8, 4, [240, 240, 240]);
        first[..4].copy_from_slice(&solid_bgra(1, 1, [124, 58, 237]));
        let mut second = first.clone();
        second[4..8].copy_from_slice(&solid_bgra(1, 1, [124, 58, 237]));
        let (_, mut differ) = FrameDiffer::first(&first, 8, 4);

        let difference = differ.difference(&second).unwrap();

        assert!(!difference.local_palette);
        assert_eq!(differ.rendered[1], [124, 58, 237]);
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

    #[derive(Clone, Copy, Debug)]
    enum BenchmarkScenario {
        Cursor,
        Localized,
        Scroll,
        FullFrame,
    }

    impl BenchmarkScenario {
        const ALL: [Self; 4] = [Self::Cursor, Self::Localized, Self::Scroll, Self::FullFrame];

        fn name(self) -> &'static str {
            match self {
                Self::Cursor => "cursor",
                Self::Localized => "localized",
                Self::Scroll => "scroll",
                Self::FullFrame => "full-frame",
            }
        }
    }

    struct BenchmarkResult {
        baseline_bytes: usize,
        adaptive_full_frame_bytes: usize,
        optimized_bytes: usize,
        baseline_mse: f64,
        optimized_mse: f64,
        optimized_frames: usize,
    }

    fn desktop_frame(width: usize, height: usize) -> Vec<u8> {
        let mut frame = vec![0_u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let color = if x > width * 2 / 5
                    && x < width * 9 / 10
                    && y > height / 5
                    && y < height * 2 / 3
                {
                    let red = ((x * 11 + y * 3) & 0xff) as u8;
                    let green = ((x * 5 + y * 7 + (x / 13) * 17) & 0xff) as u8;
                    let blue = ((x * 3 + y * 13 + (y / 11) * 19) & 0xff) as u8;
                    [red, green, blue]
                } else if y < height / 12 {
                    [42, 38, 35]
                } else if x < width / 7 {
                    [232, 228, 222]
                } else if y > height * 11 / 12 {
                    [215, 210, 204]
                } else if (y / 19) % 5 == 0 && x > width / 5 && x < width * 4 / 5 {
                    [210, 205, 198]
                } else {
                    let shade = 244_u8.saturating_sub(((x / 97 + y / 83) % 4) as u8);
                    [shade, shade.saturating_sub(2), shade.saturating_sub(5)]
                };
                set_rgb(&mut frame, width, x, y, color);
            }
        }
        frame
    }

    fn set_rgb(frame: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3]) {
        let offset = (y * width + x) * 4;
        frame[offset..offset + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
    }

    fn draw_cursor(frame: &mut [u8], width: usize, height: usize, x: usize, y: usize) {
        for dy in 0..18_usize {
            for dx in 0..12_usize {
                if x + dx >= width || y + dy >= height || dx > dy / 2 + 1 {
                    continue;
                }
                let edge = dx == 0 || dx == dy / 2 + 1 || dy == 17;
                set_rgb(
                    frame,
                    width,
                    x + dx,
                    y + dy,
                    if edge { [12, 12, 12] } else { [250, 250, 250] },
                );
            }
        }
    }

    fn fixture_frames(scenario: BenchmarkScenario, width: usize, height: usize) -> Vec<Vec<u8>> {
        let base = desktop_frame(width, height);
        (0..31_usize)
            .map(|frame_index| {
                let mut frame = base.clone();
                match scenario {
                    BenchmarkScenario::Cursor => {
                        let x = width / 5 + frame_index * (width * 3 / 5) / 30;
                        let y = height / 4 + ((frame_index * 37) % (height / 2).max(1));
                        draw_cursor(&mut frame, width, height, x, y);
                    }
                    BenchmarkScenario::Localized => {
                        let characters = frame_index.saturating_sub(5).min(20);
                        for character in 0..characters {
                            let origin_x = width / 3 + character * 7;
                            let origin_y = height / 2;
                            for dy in 0..11 {
                                for dx in 0..5 {
                                    if (dx + dy + character) % 3 != 0
                                        && origin_x + dx < width
                                        && origin_y + dy < height
                                    {
                                        set_rgb(
                                            &mut frame,
                                            width,
                                            origin_x + dx,
                                            origin_y + dy,
                                            [35, 71, 118],
                                        );
                                    }
                                }
                            }
                        }
                    }
                    BenchmarkScenario::Scroll => {
                        for y in height / 12..height * 11 / 12 {
                            for x in width / 7..width {
                                let source_y = (y + frame_index * 13) % height;
                                let shade = ((source_y / 9 + x / 23) % 9) as u8;
                                let color = if source_y % 37 < 3 {
                                    [55, 92, 136]
                                } else {
                                    [245 - shade * 3, 242 - shade * 2, 236 - shade]
                                };
                                set_rgb(&mut frame, width, x, y, color);
                            }
                        }
                    }
                    BenchmarkScenario::FullFrame => {
                        for y in 0..height {
                            for x in 0..width {
                                let seed = (x as u32).wrapping_mul(0x9e37_79b9)
                                    ^ (y as u32).wrapping_mul(0x85eb_ca6b)
                                    ^ (frame_index as u32).wrapping_mul(0xc2b2_ae35);
                                let mixed = seed ^ (seed >> 15) ^ seed.rotate_left(11);
                                set_rgb(
                                    &mut frame,
                                    width,
                                    x,
                                    y,
                                    [mixed as u8, (mixed >> 8) as u8, (mixed >> 16) as u8],
                                );
                            }
                        }
                    }
                }
                frame
            })
            .collect()
    }

    fn pixel_error(source: &[u8], rendered: &[[u8; 3]]) -> u128 {
        source
            .chunks_exact(4)
            .zip(rendered)
            .map(|(pixel, &rgb)| u128::from(color_error([pixel[2], pixel[1], pixel[0]], rgb)))
            .sum()
    }

    fn benchmark_scenario(scenario: BenchmarkScenario, width: u16, height: u16) -> BenchmarkResult {
        let fixtures = fixture_frames(scenario, usize::from(width), usize::from(height));
        let fixed_colors = legacy_fixed_palette();
        let fixed = Palette::new(fixed_colors.clone());
        let mut baseline = GifEncoder::new(Vec::new(), width, height, &fixed).unwrap();
        let mut baseline_error = 0_u128;
        for frame in &fixtures {
            let indexed = legacy_quantize_bgra(frame);
            let rendered = indexed
                .iter()
                .map(|&index| fixed_colors[usize::from(index)])
                .collect::<Vec<_>>();
            baseline_error += pixel_error(frame, &rendered);
            baseline
                .write_frame(
                    &GifFrame {
                        left: 0,
                        top: 0,
                        width,
                        height,
                        indexed,
                        palette: fixed.clone(),
                        local_palette: false,
                        transparent: false,
                    },
                    10,
                )
                .unwrap();
        }
        let baseline_bytes = baseline.finish().unwrap().len();

        let adaptive_full_frame_bytes = if width == 960 {
            let mut adaptive_full = GifEncoder::new(Vec::new(), width, height, &fixed).unwrap();
            for frame in &fixtures {
                let (image, _) = FrameDiffer::first(frame, width, height);
                let local_image = GifFrame {
                    local_palette: true,
                    ..image
                };
                adaptive_full.write_frame(&local_image, 10).unwrap();
            }
            adaptive_full.finish().unwrap().len()
        } else {
            0
        };

        let (first, mut differ) = FrameDiffer::first(&fixtures[0], width, height);
        let mut optimized = GifEncoder::new(Vec::new(), width, height, &first.palette).unwrap();
        let mut optimized_error = pixel_error(&fixtures[0], &differ.rendered);
        let mut pending = first;
        let mut pending_delay = 10_u16;
        let mut optimized_frames = 0_usize;
        for frame in fixtures.iter().skip(1) {
            let difference = differ.difference(frame);
            optimized_error += pixel_error(frame, &differ.rendered);
            if let Some(difference) = difference {
                optimized.write_frame(&pending, pending_delay).unwrap();
                optimized_frames += 1;
                pending = difference;
                pending_delay = 10;
            } else {
                pending_delay += 10;
            }
        }
        optimized.write_frame(&pending, pending_delay).unwrap();
        optimized_frames += 1;
        let optimized_bytes = optimized.finish().unwrap().len();
        let samples = fixtures.len() as f64 * f64::from(width) * f64::from(height) * 3.0;
        BenchmarkResult {
            baseline_bytes,
            adaptive_full_frame_bytes,
            optimized_bytes,
            baseline_mse: baseline_error as f64 / samples,
            optimized_mse: optimized_error as f64 / samples,
            optimized_frames,
        }
    }

    #[test]
    fn deterministic_gif_corpus_preserves_fidelity_and_static_target() {
        for scenario in BenchmarkScenario::ALL {
            let result = benchmark_scenario(scenario, 96, 54);
            assert!(
                result.optimized_mse <= result.baseline_mse,
                "{} fidelity regressed: {} > {}",
                scenario.name(),
                result.optimized_mse,
                result.baseline_mse
            );
            assert!(
                result.optimized_bytes < result.baseline_bytes
                    || matches!(
                        scenario,
                        BenchmarkScenario::Scroll | BenchmarkScenario::FullFrame
                    ),
                "{} unexpectedly grew: {} -> {}",
                scenario.name(),
                result.baseline_bytes,
                result.optimized_bytes
            );
        }
    }

    #[test]
    #[ignore = "report benchmark; run with --release --ignored --nocapture"]
    fn report_default_dimension_gif_benchmark() {
        for scenario in BenchmarkScenario::ALL {
            let result = benchmark_scenario(scenario, 960, 540);
            println!(
                "scenario={} baseline_bytes={} adaptive_full_frame_bytes={} optimized_bytes={} ratio={:.4} reduction={:.2}% baseline_mse={:.4} optimized_mse={:.4} optimized_frames={}",
                scenario.name(),
                result.baseline_bytes,
                result.adaptive_full_frame_bytes,
                result.optimized_bytes,
                result.optimized_bytes as f64 / result.baseline_bytes as f64,
                (1.0 - result.optimized_bytes as f64 / result.baseline_bytes as f64) * 100.0,
                result.baseline_mse,
                result.optimized_mse,
                result.optimized_frames,
            );
            if matches!(scenario, BenchmarkScenario::Cursor) {
                assert!(result.optimized_bytes * 10 <= result.baseline_bytes);
                assert!(result.optimized_bytes * 20 <= result.baseline_bytes);
            }
        }
    }

    #[test]
    fn transparent_local_palette_frames_composite_to_encoder_state() {
        let frames = fixture_frames(BenchmarkScenario::Cursor, 96, 54);
        let (first, mut differ) = FrameDiffer::first(&frames[0], 96, 54);
        let expected_first = differ.rendered.clone();
        let second = differ.difference(&frames[1]).unwrap();
        let expected_second = differ.rendered.clone();
        let mut encoder = GifEncoder::new(Vec::new(), 96, 54, &first.palette).unwrap();
        encoder.write_frame(&first, 10).unwrap();
        encoder.write_frame(&second, 10).unwrap();
        let decoded = decode_gif_frames(&encoder.finish().unwrap());
        assert_eq!(decoded, [expected_first, expected_second]);
    }

    fn decode_gif_frames(bytes: &[u8]) -> Vec<Vec<[u8; 3]>> {
        assert_eq!(&bytes[..6], b"GIF89a");
        let width = usize::from(u16::from_le_bytes([bytes[6], bytes[7]]));
        let height = usize::from(u16::from_le_bytes([bytes[8], bytes[9]]));
        let packed = bytes[10];
        let mut offset = 13_usize;
        let global_palette = if packed & 0x80 != 0 {
            read_palette(bytes, &mut offset, 1_usize << (usize::from(packed & 7) + 1))
        } else {
            Vec::new()
        };
        let mut transparent_index = None;
        let mut canvas = vec![[0_u8; 3]; width * height];
        let mut frames = Vec::new();
        loop {
            match bytes[offset] {
                0x21 => {
                    let label = bytes[offset + 1];
                    offset += 2;
                    if label == 0xf9 {
                        assert_eq!(bytes[offset], 4);
                        transparent_index =
                            (bytes[offset + 1] & 1 != 0).then_some(bytes[offset + 4]);
                        offset += 6;
                    } else {
                        skip_sub_blocks(bytes, &mut offset);
                    }
                }
                0x2c => {
                    let left =
                        usize::from(u16::from_le_bytes([bytes[offset + 1], bytes[offset + 2]]));
                    let top =
                        usize::from(u16::from_le_bytes([bytes[offset + 3], bytes[offset + 4]]));
                    let image_width =
                        usize::from(u16::from_le_bytes([bytes[offset + 5], bytes[offset + 6]]));
                    let image_height =
                        usize::from(u16::from_le_bytes([bytes[offset + 7], bytes[offset + 8]]));
                    let descriptor = bytes[offset + 9];
                    offset += 10;
                    let local_palette;
                    let palette = if descriptor & 0x80 != 0 {
                        local_palette = read_palette(
                            bytes,
                            &mut offset,
                            1_usize << (usize::from(descriptor & 7) + 1),
                        );
                        &local_palette
                    } else {
                        &global_palette
                    };
                    let minimum_code_size = bytes[offset];
                    offset += 1;
                    let compressed = read_sub_blocks(bytes, &mut offset);
                    let indexed = decode_lzw(&compressed, minimum_code_size);
                    assert_eq!(indexed.len(), image_width * image_height);
                    for y in 0..image_height {
                        for x in 0..image_width {
                            let index = indexed[y * image_width + x];
                            if transparent_index == Some(index) {
                                continue;
                            }
                            canvas[(top + y) * width + left + x] = palette[usize::from(index)];
                        }
                    }
                    frames.push(canvas.clone());
                    transparent_index = None;
                }
                0x3b => break,
                byte => panic!("unexpected GIF block 0x{byte:02x}"),
            }
        }
        frames
    }

    fn read_palette(bytes: &[u8], offset: &mut usize, colors: usize) -> Vec<[u8; 3]> {
        let palette = bytes[*offset..*offset + colors * 3]
            .chunks_exact(3)
            .map(|color| [color[0], color[1], color[2]])
            .collect();
        *offset += colors * 3;
        palette
    }

    fn read_sub_blocks(bytes: &[u8], offset: &mut usize) -> Vec<u8> {
        let mut data = Vec::new();
        loop {
            let length = usize::from(bytes[*offset]);
            *offset += 1;
            if length == 0 {
                return data;
            }
            data.extend_from_slice(&bytes[*offset..*offset + length]);
            *offset += length;
        }
    }

    fn skip_sub_blocks(bytes: &[u8], offset: &mut usize) {
        let _ = read_sub_blocks(bytes, offset);
    }

    #[test]
    fn lzw_round_trips_across_code_width_changes() {
        let pixels = (0..20_000)
            .map(|index| ((index * 73 + index / 17) & 0xff) as u8)
            .collect::<Vec<_>>();
        let compressed = lzw_encode(&pixels, 8);
        assert_eq!(decode_lzw(&compressed, 8), pixels);
        let small_pixels = (0..20_000)
            .map(|index| ((index * 5 + index / 23) & 0x03) as u8)
            .collect::<Vec<_>>();
        let compressed = lzw_encode(&small_pixels, 2);
        assert_eq!(decode_lzw(&compressed, 2), small_pixels);
    }

    #[test]
    fn gif_has_loop_extension_frames_delays_and_trailer() {
        let palette = Palette::new(vec![[0, 0, 0], [255, 255, 255]]);
        let mut gif = GifEncoder::new(Vec::new(), 2, 1, &palette).unwrap();
        let first = GifFrame {
            left: 0,
            top: 0,
            width: 2,
            height: 1,
            indexed: vec![0, 1],
            palette: palette.clone(),
            local_palette: false,
            transparent: false,
        };
        let second = GifFrame {
            indexed: vec![1, 0],
            ..first.clone()
        };
        gif.write_frame(&first, 8).unwrap();
        gif.write_frame(&second, 9).unwrap();
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
                .any(|window| window == b"\x21\xf9\x04\x04\x08\x00\x00\x00")
        );
        assert!(
            bytes
                .windows(8)
                .any(|window| window == b"\x21\xf9\x04\x04\x09\x00\x00\x00")
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

    fn decode_lzw(bytes: &[u8], minimum_code_size: u8) -> Vec<u8> {
        let clear = 1_u16 << minimum_code_size;
        let end = clear + 1;
        let mut reader = BitReader::new(bytes);
        let mut dictionary = (0_u16..clear)
            .map(|value| vec![value as u8])
            .collect::<Vec<_>>();
        dictionary.push(Vec::new());
        dictionary.push(Vec::new());
        let mut width = minimum_code_size + 1;
        let mut previous: Option<Vec<u8>> = None;
        let mut output = Vec::new();
        while let Some(code) = reader.read(width) {
            if code == clear {
                dictionary.truncate(usize::from(end + 1));
                width = minimum_code_size + 1;
                previous = None;
                continue;
            }
            if code == end {
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
