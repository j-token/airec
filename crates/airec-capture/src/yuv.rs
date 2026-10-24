use airec_core::CaptureFrame;

pub(crate) fn even(value: u32) -> u32 {
    value.max(2).next_multiple_of(2)
}

pub(crate) struct I420 {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Converts a frame to top-down planar 4:2:0 with the BT.709 limited-range matrix, averaging
/// each 2x2 block for chroma. Padding added to reach even dimensions is black.
pub(crate) fn i420_bt709(frame: &CaptureFrame, width: u32, height: u32) -> I420 {
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

    let mut luma = vec![0_u8; width * height];
    for y in 0..height {
        for x in 0..width {
            luma[y * width + x] = bt709_luma(rgb_at(x, y));
        }
    }
    let chroma_width = width / 2;
    let chroma_len = chroma_width * (height / 2);
    let mut blue_plane = vec![0_u8; chroma_len];
    let mut red_plane = vec![0_u8; chroma_len];
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
            let [blue, red] = bt709_chroma([average(0), average(1), average(2)]);
            let offset = y / 2 * chroma_width + x / 2;
            blue_plane[offset] = blue;
            red_plane[offset] = red;
        }
    }
    I420 {
        y: luma,
        u: blue_plane,
        v: red_plane,
    }
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

/// Borrowed planes of a decoded 4:2:0 picture.
#[cfg(any(target_os = "linux", test))]
pub(crate) struct PlanarImage<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub strides: (usize, usize, usize),
    pub width: usize,
    pub height: usize,
}

/// Converts BT.709 limited-range 4:2:0 to tightly packed opaque BGRA, the inverse of
/// [`i420_bt709`].
#[cfg(any(target_os = "linux", test))]
pub(crate) fn bgra_from_bt709(image: &PlanarImage) -> Vec<u8> {
    // Inverse BT.709 limited-range coefficients scaled by 2^16.
    const LUMA: i32 = 76_309;
    const RED_FROM_V: i32 = 117_489;
    const GREEN_FROM_U: i32 = 13_975;
    const GREEN_FROM_V: i32 = 34_925;
    const BLUE_FROM_U: i32 = 138_438;
    let clamp = |value: i32| ((value + 32_768) >> 16).clamp(0, 255) as u8;

    let (luma_stride, blue_stride, red_stride) = image.strides;
    let mut bgra = Vec::with_capacity(image.width * image.height * 4);
    for row in 0..image.height {
        for column in 0..image.width {
            let luma = LUMA * (i32::from(image.y[row * luma_stride + column]) - 16);
            let blue_difference = i32::from(image.u[row / 2 * blue_stride + column / 2]) - 128;
            let red_difference = i32::from(image.v[row / 2 * red_stride + column / 2]) - 128;
            bgra.extend([
                clamp(luma + BLUE_FROM_U * blue_difference),
                clamp(luma - GREEN_FROM_U * blue_difference - GREEN_FROM_V * red_difference),
                clamp(luma + RED_FROM_V * red_difference),
                255,
            ]);
        }
    }
    bgra
}

#[cfg(test)]
pub(crate) fn solid_frame(width: u32, height: u32, stride: u32, rgb: [u8; 3]) -> CaptureFrame {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_sizes_are_padded_for_yuv420_without_cropping() {
        assert_eq!(even(1919), 1920);
        assert_eq!(even(1081), 1082);
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
    fn i420_skips_row_padding_and_keeps_top_row_first() {
        let mut frame = solid_frame(2, 2, 12, [255, 255, 255]);
        frame.bgra[12..20].copy_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255]);

        let planes = i420_bt709(&frame, 2, 2);

        assert_eq!(planes.y, [235, 235, 16, 16]);
        assert_eq!(planes.u, [128]);
        assert_eq!(planes.v, [128]);
    }

    #[test]
    fn i420_pads_odd_frames_with_black() {
        let frame = solid_frame(1, 1, 4, [255, 255, 255]);

        let planes = i420_bt709(&frame, 2, 2);

        assert_eq!(planes.y, [235, 16, 16, 16]);
    }

    #[test]
    fn bt709_round_trip_keeps_reference_colors_within_two_code_values() {
        for rgb in [
            [0, 0, 0],
            [255, 255, 255],
            [20, 20, 20],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [124, 58, 237],
        ] {
            let planes = i420_bt709(&solid_frame(2, 2, 8, rgb), 2, 2);

            let bgra = bgra_from_bt709(&PlanarImage {
                y: &planes.y,
                u: &planes.u,
                v: &planes.v,
                strides: (2, 1, 1),
                width: 2,
                height: 2,
            });

            let decoded = [bgra[2], bgra[1], bgra[0]];
            let worst = decoded
                .iter()
                .zip(rgb)
                .map(|(&decoded, expected)| decoded.abs_diff(expected))
                .max()
                .unwrap();
            assert!(worst <= 2, "{rgb:?} decoded as {decoded:?}");
        }
    }
}
