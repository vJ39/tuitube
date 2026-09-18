//! JPEG バイト列を RGB へ。サムネイル以外の用途は想定しない。

use crate::rgb::RgbImage;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// 受け付ける元画像の広さ。mqdefault は 320x180、hq720 でも 1280x720。
/// 桁違いの画像を渡されてもメモリを取らないよう、寸法だけで先に断る。
pub const MAX_SOURCE_PIXELS: u64 = 1920 * 1080;

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    Broken(String),
    TooLarge,
}

pub fn decode(bytes: &[u8]) -> Result<RgbImage, DecodeError> {
    decode_within(bytes, MAX_SOURCE_PIXELS)
}

/// 上限を指定して読む。画素を起こす前にヘッダの寸法で断れることを確かめられる形。
pub fn decode_within(bytes: &[u8], max_pixels: u64) -> Result<RgbImage, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|e| DecodeError::Broken(e.to_string()))?;
    let (width, height) = decoder.dimensions().ok_or(DecodeError::TooLarge)?;
    let (width, height) = (width as u64, height as u64);
    if width == 0 || height == 0 || width * height > max_pixels {
        return Err(DecodeError::TooLarge);
    }
    let pixels = decoder
        .decode()
        .map_err(|e| DecodeError::Broken(e.to_string()))?;
    RgbImage::new(width as u32, height as u32, pixels)
        .ok_or_else(|| DecodeError::Broken("画素数と寸法が合いません".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成手順 (ImageMagick 7):
    ///   magick -size 2x2 xc:none -fill '#ff0000' -draw 'point 0,0' -fill '#00ff00' -draw 'point 1,0' \
    ///     -fill '#0000ff' -draw 'point 0,1' -fill '#ffffff' -draw 'point 1,1' \
    ///     -sampling-factor 1x1 -quality 100 tiny2x2.jpg
    const TINY_2X2: &[u8] = include_bytes!("testdata/tiny2x2.jpg");
    ///   magick -size 8x4 gradient:'#ff0000-#0000ff' -sampling-factor 1x1 -quality 95 tiny8x4.jpg
    const TINY_8X4: &[u8] = include_bytes!("testdata/tiny8x4.jpg");
    ///   magick -size 4x4 gradient:'#000000-#ffffff' -colorspace Gray -type Grayscale \
    ///     -sampling-factor 1x1 -quality 95 gray4x4.jpg
    const GRAY_4X4: &[u8] = include_bytes!("testdata/gray4x4.jpg");

    #[test]
    fn decode_returns_rgb_pixels_for_a_known_jpeg() {
        let image = decode(TINY_2X2).expect("読める");
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.pixels.len(), 12);
        // 左上は赤。JPEG は非可逆なので、成分の大小で見る。
        let [r, g, b] = [image.pixels[0], image.pixels[1], image.pixels[2]];
        assert!(r > 200 && g < 80 && b < 80, "{r},{g},{b}");
    }

    #[test]
    fn decode_reads_a_wider_jpeg_at_its_header_size() {
        let image = decode(TINY_8X4).expect("読める");
        assert_eq!((image.width, image.height), (8, 4));
        assert_eq!(image.pixels.len(), 8 * 4 * 3);
    }

    #[test]
    fn decode_expands_grayscale_to_rgb() {
        // components=1 の JPEG。3 バイト/px に展開され、各画素の R=G=B になる。
        let image = decode(GRAY_4X4).expect("読める");
        assert_eq!((image.width, image.height), (4, 4));
        assert_eq!(image.pixels.len(), 4 * 4 * 3);
        for px in image.pixels.chunks(3) {
            assert_eq!(px[0], px[1], "{px:?}");
            assert_eq!(px[1], px[2], "{px:?}");
        }
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert_eq!(decode(&[]), Err(DecodeError::Empty));
    }

    #[test]
    fn decode_rejects_truncated_input_without_panicking() {
        assert!(matches!(
            decode(&TINY_2X2[..20]),
            Err(DecodeError::Broken(_))
        ));
        assert!(matches!(decode(b"not a jpeg"), Err(DecodeError::Broken(_))));
        // 途中で切れた画素データも panic させない。
        let half = TINY_8X4.len() / 2;
        assert!(decode(&TINY_8X4[..half]).is_err());
    }

    #[test]
    fn decode_rejects_an_image_larger_than_the_limit() {
        // 8x4 = 32 px。上限 16 px では画素を起こす前に断る。
        assert_eq!(decode_within(TINY_8X4, 16), Err(DecodeError::TooLarge));
        assert!(decode_within(TINY_8X4, 32).is_ok());
    }
}
