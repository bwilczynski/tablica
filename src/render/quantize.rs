//! Quantization of rendered RGBA frames to the panel's gray levels, and PNG encoding.

use image::{GrayImage, Luma, RgbaImage};

use crate::config::BitDepth;

/// Luma at or above this value turns white in 1-bit output.
const THRESHOLD_1BIT: f32 = 160.0;

/// Composites onto white, takes Rec.601 luma, then snaps to 4 levels
/// (0, 85, 170, 255) at 2-bit or thresholds at 160 at 1-bit. No dithering.
pub fn quantize(rgba: &RgbaImage, depth: BitDepth) -> GrayImage {
    GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let alpha = f32::from(a) / 255.0;
        let luma = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
        let v = luma * alpha + 255.0 * (1.0 - alpha);
        Luma([match depth {
            BitDepth::Two => ((v / 85.0).round() as u8) * 85,
            BitDepth::One => {
                if v < THRESHOLD_1BIT {
                    0
                } else {
                    255
                }
            }
        }])
    })
}

/// Encodes a quantized image as a grayscale PNG packed at the given bit depth.
pub fn encode_png(img: &GrayImage, depth: BitDepth) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let (bits, per_byte, scale) = match depth {
        BitDepth::Two => (png::BitDepth::Two, 4usize, 85u8),
        BitDepth::One => (png::BitDepth::One, 8usize, 255u8),
    };
    let bits_per_px = 8 / per_byte;
    let row_bytes = (w as usize).div_ceil(per_byte);
    let mut data = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let level = img.get_pixel(x as u32, y as u32).0[0] / scale;
            let shift = 8 - bits_per_px * (x % per_byte + 1);
            data[y * row_bytes + x / per_byte] |= level << shift;
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(bits);
    enc.set_compression(png::Compression::High);
    let mut writer = enc.write_header().expect("PNG header into a Vec");
    writer.write_image_data(&data).expect("PNG data into a Vec");
    writer.finish().expect("PNG finish into a Vec");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn px(luma: u8, alpha: u8) -> RgbaImage {
        RgbaImage::from_pixel(1, 1, Rgba([luma, luma, luma, alpha]))
    }

    #[test]
    fn quantize_levels_2bit() {
        for (luma, want) in [(40, 0), (100, 85), (150, 170), (230, 255)] {
            let q = quantize(&px(luma, 255), BitDepth::Two);
            assert_eq!(q.get_pixel(0, 0).0[0], want, "luma {luma}");
        }
        assert_eq!(quantize(&px(0, 0), BitDepth::Two).get_pixel(0, 0).0[0], 255);
    }

    #[test]
    fn quantize_threshold_1bit() {
        assert_eq!(
            quantize(&px(159, 255), BitDepth::One).get_pixel(0, 0).0[0],
            0
        );
        assert_eq!(
            quantize(&px(160, 255), BitDepth::One).get_pixel(0, 0).0[0],
            255
        );
    }

    #[test]
    fn png_roundtrip_bit_depths() {
        for (depth, want) in [
            (BitDepth::Two, png::BitDepth::Two),
            (BitDepth::One, png::BitDepth::One),
        ] {
            let img = GrayImage::from_fn(800, 480, |x, y| {
                Luma([if depth == BitDepth::Two {
                    ((x + y) % 4 * 85) as u8
                } else {
                    ((x + y) % 2 * 255) as u8
                }])
            });
            let bytes = encode_png(&img, depth);
            let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                .read_info()
                .unwrap();
            assert_eq!(reader.info().bit_depth, want);
            assert_eq!((reader.info().width, reader.info().height), (800, 480));
            let mut buf = vec![0; reader.output_buffer_size().unwrap()];
            reader.next_frame(&mut buf).unwrap();
            // First row, first pixels decode back to the expected levels.
            let first = buf[0];
            match depth {
                BitDepth::Two => assert_eq!(first, 0b00_01_10_11),
                BitDepth::One => assert_eq!(first, 0b0101_0101),
            }
        }
    }
}
