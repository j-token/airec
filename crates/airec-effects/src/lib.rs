//! Pure CPU click-ripple compositing and resize letterboxing.

use airec_core::{CaptureFrame, InputEvent, MouseButton};

const RIPPLE_DURATION_MS: u64 = 500;
const RIPPLE_MAX_RADIUS: f32 = 34.0;
const RING_WIDTH: f32 = 3.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    #[must_use]
    pub const fn width(self) -> i32 {
        self.right - self.left
    }

    #[must_use]
    pub const fn height(self) -> i32 {
        self.bottom - self.top
    }

    #[must_use]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

#[derive(Clone, Debug)]
struct Ripple {
    started_ms: u64,
    x: f32,
    y: f32,
    button: MouseButton,
    double: bool,
}

#[derive(Default)]
pub struct RippleCompositor {
    ripples: Vec<Ripple>,
}

impl RippleCompositor {
    pub fn push_events(
        &mut self,
        events: impl IntoIterator<Item = InputEvent>,
        viewport: ScreenRect,
        frame: &CaptureFrame,
    ) {
        self.push_events_mapped(events, viewport, viewport, frame);
    }

    /// Filters clicks against `clip` and maps physical screen coordinates through `mapping` into
    /// the WGC frame. Window capture uses the client rect for clipping and extended frame bounds
    /// for mapping so title-bar offsets remain correct.
    pub fn push_events_mapped(
        &mut self,
        events: impl IntoIterator<Item = InputEvent>,
        clip: ScreenRect,
        mapping: ScreenRect,
        frame: &CaptureFrame,
    ) {
        for event in events {
            if let InputEvent::Click {
                t_ms,
                button,
                x,
                y,
                double,
            } = event
                && clip.contains(x, y)
            {
                let local_x =
                    (x - mapping.left) as f32 * frame.width as f32 / mapping.width().max(1) as f32;
                let local_y =
                    (y - mapping.top) as f32 * frame.height as f32 / mapping.height().max(1) as f32;
                self.ripples.push(Ripple {
                    started_ms: t_ms,
                    x: local_x,
                    y: local_y,
                    button,
                    double,
                });
            }
        }
    }

    pub fn render(&mut self, frame: &mut CaptureFrame) {
        let now = frame.t_ms;
        self.ripples
            .retain(|ripple| now.saturating_sub(ripple.started_ms) <= RIPPLE_DURATION_MS);
        for ripple in &self.ripples {
            let age = now.saturating_sub(ripple.started_ms);
            if age > RIPPLE_DURATION_MS {
                continue;
            }
            let progress = age as f32 / RIPPLE_DURATION_MS as f32;
            let radius = 6.0 + RIPPLE_MAX_RADIUS * progress;
            let alpha = 1.0 - progress;
            draw_ring(frame, ripple.x, ripple.y, radius, ripple.button, alpha);
            if ripple.double {
                draw_ring(
                    frame,
                    ripple.x,
                    ripple.y,
                    (radius - 7.0).max(2.0),
                    ripple.button,
                    alpha,
                );
            }
        }
    }
}

fn draw_ring(
    frame: &mut CaptureFrame,
    center_x: f32,
    center_y: f32,
    radius: f32,
    button: MouseButton,
    alpha: f32,
) {
    let color = match button {
        // BGRA: #FFD400
        MouseButton::Left => [0x00, 0xD4, 0xFF],
        // BGRA: #00A2FF
        MouseButton::Right => [0xFF, 0xA2, 0x00],
    };
    let outer = radius + RING_WIDTH;
    let min_x = (center_x - outer).floor().max(0.0) as u32;
    let max_x = (center_x + outer)
        .ceil()
        .min(frame.width.saturating_sub(1) as f32) as u32;
    let min_y = (center_y - outer).floor().max(0.0) as u32;
    let max_y = (center_y + outer)
        .ceil()
        .min(frame.height.saturating_sub(1) as f32) as u32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - center_x;
            let dy = y as f32 + 0.5 - center_y;
            let distance = (dx * dx + dy * dy).sqrt();
            let edge = (distance - radius).abs();
            if edge <= RING_WIDTH {
                let coverage = (1.0 - edge / RING_WIDTH) * alpha;
                blend_bgra(frame, x, y, color, coverage);
            }
        }
    }
}

fn blend_bgra(frame: &mut CaptureFrame, x: u32, y: u32, color: [u8; 3], alpha: f32) {
    let offset = y as usize * frame.stride as usize + x as usize * 4;
    if offset + 3 >= frame.bgra.len() {
        return;
    }
    for (channel, source) in color.into_iter().enumerate() {
        let destination = f32::from(frame.bgra[offset + channel]);
        frame.bgra[offset + channel] =
            (destination * (1.0 - alpha) + f32::from(source) * alpha) as u8;
    }
    frame.bgra[offset + 3] = 0xFF;
}

/// Aspect-fits a BGRA frame into a fixed canvas and fills unused pixels with black.
#[must_use]
pub fn letterbox(frame: &CaptureFrame, target_width: u32, target_height: u32) -> CaptureFrame {
    if frame.width == target_width
        && frame.height == target_height
        && frame.stride == target_width * 4
    {
        return frame.clone();
    }
    let scale = (target_width as f64 / frame.width.max(1) as f64)
        .min(target_height as f64 / frame.height.max(1) as f64);
    let scaled_width = ((frame.width as f64 * scale).round() as u32).clamp(1, target_width);
    let scaled_height = ((frame.height as f64 * scale).round() as u32).clamp(1, target_height);
    let offset_x = (target_width - scaled_width) / 2;
    let offset_y = (target_height - scaled_height) / 2;
    let stride = target_width * 4;
    let mut output = vec![0_u8; stride as usize * target_height as usize];
    for y in 0..scaled_height {
        let source_y = (u64::from(y) * u64::from(frame.height) / u64::from(scaled_height)) as u32;
        for x in 0..scaled_width {
            let source_x = (u64::from(x) * u64::from(frame.width) / u64::from(scaled_width)) as u32;
            let source = source_y as usize * frame.stride as usize + source_x as usize * 4;
            let destination =
                (y + offset_y) as usize * stride as usize + (x + offset_x) as usize * 4;
            if source + 4 <= frame.bgra.len() {
                output[destination..destination + 4]
                    .copy_from_slice(&frame.bgra[source..source + 4]);
            }
        }
    }
    CaptureFrame {
        width: target_width,
        height: target_height,
        stride,
        bgra: output,
        t_ms: frame.t_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, t_ms: u64) -> CaptureFrame {
        CaptureFrame {
            width,
            height,
            stride: width * 4,
            bgra: vec![0; (width * height * 4) as usize],
            t_ms,
        }
    }

    #[test]
    fn outside_window_click_is_not_composited() {
        let mut compositor = RippleCompositor::default();
        let mut target = frame(100, 100, 100);
        compositor.push_events(
            [InputEvent::Click {
                t_ms: 100,
                button: MouseButton::Left,
                x: 250,
                y: 250,
                double: false,
            }],
            ScreenRect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            &target,
        );
        compositor.render(&mut target);
        assert!(target.bgra.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn moved_window_maps_screen_click_to_relative_frame_position() {
        let mut compositor = RippleCompositor::default();
        let mut target = frame(100, 100, 250);
        compositor.push_events(
            [InputEvent::Click {
                t_ms: 200,
                button: MouseButton::Left,
                x: 550,
                y: 350,
                double: false,
            }],
            ScreenRect {
                left: 500,
                top: 300,
                right: 600,
                bottom: 400,
            },
            &target,
        );
        compositor.render(&mut target);
        let changed_near_center = (35..65).any(|y| {
            (35..65).any(|x| {
                target.bgra[(y * target.stride as usize) + x * 4..][..3]
                    .iter()
                    .any(|byte| *byte != 0)
            })
        });
        assert!(changed_near_center);
    }

    #[test]
    fn ripple_expires_after_half_a_second() {
        let mut compositor = RippleCompositor::default();
        let viewport = ScreenRect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let template = frame(100, 100, 0);
        compositor.push_events(
            [InputEvent::Click {
                t_ms: 0,
                button: MouseButton::Right,
                x: 50,
                y: 50,
                double: false,
            }],
            viewport,
            &template,
        );
        let mut expired = frame(100, 100, 501);
        compositor.render(&mut expired);
        assert!(expired.bgra.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn resize_is_letterboxed_to_session_resolution() {
        let mut source = frame(200, 100, 10);
        source.bgra.fill(255);
        let output = letterbox(&source, 100, 100);
        assert_eq!(
            (output.width, output.height, output.stride),
            (100, 100, 400)
        );
        assert!(output.bgra[..20 * 400].iter().all(|byte| *byte == 0));
        assert!(output.bgra[30 * 400..70 * 400].contains(&255));
    }
}
