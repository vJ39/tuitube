//! サムネイルの角へ重ねる、いいね済み/登録済みの小さな印。
//! サムネイル本体の画像とキャッシュには触らず、描くときに Kitty 配置を 1 つ足すだけ。

use crate::engagement::EngagementCache;
use crate::rgb::{self, RgbImage};
use crate::search::SearchResult;
use crate::video::{CellSize, Placement};

/// 印 1 個の 1 辺の上限 (px)。端末が大きなセル寸法を報告しても送出量が伸びないようにする。
const MAX_BADGE_PX: u16 = 24;
/// 枠の色。明るいサムネイルの上でも塗りの輪郭が出るように暗くする。
const BORDER: [u8; 3] = [16, 16, 16];

/// セルの角に出す印。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Liked,
    Subscribed,
}

impl Badge {
    /// 塗り色。再生画面のアクション行 (ui::ActionKind) の赤/緑と揃える。
    fn fill(self) -> [u8; 3] {
        match self {
            Badge::Liked => [220, 40, 40],
            Badge::Subscribed => [40, 190, 90],
        }
    }

    /// list 表示 (サムネイルを描かない) 向けの文字での印。記号は再生画面の
    /// アクション行 (ui::ActionKind) と揃える。
    pub fn symbol(self) -> &'static str {
        match self {
            Badge::Liked => "♥",
            Badge::Subscribed => "＋",
        }
    }
}

/// この行に出す印。左から並べる順。
pub fn badges_for(cache: &EngagementCache, result: &SearchResult) -> Vec<Badge> {
    let mut badges = Vec::new();
    if cache.is_liked(&result.id) {
        badges.push(Badge::Liked);
    }
    // 未確認のチャンネル (None) は未登録と同じに扱い、印を出さない。
    let subscribed = result
        .channel_id
        .as_deref()
        .and_then(|id| cache.is_subscribed(id));
    if subscribed == Some(true) {
        badges.push(Badge::Subscribed);
    }
    badges
}

/// 印 1 個ぶんの画像。端末が c=/r= で 1 セルへ拡大するので、セルの縦横比に寄せた寸法で作る。
pub fn image(badge: Badge, cell: CellSize) -> Option<RgbImage> {
    let width = u32::from(cell.width_px.min(MAX_BADGE_PX));
    let height = u32::from(cell.height_px.min(MAX_BADGE_PX));
    if width == 0 || height == 0 {
        return None;
    }
    let fill = badge.fill();
    // 枠だけで埋まる寸法では色が見えなくなるので、塗りだけにする。
    let framed = width >= 3 && height >= 3;
    let mut pixels = Vec::with_capacity((width * height) as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let edge = framed && (x == 0 || y == 0 || x + 1 == width || y + 1 == height);
            pixels.extend_from_slice(if edge { &BORDER } else { &fill });
        }
    }
    RgbImage::new(width, height, pixels)
}

/// 印を置く場所。サムネイルの左上から右へ 1 セルずつ並べる。
/// 収まらない分は隣のセルを汚すので出さない。
pub fn placements(at: Placement, count: usize) -> Vec<Placement> {
    if at.rows == 0 {
        return Vec::new();
    }
    (0..count.min(usize::from(at.cols)))
        .filter_map(|i| {
            Some(Placement {
                row: at.row,
                col: at.col.checked_add(u16::try_from(i).ok()?)?,
                cols: 1,
                rows: 1,
            })
        })
        .collect()
}

/// 貼ったサムネイル (`at`) の上へ印を追加で送る。サムネイル本体の送出列には手を入れない。
pub fn encode(badges: &[Badge], at: Placement, cell: CellSize, out: &mut Vec<u8>) {
    for (badge, place) in badges.iter().zip(placements(at, badges.len())) {
        if let Some(image) = image(*badge, cell) {
            rgb::encode_image(&image, place, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const CELL: CellSize = CellSize {
        width_px: 8,
        height_px: 16,
    };

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000)
    }

    fn result(id: &str, channel_id: Option<&str>) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
            channel_id: channel_id.map(str::to_string),
        }
    }

    /// サムネイル 3x2 セルぶんの配置。
    fn placed() -> Placement {
        Placement {
            row: 5,
            col: 9,
            cols: 3,
            rows: 2,
        }
    }

    /// 画像の (x, y) の色。
    fn pixel(image: &RgbImage, x: u32, y: u32) -> [u8; 3] {
        let at = ((y * image.width + x) * 3) as usize;
        [image.pixels[at], image.pixels[at + 1], image.pixels[at + 2]]
    }

    #[test]
    fn symbols_differ_between_liked_and_subscribed() {
        assert_ne!(Badge::Liked.symbol(), Badge::Subscribed.symbol());
    }

    #[test]
    fn a_video_that_is_neither_liked_nor_subscribed_gets_no_badge() {
        let cache = EngagementCache::default();

        assert!(badges_for(&cache, &result("v1", Some("UC1"))).is_empty());
    }

    #[test]
    fn a_liked_video_gets_the_like_badge() {
        let mut cache = EngagementCache::default();
        cache.remember_like("v1", true, now());

        assert_eq!(
            badges_for(&cache, &result("v1", None)),
            vec![Badge::Liked],
            "チャンネル ID の無い行でもいいねの印は出す"
        );
        assert!(badges_for(&cache, &result("v2", None)).is_empty());
    }

    #[test]
    fn a_subscribed_channel_gets_the_subscribe_badge() {
        let mut cache = EngagementCache::default();
        cache.remember_subscription("UC1", true, now());

        assert_eq!(
            badges_for(&cache, &result("v1", Some("UC1"))),
            vec![Badge::Subscribed]
        );
    }

    #[test]
    fn an_unconfirmed_or_unsubscribed_channel_gets_no_badge() {
        let mut cache = EngagementCache::default();
        cache.remember_subscription("UC1", false, now());

        assert!(badges_for(&cache, &result("v1", Some("UC1"))).is_empty());
        // 未確認 (問い合わせていない) も印を出さない。
        assert!(badges_for(&cache, &result("v1", Some("UC9"))).is_empty());
    }

    #[test]
    fn a_liked_video_on_a_subscribed_channel_gets_both_badges() {
        let mut cache = EngagementCache::default();
        cache.remember_like("v1", true, now());
        cache.remember_subscription("UC1", true, now());

        assert_eq!(
            badges_for(&cache, &result("v1", Some("UC1"))),
            vec![Badge::Liked, Badge::Subscribed],
            "並びは いいね → 登録"
        );
    }

    #[test]
    fn the_badges_start_at_the_top_left_corner_of_the_thumbnail() {
        let places = placements(placed(), 2);

        assert_eq!(
            places,
            vec![
                Placement {
                    row: 5,
                    col: 9,
                    cols: 1,
                    rows: 1
                },
                Placement {
                    row: 5,
                    col: 10,
                    cols: 1,
                    rows: 1
                },
            ]
        );
    }

    #[test]
    fn badges_that_do_not_fit_the_thumbnail_width_are_dropped() {
        let narrow = Placement {
            cols: 1,
            ..placed()
        };

        assert_eq!(placements(narrow, 2).len(), 1, "隣のセルまではみ出さない");
        assert!(placements(narrow, 0).is_empty());
    }

    #[test]
    fn a_thumbnail_with_no_room_gets_no_badge() {
        for empty in [
            Placement {
                cols: 0,
                ..placed()
            },
            Placement {
                rows: 0,
                ..placed()
            },
        ] {
            assert!(placements(empty, 2).is_empty());
        }
    }

    #[test]
    fn the_badge_image_follows_the_shape_of_a_cell() {
        let image = image(Badge::Liked, CELL).expect("作れる");

        assert_eq!((image.width, image.height), (8, 16), "1 セルに収める");
    }

    #[test]
    fn the_badge_image_has_a_dark_border_around_the_fill() {
        let image = image(Badge::Liked, CELL).expect("作れる");

        assert_eq!(pixel(&image, 0, 0), BORDER);
        assert_eq!(pixel(&image, 7, 15), BORDER);
        assert_eq!(pixel(&image, 4, 8), Badge::Liked.fill());
    }

    #[test]
    fn a_badge_image_too_small_for_a_border_is_all_fill() {
        let tiny = CellSize {
            width_px: 2,
            height_px: 2,
        };
        let image = image(Badge::Subscribed, tiny).expect("作れる");

        // 枠だけになって色が見えなくなるより、塗りだけを出す。
        assert_eq!(pixel(&image, 0, 0), Badge::Subscribed.fill());
    }

    #[test]
    fn the_badge_image_is_bounded_when_the_terminal_reports_a_huge_cell() {
        let huge = CellSize {
            width_px: 400,
            height_px: 900,
        };
        let image = image(Badge::Liked, huge).expect("作れる");

        assert_eq!(
            (image.width, image.height),
            (u32::from(MAX_BADGE_PX), u32::from(MAX_BADGE_PX)),
            "送出量が伸びない寸法で打ち止める"
        );
    }

    #[test]
    fn a_zero_sized_cell_gives_no_badge_image() {
        for broken in [
            CellSize {
                width_px: 0,
                height_px: 16,
            },
            CellSize {
                width_px: 8,
                height_px: 0,
            },
        ] {
            assert!(image(Badge::Liked, broken).is_none());
        }
    }

    #[test]
    fn the_like_and_the_subscribe_badge_use_different_colors() {
        let liked = image(Badge::Liked, CELL).expect("作れる");
        let subscribed = image(Badge::Subscribed, CELL).expect("作れる");

        assert_ne!(pixel(&liked, 4, 8), pixel(&subscribed, 4, 8));
    }

    /// 送出列に入っている画像配置の数。
    fn image_count(out: &[u8]) -> usize {
        out.windows(6).filter(|w| *w == b"\x1b_Ga=T").count()
    }

    #[test]
    fn encoding_writes_one_placement_per_badge() {
        let mut out = Vec::new();
        encode(&[Badge::Liked, Badge::Subscribed], placed(), CELL, &mut out);

        assert_eq!(image_count(&out), 2);
        let text = String::from_utf8_lossy(&out);
        // 左上から右へ 1 セルずつ、1 セル分に拡大させる。
        assert!(text.contains("\x1b[5;9H"), "1 個目の位置が違う");
        assert!(text.contains("\x1b[5;10H"), "2 個目の位置が違う");
        assert_eq!(text.matches("c=1,r=1").count(), 2);
    }

    #[test]
    fn encoding_no_badge_writes_nothing() {
        let mut out = Vec::new();
        encode(&[], placed(), CELL, &mut out);

        assert!(out.is_empty());
    }

    #[test]
    fn encoding_skips_a_badge_that_does_not_fit() {
        let narrow = Placement {
            cols: 1,
            ..placed()
        };
        let mut out = Vec::new();
        encode(&[Badge::Liked, Badge::Subscribed], narrow, CELL, &mut out);

        assert_eq!(image_count(&out), 1);
    }

    #[test]
    fn encoding_with_a_broken_cell_size_writes_nothing() {
        let broken = CellSize {
            width_px: 0,
            height_px: 0,
        };
        let mut out = Vec::new();
        encode(&[Badge::Liked], placed(), broken, &mut out);

        assert!(out.is_empty());
    }
}
