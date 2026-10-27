//! Conversion between PNG and the device-independent bitmaps (DIBs) of the Windows clipboard
//! (`CF_DIB` / `CF_DIBV5`: a `BITMAPINFOHEADER` or larger header, then the pixels).
//!
//! Plain Rust with no Win32 calls, so the tests run on every platform. Only what clipboards carry
//! in practice is read: uncompressed 24-bit and 32-bit pixels (`BI_RGB`, or `BI_BITFIELDS` with
//! 8-bit channels), bottom-up or top-down. Anything else (palettes, 16-bit, RLE, a JPEG or PNG
//! inside the DIB) is rejected with [`DibError::Unsupported`] instead of guessed at.

use std::io::Cursor;

/// Size of a `BITMAPINFOHEADER`, the smallest header read. Larger headers (`BITMAPV4HEADER`,
/// `BITMAPV5HEADER`) start with the same fields.
const INFO_HEADER_LEN: usize = 40;

/// Size of the header with the RGB masks inside it (`BITMAPV2INFOHEADER` and up).
const RGB_MASKS_HEADER_LEN: usize = 52;

/// Size of the header with the alpha mask inside it too (`BITMAPV3INFOHEADER` and up).
const ALPHA_MASK_HEADER_LEN: usize = 56;

/// `biCompression`: plain pixels in the default channel order (blue, green, red, then unused).
const BI_RGB: u32 = 0;

/// `biCompression`: plain pixels with the channel positions given by masks.
const BI_BITFIELDS: u32 = 3;

/// The PNG signature that every PNG starts with.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, thiserror::Error)]
pub enum DibError {
    /// A DIB this module does not read (see module docs), or a malformed one.
    #[error("unsupported bitmap: {0}")]
    Unsupported(&'static str),
    #[error("could not encode the image as PNG")]
    Encode(#[from] png::EncodingError),
    #[error("could not decode the PNG")]
    Decode(#[from] png::DecodingError),
}

/// Converts a DIB to PNG. The PNG is RGB when every pixel is opaque, RGBA otherwise.
///
/// The fourth byte of a 32-bit `BI_RGB` pixel is officially unused, but apps put alpha there.
/// It is read as alpha unless it is 0 for every pixel, which means it was really unused (an
/// all-transparent image would be invisible anyway).
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, DibError> {
    let format = Format::parse(dib).map_err(DibError::Unsupported)?;
    let mut pixels: Vec<[u8; 4]> = Vec::with_capacity(format.width * format.rows);
    for row in 0..format.rows {
        let source_row = if format.top_down {
            row
        } else {
            format.rows - 1 - row
        };
        let start = format.pixels_offset + source_row * format.stride;
        // In bounds: `Format::parse` checked that every row fits.
        let line = &dib[start..start + format.width * format.bytes_per_pixel];
        pixels.extend(line.chunks_exact(format.bytes_per_pixel).map(|pixel| {
            let mut word = [0; 4];
            word[..pixel.len()].copy_from_slice(pixel);
            let value = u32::from_le_bytes(word);
            let alpha = format.alpha.map_or(u8::MAX, |alpha| alpha.of(value));
            [
                format.red.of(value),
                format.green.of(value),
                format.blue.of(value),
                alpha,
            ]
        }));
    }
    if pixels.iter().all(|pixel| pixel[3] == 0) {
        for pixel in &mut pixels {
            pixel[3] = u8::MAX;
        }
    }

    let (color, samples) = if pixels.iter().all(|pixel| pixel[3] == u8::MAX) {
        let rgb = pixels
            .iter()
            .flat_map(|pixel| &pixel[..3])
            .copied()
            .collect();
        (png::ColorType::Rgb, rgb)
    } else {
        (png::ColorType::Rgba, pixels.into_flattened())
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, format.width_u32, format.rows_u32);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    // Several times faster than the default for a slightly bigger file; a screenshot must
    // convert well within the backend's time limit.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&samples)?;
    writer.finish()?;
    Ok(out)
}

/// Converts a PNG to a `CF_DIB`: a `BITMAPINFOHEADER` and 32-bit bottom-up `BI_RGB` pixels with
/// alpha in the fourth byte. Bottom-up because some apps mishandle top-down DIBs.
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, DibError> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    // 8 bits per sample, palettes expanded: the output is gray, gray + alpha, RGB or RGBA.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let too_large = || DibError::Unsupported("the image is too large");
    let mut buffer = vec![0; reader.output_buffer_size().ok_or_else(too_large)?];
    let info = reader.next_frame(&mut buffer)?;
    let width = i32::try_from(info.width).map_err(|_| too_large())?;
    let height = i32::try_from(info.height).map_err(|_| too_large())?;
    let image_size = info
        .width
        .checked_mul(info.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(too_large)?;

    let mut dib =
        Vec::with_capacity(INFO_HEADER_LEN + usize::try_from(image_size).map_err(|_| too_large())?);
    dib.extend(40_u32.to_le_bytes()); // header size: `INFO_HEADER_LEN`
    dib.extend(width.to_le_bytes());
    // Positive: bottom-up.
    dib.extend(height.to_le_bytes());
    dib.extend(1_u16.to_le_bytes()); // planes
    dib.extend(32_u16.to_le_bytes()); // bits per pixel
    dib.extend(BI_RGB.to_le_bytes());
    dib.extend(image_size.to_le_bytes());
    // Resolution (unknown), palette size and important colors (none).
    dib.extend([0; 16]);

    let samples = info.color_type.samples();
    // At 8 bits per sample a line is exactly `width * samples` bytes.
    for line in buffer[..info.buffer_size()]
        .chunks_exact(info.line_size)
        .rev()
    {
        for pixel in line.chunks_exact(samples) {
            let (red, green, blue, alpha) = match *pixel {
                [gray] => (gray, gray, gray, u8::MAX),
                [gray, alpha] => (gray, gray, gray, alpha),
                [red, green, blue] => (red, green, blue, u8::MAX),
                [red, green, blue, alpha] => (red, green, blue, alpha),
                _ => return Err(DibError::Unsupported("an unexpected PNG sample layout")),
            };
            dib.extend([blue, green, red, alpha]);
        }
    }
    Ok(dib)
}

/// The length of the PNG at the start of `bytes`: up to the end of its `IEND` chunk. `None` if
/// `bytes` is not a PNG or is cut short. A Windows clipboard memory block may be bigger than the
/// data put in it, so the end has to be found to return the exact bytes.
pub fn png_len(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let mut offset = PNG_SIGNATURE.len();
    loop {
        // Each chunk: data length (4 bytes, big-endian), type (4), data, CRC (4).
        let (len, kind) = bytes.get(offset..)?.first_chunk::<8>()?.split_at(4);
        let len = usize::try_from(u32::from_be_bytes(len.try_into().ok()?)).ok()?;
        let end = offset.checked_add(12)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        if kind == b"IEND" {
            return Some(end);
        }
        offset = end;
    }
}

/// Where a DIB's pixels are and how to read them.
#[derive(Debug)]
struct Format {
    width: usize,
    width_u32: u32,
    rows: usize,
    rows_u32: u32,
    top_down: bool,
    bytes_per_pixel: usize,
    /// Bytes per row, padding included.
    stride: usize,
    pixels_offset: usize,
    red: Channel,
    green: Channel,
    blue: Channel,
    alpha: Option<Channel>,
}

impl Format {
    /// Reads the header. On failure, the reason the DIB is not read.
    fn parse(dib: &[u8]) -> Result<Self, &'static str> {
        const TOO_LARGE: &str = "the size does not fit in memory";
        let u32_at = |offset: usize| {
            dib.get(offset..)
                .and_then(<[u8]>::first_chunk::<4>)
                .map(|bytes| u32::from_le_bytes(*bytes))
        };
        let header_len = u32_at(0).ok_or("the header is cut short")?;
        let header_len = usize::try_from(header_len)
            .ok()
            .filter(|&len| len >= INFO_HEADER_LEN && len <= dib.len())
            .ok_or("an unknown header")?;
        // The whole header is in bounds from here on, so its fields are too.
        let field = |offset| u32_at(offset).unwrap_or_default();
        let width = field(4).cast_signed();
        let height = field(8).cast_signed();
        // `biBitCount` is a 16-bit field.
        let bit_count = field(14) & 0xFFFF;
        let compression = field(16);
        let palette_len = field(32);

        let (bytes_per_pixel, masks_after_header, [red, green, blue, alpha]) =
            match (compression, bit_count) {
                (BI_RGB, 24) => (3, 0, [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0]),
                (BI_RGB, 32) => (4, 0, [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000]),
                (BI_BITFIELDS, 32) if header_len >= RGB_MASKS_HEADER_LEN => {
                    let alpha = if header_len >= ALPHA_MASK_HEADER_LEN {
                        field(52)
                    } else {
                        0
                    };
                    (4, 0, [field(40), field(44), field(48), alpha])
                }
                // A plain `BITMAPINFOHEADER` keeps the three masks right after it.
                (BI_BITFIELDS, 32) => {
                    let mask = |index: usize| u32_at(INFO_HEADER_LEN + 4 * index);
                    let (Some(red), Some(green), Some(blue)) = (mask(0), mask(1), mask(2)) else {
                        return Err("the header is cut short");
                    };
                    (4, 12, [red, green, blue, 0])
                }
                (BI_RGB | BI_BITFIELDS, _) => return Err("not 24 or 32 bits per pixel"),
                _ => return Err("compressed pixels"),
            };

        let width_u32 = u32::try_from(width)
            .ok()
            .filter(|&w| w > 0)
            .ok_or("no pixels")?;
        let rows_u32 = height.unsigned_abs();
        if rows_u32 == 0 {
            return Err("no pixels");
        }
        let width = usize::try_from(width_u32).map_err(|_| TOO_LARGE)?;
        let rows = usize::try_from(rows_u32).map_err(|_| TOO_LARGE)?;
        // Rows are padded to a multiple of 4 bytes.
        let stride = width
            .checked_mul(bytes_per_pixel)
            .and_then(|len| len.checked_next_multiple_of(4))
            .ok_or(TOO_LARGE)?;
        // An optional palette may sit between the header (and masks) and the pixels.
        let pixels_offset = usize::try_from(palette_len)
            .ok()
            .and_then(|len| len.checked_mul(4))
            .and_then(|len| len.checked_add(header_len + masks_after_header))
            .ok_or(TOO_LARGE)?;
        let pixels_end = stride
            .checked_mul(rows)
            .and_then(|len| len.checked_add(pixels_offset));
        if pixels_end.is_none_or(|end| end > dib.len()) {
            return Err("the pixels are cut short");
        }

        Ok(Self {
            width,
            width_u32,
            rows,
            rows_u32,
            top_down: height < 0,
            bytes_per_pixel,
            stride,
            pixels_offset,
            red: Channel::from_mask(red)?,
            green: Channel::from_mask(green)?,
            blue: Channel::from_mask(blue)?,
            alpha: (alpha != 0)
                .then(|| Channel::from_mask(alpha))
                .transpose()?,
        })
    }
}

/// An 8-bit color channel inside a pixel's little-endian value.
#[derive(Debug, Clone, Copy)]
struct Channel {
    shift: u32,
}

impl Channel {
    fn from_mask(mask: u32) -> Result<Self, &'static str> {
        let shift = mask.trailing_zeros();
        if mask.checked_shr(shift) == Some(0xFF) {
            Ok(Self { shift })
        } else {
            Err("a color channel that is not 8 bits")
        }
    }

    fn of(self, pixel: u32) -> u8 {
        (pixel >> self.shift).to_le_bytes()[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const WHITE: [u8; 3] = [255, 255, 255];

    /// A `BITMAPINFOHEADER`.
    fn info_header(width: i32, height: i32, bits: u16, compression: u32, palette: u32) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend(40_u32.to_le_bytes());
        header.extend(width.to_le_bytes());
        header.extend(height.to_le_bytes());
        header.extend(1_u16.to_le_bytes());
        header.extend(bits.to_le_bytes());
        header.extend(compression.to_le_bytes());
        header.extend([0; 12]); // image size (may be 0 for BI_RGB), resolution
        header.extend(palette.to_le_bytes());
        header.extend([0; 4]); // important colors
        header
    }

    /// A `BITMAPV5HEADER` with `BI_BITFIELDS` and these masks, as `CF_DIBV5` carries.
    fn v5_header(width: i32, height: i32, [red, green, blue, alpha]: [u32; 4]) -> Vec<u8> {
        let mut header = info_header(width, height, 32, BI_BITFIELDS, 0);
        header[..4].copy_from_slice(&124_u32.to_le_bytes());
        for mask in [red, green, blue, alpha] {
            header.extend(mask.to_le_bytes());
        }
        header.resize(124, 0); // color space, endpoints, gamma, intent, profile
        header
    }

    /// Decodes a PNG into its color type, size and samples.
    fn decode(png: &[u8]) -> (png::ColorType, u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(Cursor::new(png))
            .read_info()
            .expect("valid PNG");
        let mut samples = vec![0; reader.output_buffer_size().expect("small image")];
        let info = reader.next_frame(&mut samples).expect("decode");
        samples.truncate(info.buffer_size());
        (info.color_type, info.width, info.height, samples)
    }

    #[test]
    fn reads_a_24_bit_bottom_up_dib_with_row_padding() {
        // 2x2: rows of 6 bytes padded to 8, stored bottom row first, pixels as BGR.
        let mut dib = info_header(2, 2, 24, BI_RGB, 0);
        dib.extend([255, 0, 0, 255, 255, 255, 0, 0]); // bottom: blue, white
        dib.extend([0, 0, 255, 0, 255, 0, 0, 0]); // top: red, green

        let (color, width, height, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!((color, width, height), (png::ColorType::Rgb, 2, 2));
        assert_eq!(samples, [RED, GREEN, BLUE, WHITE].concat());
    }

    #[test]
    fn reads_a_32_bit_top_down_dib_whose_fourth_byte_is_unused() {
        // Negative height: top row first. All fourth bytes 0: opaque, so RGB.
        let mut dib = info_header(2, -2, 32, BI_RGB, 0);
        dib.extend([0, 0, 255, 0, 0, 255, 0, 0]); // top: red, green
        dib.extend([255, 0, 0, 0, 255, 255, 255, 0]); // bottom: blue, white

        let (color, _, _, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!(color, png::ColorType::Rgb);
        assert_eq!(samples, [RED, GREEN, BLUE, WHITE].concat());
    }

    #[test]
    fn keeps_alpha_from_the_fourth_byte() {
        let mut dib = info_header(2, 1, 32, BI_RGB, 0);
        dib.extend([0, 0, 255, 128, 0, 255, 0, 255]); // half-transparent red, green

        let (color, _, _, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!(color, png::ColorType::Rgba);
        assert_eq!(samples, [255, 0, 0, 128, 0, 255, 0, 255]);
    }

    #[test]
    fn reads_bitfields_after_an_info_header_and_inside_a_v5_header() {
        // Masks after a plain header, in RGBA byte order (red in the lowest byte).
        let mut dib = info_header(1, 1, 32, BI_BITFIELDS, 0);
        for mask in [0x0000_00FF_u32, 0x0000_FF00, 0x00FF_0000] {
            dib.extend(mask.to_le_bytes());
        }
        dib.extend([10, 20, 30, 0]);
        let (_, _, _, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!(samples, [10, 20, 30]);

        // What Windows and browsers put in `CF_DIBV5`: BGRA with an alpha mask.
        let mut dib = v5_header(1, 1, [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000]);
        dib.extend([30, 20, 10, 40]);
        let (color, _, _, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!(color, png::ColorType::Rgba);
        assert_eq!(samples, [10, 20, 30, 40]);
    }

    #[test]
    fn skips_an_optional_palette_before_the_pixels() {
        let mut dib = info_header(1, 1, 24, BI_RGB, 2);
        dib.extend([9; 8]); // two palette entries, not used by 24-bit pixels
        dib.extend([0, 0, 255, 0]);
        let (_, _, _, samples) = decode(&dib_to_png(&dib).expect("convert"));
        assert_eq!(samples, RED);
    }

    #[test]
    fn rejects_dibs_it_does_not_read() {
        let unsupported = |dib: &[u8]| matches!(dib_to_png(dib), Err(DibError::Unsupported(_)));

        let mut paletted = info_header(1, 1, 8, BI_RGB, 1);
        paletted.extend([0; 8]);
        assert!(unsupported(&paletted), "8-bit palette");

        let mut rle = info_header(1, 1, 8, 1, 0);
        rle.extend([0; 4]);
        assert!(unsupported(&rle), "RLE");

        let mut rgb565 = v5_header(1, 1, [0xF800, 0x07E0, 0x001F, 0]);
        rgb565.extend([0; 4]);
        assert!(unsupported(&rgb565), "5- and 6-bit channels");

        let mut short = info_header(2, 2, 32, BI_RGB, 0);
        short.extend([0; 12]);
        assert!(unsupported(&short), "pixels cut short");

        assert!(unsupported(&info_header(0, 1, 32, BI_RGB, 0)), "no pixels");
        assert!(unsupported(&[40, 0, 0]), "header cut short");
        assert!(unsupported(&[]), "empty");
    }

    #[test]
    fn a_png_round_trips_through_a_dib() {
        let mut dib = info_header(2, 1, 32, BI_RGB, 0);
        dib.extend([0, 0, 255, 128, 0, 255, 0, 255]);
        let png = dib_to_png(&dib).expect("to PNG");

        let back = png_to_dib(&png).expect("to DIB");
        let mut header = info_header(2, 1, 32, BI_RGB, 0);
        header[20..24].copy_from_slice(&8_u32.to_le_bytes()); // image size, filled in
        assert_eq!(back[..40], header);
        assert_eq!(back[40..], dib[40..]);
        assert_eq!(dib_to_png(&back).expect("to PNG again"), png);
    }

    #[test]
    fn writes_gray_pngs_as_bottom_up_bgra() {
        // A 1x2 gray + alpha PNG: top pixel black, bottom white half-transparent.
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 1, 2);
        encoder.set_color(png::ColorType::GrayscaleAlpha);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&[0, 255, 255, 128]).expect("data");
        writer.finish().expect("finish");

        let dib = png_to_dib(&png).expect("to DIB");
        assert_eq!(dib[40..], [255, 255, 255, 128, 0, 0, 0, 255]);
    }

    #[test]
    fn finds_the_end_of_a_png() {
        let mut dib = info_header(1, 1, 24, BI_RGB, 0);
        dib.extend([0; 4]);
        let png = dib_to_png(&dib).expect("to PNG");
        assert_eq!(png_len(&png), Some(png.len()));

        let mut padded = png.clone();
        padded.extend([0; 7]);
        assert_eq!(png_len(&padded), Some(png.len()));

        assert_eq!(png_len(&png[..png.len() - 1]), None);
        assert_eq!(png_len(b"GIF89a"), None);
    }
}
