//! RGB 画像の縮小と、Kitty graphics protocol への載せ方。外部依存なし。

use crate::video::Placement;

/// APC 1 個のペイロード上限。継続チャンクはこの単位で切る。
const APC_CHUNK: usize = 4096;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    /// 3 バイト/px。長さは width*height*3。
    pub pixels: Vec<u8>,
}

impl RgbImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let image = Self {
            width,
            height,
            pixels,
        };
        image.is_consistent().then_some(image)
    }

    fn is_consistent(&self) -> bool {
        self.expected_len() == Some(self.pixels.len())
    }

    fn expected_len(&self) -> Option<usize> {
        let pixels = u64::from(self.width).checked_mul(u64::from(self.height))?;
        usize::try_from(pixels.checked_mul(3)?).ok()
    }
}

/// アスペクトを保ったまま箱に内接する寸法。既に収まっていればそのまま返す。
pub fn fit_box(src: (u32, u32), box_px: (u32, u32)) -> (u32, u32) {
    if src.0 == 0 || src.1 == 0 || box_px.0 == 0 || box_px.1 == 0 {
        return (0, 0);
    }
    if src.0 <= box_px.0 && src.1 <= box_px.1 {
        return src;
    }
    let (sw, sh) = (u64::from(src.0), u64::from(src.1));
    let (bw, bh) = (u64::from(box_px.0), u64::from(box_px.1));
    // 高さと幅のどちらが先に箱に当たるかを、割り算を使わず交差積で決める。
    let (w, h) = if sw * bh <= bw * sh {
        (sw * bh / sh, bh)
    } else {
        (bw, sh * bw / sw)
    };
    (w.max(1) as u32, h.max(1) as u32)
}

/// 縮小のみ。出力 1 px は対応する入力矩形の平均。dst が src 以上なら複製を返す。
pub fn shrink(src: &RgbImage, dst_w: u32, dst_h: u32) -> Option<RgbImage> {
    if dst_w == 0 || dst_h == 0 || !src.is_consistent() || src.width == 0 || src.height == 0 {
        return None;
    }
    if dst_w >= src.width && dst_h >= src.height {
        return Some(src.clone());
    }
    let dst_w = dst_w.min(src.width);
    let dst_h = dst_h.min(src.height);
    let mut pixels = Vec::with_capacity((dst_w as usize) * (dst_h as usize) * 3);
    for y in 0..dst_h {
        let y0 = (u64::from(y) * u64::from(src.height) / u64::from(dst_h)) as u32;
        let y1 = ((u64::from(y) + 1) * u64::from(src.height) / u64::from(dst_h)) as u32;
        for x in 0..dst_w {
            let x0 = (u64::from(x) * u64::from(src.width) / u64::from(dst_w)) as u32;
            let x1 = ((u64::from(x) + 1) * u64::from(src.width) / u64::from(dst_w)) as u32;
            let mut sum = [0u64; 3];
            let mut count = 0u64;
            for sy in y0..y1.max(y0 + 1) {
                for sx in x0..x1.max(x0 + 1) {
                    let at = ((sy as usize) * (src.width as usize) + sx as usize) * 3;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u64::from(src.pixels[at + channel]);
                    }
                    count += 1;
                }
            }
            for total in sum {
                pixels.push((total / count) as u8);
            }
        }
    }
    RgbImage::new(dst_w, dst_h, pixels)
}

/// RFC 4648 標準アルファベット。out へ追記する (既存の内容は消さない)。
pub fn base64_into(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let packed = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let index = |shift: u32| BASE64[((packed >> shift) & 0x3f) as usize] as char;
        out.push(index(18));
        out.push(index(12));
        out.push(if chunk.len() > 1 { index(6) } else { '=' });
        out.push(if chunk.len() > 2 { index(0) } else { '=' });
    }
}

/// CUP で左上へ寄せてから、raw RGB を base64 でチャンク分割して送る。
/// 先頭チャンクだけが制御キーを持ち、c=/r= で端末側にセル矩形へ拡大させる。
pub fn encode_image(image: &RgbImage, at: Placement, out: &mut Vec<u8>) {
    if image.width == 0 || image.height == 0 || !image.is_consistent() {
        return;
    }
    let mut payload = String::new();
    base64_into(&image.pixels, &mut payload);
    out.extend_from_slice(format!("\x1b[{};{}H", at.row, at.col).as_bytes());
    let chunks = payload.as_bytes().chunks(APC_CHUNK);
    let last = payload.len().div_ceil(APC_CHUNK).saturating_sub(1);
    for (i, chunk) in chunks.enumerate() {
        let more = u8::from(i != last);
        let head = if i == 0 {
            format!(
                "\x1b_Ga=T,f=24,s={},v={},C=1,q=2,c={},r={},m={more};",
                image.width, image.height, at.cols, at.rows
            )
        } else {
            format!("\x1b_Gm={more};")
        };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(crate::kitty::ST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::{ApcParser, FrameAssembler, FrameEvent};

    const BLACK: [u8; 3] = [0, 0, 0];
    const WHITE: [u8; 3] = [255, 255, 255];

    fn b64(bytes: &[u8]) -> String {
        let mut out = String::new();
        base64_into(bytes, &mut out);
        out
    }

    /// 市松模様。(x+y) が偶数なら黒。
    fn checkerboard(width: u32, height: u32) -> RgbImage {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let color = if (x + y) % 2 == 0 { BLACK } else { WHITE };
                pixels.extend_from_slice(&color);
            }
        }
        RgbImage::new(width, height, pixels).expect("長さは合っている")
    }

    fn placement() -> Placement {
        Placement {
            row: 3,
            col: 5,
            cols: 1,
            rows: 1,
        }
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64(input.as_bytes()), expected, "{input}");
        }
    }

    #[test]
    fn base64_handles_non_ascii_bytes() {
        assert_eq!(b64(&[0x00, 0xff, 0x80]), "AP+A");
        assert_eq!(b64(&[0xff, 0xff, 0xff]), "////");
    }

    #[test]
    fn base64_appends_without_clearing_the_buffer() {
        let mut out = "head:".to_string();
        base64_into(b"foo", &mut out);
        base64_into(b"bar", &mut out);
        assert_eq!(out, "head:Zm9vYmFy");
    }

    #[test]
    fn fit_box_keeps_aspect_and_fits_inside() {
        let fitted = fit_box((320, 180), (144, 80));
        assert_eq!(fitted, (142, 80));
        assert!(fitted.0 <= 144 && fitted.1 <= 80);

        // 幅が先に当たる箱。
        assert_eq!(fit_box((320, 180), (80, 200)), (80, 45));
    }

    #[test]
    fn fit_box_returns_the_source_when_it_already_fits() {
        assert_eq!(fit_box((32, 18), (144, 80)), (32, 18));
        assert_eq!(fit_box((144, 80), (144, 80)), (144, 80));
    }

    #[test]
    fn fit_box_is_zero_for_a_degenerate_input() {
        assert_eq!(fit_box((0, 180), (144, 80)), (0, 0));
        assert_eq!(fit_box((320, 0), (144, 80)), (0, 0));
        assert_eq!(fit_box((320, 180), (0, 80)), (0, 0));
        assert_eq!(fit_box((320, 180), (144, 0)), (0, 0));
    }

    #[test]
    fn shrink_averages_the_source_area() {
        // 市松が潰れて灰色になる。最近傍だと 0 か 255 のどちらかに寄る。
        let shrunk = shrink(&checkerboard(2, 2), 1, 1).expect("縮小できる");
        assert_eq!(shrunk.width, 1);
        assert_eq!(shrunk.height, 1);
        assert_eq!(shrunk.pixels, [127, 127, 127]);
    }

    #[test]
    fn shrink_halves_a_4x4_checkerboard_into_2x2() {
        let shrunk = shrink(&checkerboard(4, 4), 2, 2).expect("縮小できる");
        assert_eq!((shrunk.width, shrunk.height), (2, 2));
        assert_eq!(shrunk.pixels, [127; 12]);
    }

    #[test]
    fn shrink_copies_when_the_target_is_not_smaller() {
        let src = checkerboard(2, 2);
        assert_eq!(shrink(&src, 2, 2), Some(src.clone()));
        assert_eq!(shrink(&src, 8, 8), Some(src));
    }

    #[test]
    fn shrink_rejects_a_zero_sized_target() {
        let src = checkerboard(4, 4);
        assert_eq!(shrink(&src, 0, 2), None);
        assert_eq!(shrink(&src, 2, 0), None);
    }

    #[test]
    fn shrink_rejects_a_pixel_buffer_whose_length_does_not_match() {
        let broken = RgbImage {
            width: 4,
            height: 4,
            pixels: vec![0; 10],
        };
        assert_eq!(shrink(&broken, 2, 2), None);
        assert_eq!(RgbImage::new(4, 4, vec![0; 10]), None);
        assert!(RgbImage::new(4, 4, vec![0; 48]).is_some());
    }

    #[test]
    fn encode_image_writes_cup_then_a_single_apc_for_a_small_image() {
        let image = checkerboard(2, 2);
        let mut out = Vec::new();
        encode_image(&image, placement(), &mut out);

        let mut expected = b"\x1b[3;5H\x1b_Ga=T,f=24,s=2,v=2,C=1,q=2,c=1,r=1,m=0;".to_vec();
        expected.extend_from_slice(b64(&image.pixels).as_bytes());
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(out, expected);
    }

    #[test]
    fn encode_image_always_carries_q2() {
        // q=2 が無いと端末の応答が tuitube の stdin に入りキーイベントとして誤読される。
        let mut out = Vec::new();
        encode_image(&checkerboard(64, 64), placement(), &mut out);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("q=2"), "{}", &text[..80]);
    }

    #[test]
    fn encode_image_splits_the_payload_into_4096_byte_chunks() {
        // 64x64 px = 12288 バイト → base64 16384 バイト → 4 チャンク。
        let mut out = Vec::new();
        encode_image(&checkerboard(64, 64), placement(), &mut out);

        let heads: Vec<&str> = std::str::from_utf8(&out)
            .expect("base64 と制御列だけ")
            .split("\x1b_G")
            .skip(1)
            .map(|part| part.split(';').next().expect("制御部"))
            .collect();
        assert_eq!(
            heads,
            [
                "a=T,f=24,s=64,v=64,C=1,q=2,c=1,r=1,m=1",
                "m=1",
                "m=1",
                "m=0",
            ]
        );
    }

    #[test]
    fn encode_image_round_trips_through_the_existing_parser() {
        // 検証済みの読み取り側へ食わせて、寸法が往復することを確かめる。
        let image = checkerboard(40, 24);
        let mut out = Vec::new();
        encode_image(&image, placement(), &mut out);

        let mut parser = ApcParser::default();
        let mut commands = Vec::new();
        parser.feed(&out, &mut commands);
        let mut assembler = FrameAssembler::new(u64::from(image.width) * u64::from(image.height));
        let events: Vec<FrameEvent> = commands
            .into_iter()
            .filter_map(|c| assembler.push(c))
            .collect();

        assert_eq!(events.len(), 1);
        let FrameEvent::Frame(frame) = &events[0] else {
            panic!("フレームになっていない");
        };
        assert_eq!((frame.width_px, frame.height_px), (40, 24));
    }

    #[test]
    fn encode_image_writes_nothing_for_a_degenerate_image() {
        let mut out = Vec::new();
        encode_image(
            &RgbImage {
                width: 0,
                height: 2,
                pixels: Vec::new(),
            },
            placement(),
            &mut out,
        );
        encode_image(
            &RgbImage {
                width: 2,
                height: 2,
                pixels: vec![0; 3],
            },
            placement(),
            &mut out,
        );
        assert!(out.is_empty());
    }
}
