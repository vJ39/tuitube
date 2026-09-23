//! 検索欄に貼られた YouTube の URL の読み取り。動画かプレイリストを指していれば ID を返す。

/// youtu.be 以外で受け付けるホスト。
const YOUTUBE_HOSTS: [&str; 4] = [
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
];
const SHORT_HOST: &str = "youtu.be";
/// 動画 ID の長さ。
const VIDEO_ID_LEN: usize = 11;
/// プレイリスト ID の長さの上限。長すぎるものは URL の読み違いとして扱う。
const PLAYLIST_ID_MAX: usize = 64;

/// URL をホスト・パス・クエリに分けたもの。
struct Parts<'a> {
    host: &'a str,
    path: &'a str,
    query: &'a str,
}

fn split(text: &str) -> Parts<'_> {
    let rest = text.trim();
    let rest = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))
        .unwrap_or(rest);
    let rest = rest.split('#').next().unwrap_or_default();
    let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = before_query.split_once('/').unwrap_or((before_query, ""));
    Parts { host, path, query }
}

fn is_youtube_host(host: &str) -> bool {
    YOUTUBE_HOSTS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(host))
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, value)| value)
}

/// ID に使われる文字。
fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn is_video_id(id: &str) -> bool {
    id.len() == VIDEO_ID_LEN && id.chars().all(is_id_char)
}

/// 動画の URL なら動画 ID。
pub fn video_id(text: &str) -> Option<String> {
    let parts = split(text);
    let id = if parts.host.eq_ignore_ascii_case(SHORT_HOST) {
        parts.path.split('/').next()
    } else if is_youtube_host(parts.host) {
        let segments: Vec<&str> = parts.path.split('/').collect();
        match segments.as_slice() {
            ["watch"] => query_value(parts.query, "v"),
            ["shorts" | "live" | "embed", id, ..] => Some(*id),
            _ => None,
        }
    } else {
        None
    }?;
    is_video_id(id).then(|| id.to_string())
}

/// プレイリストの URL ならプレイリスト ID。動画の URL に付いた list= は見ない。
pub fn playlist_id(text: &str) -> Option<String> {
    let parts = split(text);
    if !is_youtube_host(parts.host) || parts.path != "playlist" {
        return None;
    }
    let id = query_value(parts.query, "list")?;
    let valid = !id.is_empty() && id.len() <= PLAYLIST_ID_MAX && id.chars().all(is_id_char);
    valid.then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_url_gives_the_video_id() {
        for url in [
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "http://youtube.com/watch?v=jNQXAC9IVRw",
            "www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://m.youtube.com/watch?v=jNQXAC9IVRw",
            "https://music.youtube.com/watch?v=jNQXAC9IVRw",
            "https://WWW.YouTube.com/watch?v=jNQXAC9IVRw",
            "https://www.youtube.com/watch?feature=share&v=jNQXAC9IVRw",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw&t=42s",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PLabc",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw#comments",
            "  https://www.youtube.com/watch?v=jNQXAC9IVRw  ",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("jNQXAC9IVRw"), "{url}");
        }
    }

    #[test]
    fn the_short_and_path_forms_give_the_video_id() {
        for url in [
            "https://youtu.be/jNQXAC9IVRw",
            "youtu.be/jNQXAC9IVRw?t=10",
            "https://www.youtube.com/shorts/jNQXAC9IVRw",
            "https://youtube.com/shorts/jNQXAC9IVRw?feature=share",
            "https://www.youtube.com/live/jNQXAC9IVRw",
            "https://www.youtube.com/embed/jNQXAC9IVRw",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("jNQXAC9IVRw"), "{url}");
        }
    }

    #[test]
    fn anything_else_is_not_a_video() {
        for text in [
            // ID だけ・ただの語句は検索語のまま。
            "jNQXAC9IVRw",
            "猫 動画",
            // 別のサイト。
            "https://example.com/watch?v=jNQXAC9IVRw",
            "https://www.youtube.com.example/watch?v=jNQXAC9IVRw",
            // ID の形が違う。
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=jNQXAC9IVR!",
            "https://youtu.be/",
            // 動画ではない URL。
            "https://www.youtube.com/playlist?list=PLabc",
            "https://www.youtube.com/@jawed",
            "https://www.youtube.com/watch",
        ] {
            assert_eq!(video_id(text), None, "{text}");
        }
    }

    #[test]
    fn a_playlist_url_gives_the_playlist_id() {
        for url in [
            "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            "youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI&si=abc",
            "https://m.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
        ] {
            assert_eq!(
                playlist_id(url).as_deref(),
                Some("PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI"),
                "{url}"
            );
        }
    }

    #[test]
    fn a_video_url_with_a_list_is_not_a_playlist() {
        // 再生中の動画から共有した URL。見たいのは動画の方。
        assert_eq!(
            playlist_id("https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PLabc"),
            None
        );
        assert_eq!(playlist_id("https://www.youtube.com/playlist?list="), None);
        assert_eq!(
            playlist_id("https://www.youtube.com/playlist?list=PL<x>"),
            None
        );
        assert_eq!(playlist_id("https://example.com/playlist?list=PLabc"), None);
        assert_eq!(playlist_id("https://youtu.be/playlist?list=PLabc"), None);
        assert_eq!(playlist_id("PLabc"), None);
    }
}
