use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::AirecError;

pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

pub const DEFAULT_CLICK_DURATION_MS: u64 = 500;
pub const DEFAULT_CLICK_SIZE: f32 = 34.0;
pub const DEFAULT_DRAG_DURATION_MS: u64 = 650;
pub const DEFAULT_DRAG_SIZE: f32 = 4.0;
pub const DEFAULT_TRAIL_DURATION_MS: u64 = 250;
pub const DEFAULT_TRAIL_SIZE: f32 = 3.0;
pub const DEFAULT_DRAG_THRESHOLD_PIXELS: u32 = 4;
pub const DEFAULT_DRAG_THRESHOLD_MS: u64 = 150;
pub const DEFAULT_MOVE_THROTTLE_MS: u64 = 100;
/// Covers more than 100 minutes at the default 10 Hz move rate while bounding memory to a few MB.
pub const DEFAULT_MAX_INPUT_HISTORY_EVENTS: usize = 65_536;

/// An RGB effect color that is independent of the capture frame's pixel format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl EffectColor {
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// v0.1-compatible click colors: yellow for left and blue for right clicks.
pub const DEFAULT_LEFT_CLICK_COLOR: EffectColor = EffectColor::rgb(0xFF, 0xD4, 0x00);
pub const DEFAULT_RIGHT_CLICK_COLOR: EffectColor = EffectColor::rgb(0x00, 0xA2, 0xFF);
pub const DEFAULT_DRAG_COLOR: EffectColor = EffectColor::rgb(0xFF, 0x40, 0x81);
pub const DEFAULT_TRAIL_COLOR: EffectColor = EffectColor::rgb(0x00, 0xE5, 0xFF);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClickEffectStyle {
    pub left_color: EffectColor,
    pub right_color: EffectColor,
    /// Maximum ripple radius in output-frame pixels.
    pub size: f32,
    pub duration_ms: u64,
}

impl Default for ClickEffectStyle {
    fn default() -> Self {
        Self {
            left_color: DEFAULT_LEFT_CLICK_COLOR,
            right_color: DEFAULT_RIGHT_CLICK_COLOR,
            size: DEFAULT_CLICK_SIZE,
            duration_ms: DEFAULT_CLICK_DURATION_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct LineEffectStyle {
    pub color: EffectColor,
    /// Stroke width in output-frame pixels.
    pub size: f32,
    /// Time the completed stroke or movement segment remains visible.
    pub duration_ms: u64,
}

impl LineEffectStyle {
    #[must_use]
    pub const fn drag_default() -> Self {
        Self {
            color: DEFAULT_DRAG_COLOR,
            size: DEFAULT_DRAG_SIZE,
            duration_ms: DEFAULT_DRAG_DURATION_MS,
        }
    }

    #[must_use]
    pub const fn trail_default() -> Self {
        Self {
            color: DEFAULT_TRAIL_COLOR,
            size: DEFAULT_TRAIL_SIZE,
            duration_ms: DEFAULT_TRAIL_DURATION_MS,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectStyles {
    pub click: ClickEffectStyle,
    pub drag: LineEffectStyle,
    pub trail: LineEffectStyle,
}

impl Default for EffectStyles {
    fn default() -> Self {
        Self {
            click: ClickEffectStyle::default(),
            drag: LineEffectStyle::drag_default(),
            trail: LineEffectStyle::trail_default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputOptions {
    /// A drag starts as soon as either this displacement or the time threshold is exceeded.
    pub drag_threshold_pixels: u32,
    /// A non-stationary press becomes a drag after this duration even below the pixel threshold.
    pub drag_threshold_ms: u64,
    /// Only `move` recording is throttled; drag recognition examines every mouse move.
    pub move_throttle_ms: u64,
    /// Maximum retained events. Lagging subscribers resume at the oldest retained event.
    pub max_history_events: usize,
}

impl Default for InputOptions {
    fn default() -> Self {
        Self {
            drag_threshold_pixels: DEFAULT_DRAG_THRESHOLD_PIXELS,
            drag_threshold_ms: DEFAULT_DRAG_THRESHOLD_MS,
            move_throttle_ms: DEFAULT_MOVE_THROTTLE_MS,
            max_history_events: DEFAULT_MAX_INPUT_HISTORY_EVENTS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    #[default]
    Continue,
    Abort,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
}

impl Quality {
    #[must_use]
    pub fn bitrate(self, width: u32, height: u32, fps: u32) -> u32 {
        let pixels_per_second = width as u64 * height as u64 * fps as u64;
        let divisor = match self {
            Self::Low => 36,
            Self::Medium => 24,
            Self::High => 14,
        };
        let bits = pixels_per_second / divisor;
        bits.clamp(500_000, 25_000_000) as u32
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CaptureTarget {
    PrimaryMonitor,
    Monitor(usize),
    AllMonitors,
    WindowTitle(String),
    WindowHandle(isize),
    Process(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Monitor,
    Window,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedTarget {
    pub kind: TargetKind,
    pub id: String,
    pub title: Option<String>,
    pub process: Option<String>,
    pub hwnd: Option<isize>,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
    pub output: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub scale: f64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

#[derive(Clone, Debug)]
pub struct RecordingOptions {
    pub started_at: std::time::Instant,
    pub fps: u32,
    pub quality: Quality,
    pub cursor: bool,
    pub effects: bool,
    pub effect_styles: EffectStyles,
    pub input: InputOptions,
    pub max_duration: Duration,
    pub first_frame_timeout: Duration,
    pub failure_policy: FailurePolicy,
    pub event_log: Option<PathBuf>,
    pub verbose: bool,
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            fps: 30,
            quality: Quality::Medium,
            cursor: true,
            effects: true,
            effect_styles: EffectStyles::default(),
            input: InputOptions::default(),
            max_duration: Duration::from_secs(30 * 60),
            first_frame_timeout: FIRST_FRAME_TIMEOUT,
            failure_policy: FailurePolicy::Continue,
            event_log: None,
            verbose: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bgra: Vec<u8>,
    pub t_ms: u64,
}

pub trait FrameSource: Send {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CaptureFrame>, AirecError>;
    fn is_target_alive(&self) -> bool;
    fn is_minimized(&self) -> bool;
    /// Frames discarded by the capture transport before the session could consume them.
    fn dropped_frames(&self) -> u64 {
        0
    }
}

pub trait TargetCatalog: Send + Sync {
    fn monitors(&self) -> Result<Vec<MonitorInfo>, AirecError>;
    fn windows(&self) -> Result<Vec<WindowInfo>, AirecError>;
    fn resolve(
        &self,
        targets: &[CaptureTarget],
        output: &std::path::Path,
        output_is_directory: bool,
    ) -> Result<Vec<ResolvedTarget>, AirecError>;
}

pub trait CaptureBackend: TargetCatalog {
    fn open(
        &self,
        target: &ResolvedTarget,
        options: &RecordingOptions,
    ) -> Result<Box<dyn FrameSource>, AirecError>;
}

pub trait PipelineEncoder: Send {
    fn write_frame(&mut self, frame: &CaptureFrame) -> Result<(), AirecError>;
    fn finish(&mut self) -> Result<(), AirecError>;
}

pub trait FrameProcessor: Send {
    fn process(&mut self, frame: CaptureFrame) -> Result<CaptureFrame, AirecError>;
}

impl FrameProcessor for () {
    fn process(&mut self, frame: CaptureFrame) -> Result<CaptureFrame, AirecError> {
        Ok(frame)
    }
}

pub trait EncoderFactory: Send + Sync {
    fn create(
        &self,
        target: &ResolvedTarget,
        options: &RecordingOptions,
    ) -> Result<Box<dyn PipelineEncoder>, AirecError>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum InputEvent {
    Click {
        t_ms: u64,
        button: MouseButton,
        x: i32,
        y: i32,
        double: bool,
    },
    DragStart {
        t_ms: u64,
        button: MouseButton,
        x: i32,
        y: i32,
    },
    DragEnd {
        t_ms: u64,
        button: MouseButton,
        x: i32,
        y: i32,
    },
    Move {
        t_ms: u64,
        x: i32,
        y: i32,
    },
}

pub trait InputSource: Send {
    fn drain_until(&mut self, t_ms: u64) -> Vec<InputEvent>;
}
