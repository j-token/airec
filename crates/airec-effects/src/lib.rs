//! Pure CPU mouse-effect compositing and resize letterboxing.

use std::collections::VecDeque;

use airec_core::{CaptureFrame, EffectColor, EffectStyles, InputEvent, MouseButton};

const RING_WIDTH: f32 = 3.0;
const MAX_STROKE_SEGMENTS: usize = 4_096;

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

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    started_ms: u64,
    from: Point,
    to: Point,
}

#[derive(Clone, Debug)]
struct DragStroke {
    button: MouseButton,
    segments: VecDeque<Segment>,
    last_point: Option<Point>,
    ended_ms: Option<u64>,
}

pub struct RippleCompositor {
    styles: EffectStyles,
    ripples: Vec<Ripple>,
    drag: Option<DragStroke>,
    trail: VecDeque<Segment>,
    last_trail_point: Option<Point>,
    last_clip_and_mapping: Option<(ScreenRect, ScreenRect)>,
}

impl Default for RippleCompositor {
    fn default() -> Self {
        Self::new(EffectStyles::default())
    }
}

impl RippleCompositor {
    #[must_use]
    pub const fn new(styles: EffectStyles) -> Self {
        Self {
            styles,
            ripples: Vec::new(),
            drag: None,
            trail: VecDeque::new(),
            last_trail_point: None,
            last_clip_and_mapping: None,
        }
    }

    #[must_use]
    pub const fn styles(&self) -> EffectStyles {
        self.styles
    }

    pub fn set_styles(&mut self, styles: EffectStyles) {
        self.styles = styles;
    }

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
        if self.last_clip_and_mapping != Some((clip, mapping)) {
            self.last_trail_point = None;
            if let Some(drag) = &mut self.drag {
                drag.last_point = None;
            }
            self.last_clip_and_mapping = Some((clip, mapping));
        }

        for event in events {
            match event {
                InputEvent::Click {
                    t_ms,
                    button,
                    x,
                    y,
                    double,
                } => {
                    if let Some(point) = map_point(x, y, clip, mapping, frame) {
                        self.ripples.push(Ripple {
                            started_ms: t_ms,
                            x: point.x,
                            y: point.y,
                            button,
                            double,
                        });
                    }
                }
                InputEvent::DragStart {
                    t_ms: _,
                    button,
                    x,
                    y,
                } => {
                    self.drag = map_point(x, y, clip, mapping, frame).map(|point| DragStroke {
                        button,
                        segments: VecDeque::new(),
                        last_point: Some(point),
                        ended_ms: None,
                    });
                }
                InputEvent::Move { t_ms, x, y } => {
                    let point = map_point(x, y, clip, mapping, frame);
                    if let (Some(from), Some(to)) = (self.last_trail_point, point) {
                        push_bounded_segment(
                            &mut self.trail,
                            Segment {
                                started_ms: t_ms,
                                from,
                                to,
                            },
                        );
                    }
                    self.last_trail_point = point;

                    if let Some(drag) = &mut self.drag {
                        if let (Some(from), Some(to)) = (drag.last_point, point) {
                            push_bounded_segment(
                                &mut drag.segments,
                                Segment {
                                    started_ms: t_ms,
                                    from,
                                    to,
                                },
                            );
                        }
                        drag.last_point = point;
                    }
                }
                InputEvent::DragEnd { t_ms, button, x, y } => {
                    if let Some(drag) = &mut self.drag
                        && drag.button == button
                    {
                        let point = map_point(x, y, clip, mapping, frame);
                        if let (Some(from), Some(to)) = (drag.last_point, point) {
                            push_bounded_segment(
                                &mut drag.segments,
                                Segment {
                                    started_ms: t_ms,
                                    from,
                                    to,
                                },
                            );
                        }
                        drag.last_point = point;
                        drag.ended_ms = Some(t_ms);
                    }
                }
            }
        }
    }

    pub fn render(&mut self, frame: &mut CaptureFrame) {
        let now = frame.t_ms;
        let click_style = self.styles.click;
        self.ripples
            .retain(|ripple| now.saturating_sub(ripple.started_ms) <= click_style.duration_ms);
        for ripple in &self.ripples {
            let age = now.saturating_sub(ripple.started_ms);
            if age > click_style.duration_ms {
                continue;
            }
            let progress = age as f32 / click_style.duration_ms.max(1) as f32;
            let radius = 6.0 + click_style.size.max(0.0) * progress;
            let alpha = 1.0 - progress;
            let color = match ripple.button {
                MouseButton::Left => click_style.left_color,
                MouseButton::Right => click_style.right_color,
            };
            draw_ring(frame, ripple.x, ripple.y, radius, color, alpha);
            if ripple.double {
                draw_ring(
                    frame,
                    ripple.x,
                    ripple.y,
                    (radius - 7.0).max(2.0),
                    color,
                    alpha,
                );
            }
        }

        let trail_style = self.styles.trail;
        while self
            .trail
            .front()
            .is_some_and(|segment| now.saturating_sub(segment.started_ms) > trail_style.duration_ms)
        {
            self.trail.pop_front();
        }
        for segment in &self.trail {
            let age = now.saturating_sub(segment.started_ms);
            let alpha = 1.0 - age as f32 / trail_style.duration_ms.max(1) as f32;
            draw_line(
                frame,
                segment.from,
                segment.to,
                trail_style.color,
                trail_style.size,
                alpha,
            );
        }

        let drag_style = self.styles.drag;
        if self.drag.as_ref().is_some_and(|drag| {
            drag.ended_ms
                .is_some_and(|ended| now.saturating_sub(ended) > drag_style.duration_ms)
        }) {
            self.drag = None;
        }
        if let Some(drag) = &self.drag {
            let alpha = drag.ended_ms.map_or(1.0, |ended| {
                1.0 - now.saturating_sub(ended) as f32 / drag_style.duration_ms.max(1) as f32
            });
            for segment in &drag.segments {
                draw_line(
                    frame,
                    segment.from,
                    segment.to,
                    drag_style.color,
                    drag_style.size,
                    alpha,
                );
            }
        }
    }
}

/// Additive name for callers that composite all v0.2 mouse effects.
pub type EffectCompositor = RippleCompositor;

fn map_point(
    x: i32,
    y: i32,
    clip: ScreenRect,
    mapping: ScreenRect,
    frame: &CaptureFrame,
) -> Option<Point> {
    clip.contains(x, y).then(|| Point {
        x: (x - mapping.left) as f32 * frame.width as f32 / mapping.width().max(1) as f32,
        y: (y - mapping.top) as f32 * frame.height as f32 / mapping.height().max(1) as f32,
    })
}

fn push_bounded_segment(segments: &mut VecDeque<Segment>, segment: Segment) {
    if segment.from.x == segment.to.x && segment.from.y == segment.to.y {
        return;
    }
    if segments.len() == MAX_STROKE_SEGMENTS {
        segments.pop_front();
    }
    segments.push_back(segment);
}

fn draw_ring(
    frame: &mut CaptureFrame,
    center_x: f32,
    center_y: f32,
    radius: f32,
    color: EffectColor,
    alpha: f32,
) {
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

fn draw_line(
    frame: &mut CaptureFrame,
    from: Point,
    to: Point,
    color: EffectColor,
    width: f32,
    alpha: f32,
) {
    let radius = width.max(0.0) / 2.0;
    if radius == 0.0 || alpha <= 0.0 || frame.width == 0 || frame.height == 0 {
        return;
    }
    let min_x = (from.x.min(to.x) - radius).floor().max(0.0) as u32;
    let max_x = (from.x.max(to.x) + radius)
        .ceil()
        .min(frame.width.saturating_sub(1) as f32) as u32;
    let min_y = (from.y.min(to.y) - radius).floor().max(0.0) as u32;
    let max_y = (from.y.max(to.y) + radius)
        .ceil()
        .min(frame.height.saturating_sub(1) as f32) as u32;
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let length_squared = dx * dx + dy * dy;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let projection =
                (((px - from.x) * dx + (py - from.y) * dy) / length_squared).clamp(0.0, 1.0);
            let closest_x = from.x + projection * dx;
            let closest_y = from.y + projection * dy;
            let distance = ((px - closest_x).powi(2) + (py - closest_y).powi(2)).sqrt();
            if distance <= radius {
                let coverage = (1.0 - distance / radius).clamp(0.0, 1.0) * alpha;
                blend_bgra(frame, x, y, color, coverage);
            }
        }
    }
}

fn blend_bgra(frame: &mut CaptureFrame, x: u32, y: u32, color: EffectColor, alpha: f32) {
    let offset = y as usize * frame.stride as usize + x as usize * 4;
    if offset + 3 >= frame.bgra.len() {
        return;
    }
    let bgra = [color.blue, color.green, color.red];
    for (channel, source) in bgra.into_iter().enumerate() {
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
    fn custom_click_style_controls_color_size_and_duration() {
        let mut styles = EffectStyles::default();
        styles.click.left_color = EffectColor::rgb(255, 0, 0);
        styles.click.size = 10.0;
        styles.click.duration_ms = 100;
        let mut compositor = RippleCompositor::new(styles);
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
                button: MouseButton::Left,
                x: 50,
                y: 50,
                double: false,
            }],
            viewport,
            &template,
        );
        let mut visible = frame(100, 100, 50);
        compositor.render(&mut visible);
        assert!(
            visible
                .bgra
                .chunks_exact(4)
                .any(|pixel| { pixel[2] > 0 && pixel[0] == 0 && pixel[1] == 0 })
        );

        let mut expired = frame(100, 100, 101);
        compositor.render(&mut expired);
        assert!(expired.bgra.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn drag_trajectory_is_composited_and_expires() {
        let mut styles = EffectStyles::default();
        styles.drag.color = EffectColor::rgb(0, 255, 0);
        styles.drag.size = 8.0;
        styles.drag.duration_ms = 30;
        styles.trail.size = 0.0;
        let mut compositor = RippleCompositor::new(styles);
        let viewport = ScreenRect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let template = frame(100, 100, 0);
        compositor.push_events(
            [
                InputEvent::DragStart {
                    t_ms: 0,
                    button: MouseButton::Left,
                    x: 10,
                    y: 50,
                },
                InputEvent::Move {
                    t_ms: 10,
                    x: 50,
                    y: 50,
                },
                InputEvent::DragEnd {
                    t_ms: 20,
                    button: MouseButton::Left,
                    x: 90,
                    y: 50,
                },
            ],
            viewport,
            &template,
        );
        let mut visible = frame(100, 100, 20);
        compositor.render(&mut visible);
        assert!(
            visible
                .bgra
                .chunks_exact(4)
                .any(|pixel| { pixel[1] > 0 && pixel[0] == 0 && pixel[2] == 0 })
        );

        let mut expired = frame(100, 100, 51);
        compositor.render(&mut expired);
        assert!(expired.bgra.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn outside_moves_break_trail_and_drag_segments() {
        let mut compositor = RippleCompositor::default();
        let viewport = ScreenRect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let template = frame(100, 100, 0);
        compositor.push_events(
            [
                InputEvent::DragStart {
                    t_ms: 0,
                    button: MouseButton::Left,
                    x: 10,
                    y: 50,
                },
                InputEvent::Move {
                    t_ms: 10,
                    x: 110,
                    y: 50,
                },
                InputEvent::Move {
                    t_ms: 20,
                    x: 90,
                    y: 50,
                },
                InputEvent::DragEnd {
                    t_ms: 30,
                    button: MouseButton::Left,
                    x: 90,
                    y: 50,
                },
            ],
            viewport,
            &template,
        );
        let mut target = frame(100, 100, 30);
        compositor.render(&mut target);
        assert!(target.bgra.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn movement_trail_maps_through_moved_viewport() {
        let mut styles = EffectStyles::default();
        styles.trail.color = EffectColor::rgb(0, 0, 255);
        styles.trail.size = 10.0;
        styles.trail.duration_ms = 20;
        let mut compositor = RippleCompositor::new(styles);
        let viewport = ScreenRect {
            left: 500,
            top: 300,
            right: 600,
            bottom: 400,
        };
        let template = frame(200, 200, 0);
        compositor.push_events(
            [
                InputEvent::Move {
                    t_ms: 0,
                    x: 525,
                    y: 350,
                },
                InputEvent::Move {
                    t_ms: 10,
                    x: 575,
                    y: 350,
                },
            ],
            viewport,
            &template,
        );
        let mut target = frame(200, 200, 10);
        compositor.render(&mut target);
        let pixel = &target.bgra[103 * 800 + 100 * 4..][..3];
        assert!(pixel[0] > 0 && pixel[1] == 0 && pixel[2] == 0);

        let mut expired = frame(200, 200, 31);
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
