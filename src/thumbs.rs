//! サムネイルの URL・キャッシュパス・状態表と、取得から縮小までの一連の流れ。

use crate::fetch::{Download, Fetcher, PARALLEL_MAX, curl_args};
use crate::rgb::{self, RgbImage};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const APP_DIR: &str = "tuitube";
const THUMBS_DIR: &str = "thumbs";
/// 保存先を用意できないときの一時的な置き場。読んだ後に消す。
const TEMP_DIR: &str = "tuitube-thumbs";

pub const MISSING_CURL: &str =
    "curl が見つかりません (PATH を確認してください)。サムネイルなしで表示します";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbState {
    Pending,
    Ready(RgbImage),
    Failed,
}

#[derive(Debug, Default)]
pub struct Thumbs {
    entries: HashMap<String, ThumbState>,
    /// 今デコードしてある目標寸法。
    decoded_px: (u32, u32),
    dirty: bool,
    /// curl が無い等、取得を止めた理由。
    disabled: Option<String>,
    fetching: bool,
}

impl Thumbs {
    /// 新しい結果集合に入れ替える。消える ID の状態は捨てる。
    pub fn reset(&mut self, ids: &[String]) {
        self.entries
            .retain(|id, _| ids.iter().any(|want| want == id));
        for id in ids {
            self.entries
                .entry(id.clone())
                .or_insert(ThumbState::Pending);
        }
    }

    /// 取得が要る ID (未取得・寸法が変わった) を返す。
    pub fn wanted(&self, ids: &[String], target_px: (u32, u32)) -> Vec<String> {
        if self.disabled.is_some() || target_px.0 == 0 || target_px.1 == 0 {
            return Vec::new();
        }
        let resized = self.decoded_px != target_px;
        ids.iter()
            .filter(|id| {
                resized
                    || !matches!(
                        self.entries.get(id.as_str()),
                        Some(ThumbState::Ready(_) | ThumbState::Failed)
                    )
            })
            .cloned()
            .collect()
    }

    pub fn apply(&mut self, images: Vec<(String, Result<RgbImage, ()>)>, target_px: (u32, u32)) {
        if images.is_empty() {
            return;
        }
        let mut changed = self.decoded_px != target_px;
        self.decoded_px = target_px;
        for (id, image) in images {
            let next = match image {
                Ok(image) => ThumbState::Ready(image),
                Err(()) => ThumbState::Failed,
            };
            if self.entries.get(&id) != Some(&next) {
                changed = true;
            }
            self.entries.insert(id, next);
        }
        if changed {
            self.dirty = true;
        }
    }

    pub fn get(&self, id: &str) -> Option<&RgbImage> {
        match self.entries.get(id) {
            Some(ThumbState::Ready(image)) => Some(image),
            _ => None,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// 以後の取得を止める。理由はステータス行に一度だけ出す。
    pub fn disable(&mut self, reason: String) {
        self.disabled = Some(reason);
    }

    pub fn disabled(&self) -> Option<&str> {
        self.disabled.as_deref()
    }

    pub fn set_fetching(&mut self, fetching: bool) {
        self.fetching = fetching;
    }

    pub fn is_fetching(&self) -> bool {
        self.fetching
    }
}

/// 動画 ID から組み立てる固定 URL。thumbnails 配列の URL は WebP を返すので使わない。
pub fn url_for(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
}

/// パス操作の材料にするので、動画 ID として素直な文字だけを通す。
pub fn safe_id(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(id)
}

/// `$XDG_CACHE_HOME/tuitube/thumbs` か `$HOME/.cache/tuitube/thumbs`。
pub fn cache_dir(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_cache_home.filter(|v| !v.is_empty()) {
        return Some(Path::new(xdg).join(APP_DIR).join(THUMBS_DIR));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(
        Path::new(home)
            .join(".cache")
            .join(APP_DIR)
            .join(THUMBS_DIR),
    )
}

pub fn cache_path(dir: &Path, id: &str) -> Option<PathBuf> {
    Some(dir.join(format!("{}.jpg", safe_id(id)?)))
}

/// 更新時刻の新しい順に keep 枚だけ残し、それ以外を返す (削除は呼び出し側)。
pub fn prune_targets(files: Vec<(PathBuf, SystemTime)>, keep: usize) -> Vec<PathBuf> {
    if files.len() <= keep {
        return Vec::new();
    }
    let mut files = files;
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    files.into_iter().skip(keep).map(|(path, _)| path).collect()
}

/// 起動時の間引き。読めないディレクトリは黙って諦める。
pub fn prune_cache(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            meta.is_file()
                .then(|| Some((entry.path(), meta.modified().ok()?)))?
        })
        .collect();
    for path in prune_targets(files, keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, Default)]
pub struct ThumbsOutcome {
    pub images: Vec<(String, Result<RgbImage, ()>)>,
    /// 一度だけ伝える事情。
    pub notice: Option<String>,
    /// 以後の取得を止める理由。
    pub disable: Option<String>,
}

/// 置き場と、読んだ後に残すかどうか。
struct Store {
    dir: PathBuf,
    keep: bool,
}

/// キャッシュにあるものは読むだけ、無いものだけ curl 1 プロセスで取り、
/// まとめてデコードして目標寸法へ縮める。
pub async fn fetch_thumbnails<F: Fetcher>(
    fetcher: &F,
    ids: Vec<String>,
    target_px: (u32, u32),
    cache: Option<PathBuf>,
    timeout: Duration,
) -> ThumbsOutcome {
    let mut outcome = ThumbsOutcome::default();
    if target_px.0 == 0 || target_px.1 == 0 {
        return outcome;
    }
    let (store, notice) = prepare_store(cache);
    outcome.notice = notice;
    let Some(store) = store else {
        outcome
            .images
            .extend(ids.into_iter().map(|id| (id, Err(()))));
        return outcome;
    };

    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    let mut downloads: Vec<Download> = Vec::new();
    for id in ids {
        let Some(path) = cache_path(&store.dir, &id) else {
            outcome.images.push((id, Err(())));
            continue;
        };
        if !path.exists() {
            downloads.push(Download {
                id: id.clone(),
                url: url_for(&id),
                path: path.clone(),
            });
        }
        paths.push((id, path));
    }

    if !downloads.is_empty() {
        let args = curl_args(&downloads, timeout, PARALLEL_MAX);
        // 終了コードは見ない。落ちてきたファイルだけを後で読む。
        if let Err(e) = fetcher.run(args).await
            && e.kind() == ErrorKind::NotFound
        {
            outcome.disable = Some(MISSING_CURL.to_string());
        }
    }

    let keep = store.keep;
    let decoded = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|(id, path)| {
                let image = read_and_shrink(&path, target_px);
                // 壊れた・書きかけのファイルを残すと、次も同じように失敗する。
                if !keep || image.is_err() {
                    let _ = std::fs::remove_file(&path);
                }
                (id, image)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    outcome.images.extend(decoded);
    outcome
}

fn prepare_store(cache: Option<PathBuf>) -> (Option<Store>, Option<String>) {
    if let Some(dir) = cache {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => return (Some(Store { dir, keep: true }), None),
            Err(e) => {
                let notice = format!(
                    "サムネイルの保存先を作れませんでした: {e}。今回は保存せずに表示します"
                );
                return (temp_store(), Some(notice));
            }
        }
    }
    (temp_store(), None)
}

fn temp_store() -> Option<Store> {
    let dir = std::env::temp_dir().join(TEMP_DIR);
    std::fs::create_dir_all(&dir)
        .ok()
        .map(|()| Store { dir, keep: false })
}

fn read_and_shrink(path: &Path, target_px: (u32, u32)) -> Result<RgbImage, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    let image = crate::jpeg::decode(&bytes).map_err(|_| ())?;
    let (width, height) = rgb::fit_box((image.width, image.height), target_px);
    rgb::shrink(&image, width, height).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::fixtures::{CurlResult, FakeCurl};

    const TINY_8X4: &[u8] = include_bytes!("testdata/tiny8x4.jpg");
    const TARGET: (u32, u32) = (144, 80);

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    fn image(width: u32, height: u32) -> RgbImage {
        RgbImage::new(
            width,
            height,
            vec![7; (width as usize) * (height as usize) * 3],
        )
        .expect("長さは合っている")
    }

    fn ready(thumbs: &mut Thumbs, id: &str, target_px: (u32, u32)) {
        thumbs.apply(vec![(id.to_string(), Ok(image(2, 2)))], target_px);
        thumbs.take_dirty();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-thumbs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn at(time: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(time)
    }

    #[test]
    fn url_for_builds_the_mqdefault_url() {
        assert_eq!(
            url_for("abc123"),
            "https://i.ytimg.com/vi/abc123/mqdefault.jpg"
        );
    }

    #[test]
    fn safe_id_rejects_path_separators_and_dots() {
        assert_eq!(safe_id("a-b_C9"), Some("a-b_C9"));
        for bad in ["../etc/passwd", "ab/cd", "a.b", "", "a b", "a\\b", "日本語"] {
            assert_eq!(safe_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn cache_dir_prefers_xdg_cache_home_over_home() {
        assert_eq!(
            cache_dir(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/tuitube/thumbs"))
        );
        assert_eq!(
            cache_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.cache/tuitube/thumbs"))
        );
        // 空文字は未設定と同じに扱う。
        assert_eq!(
            cache_dir(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.cache/tuitube/thumbs"))
        );
    }

    #[test]
    fn cache_dir_is_none_without_either_variable() {
        assert_eq!(cache_dir(None, None), None);
        assert_eq!(cache_dir(Some(OsStr::new("")), Some(OsStr::new(""))), None);
    }

    #[test]
    fn cache_path_refuses_an_unsafe_id() {
        let dir = Path::new("/cache");
        assert_eq!(
            cache_path(dir, "abc"),
            Some(PathBuf::from("/cache/abc.jpg"))
        );
        assert_eq!(cache_path(dir, "../x"), None);
    }

    #[test]
    fn prune_targets_keeps_the_newest_files() {
        let files = vec![
            (PathBuf::from("old.jpg"), at(10)),
            (PathBuf::from("new.jpg"), at(30)),
            (PathBuf::from("mid.jpg"), at(20)),
        ];
        assert_eq!(
            prune_targets(files.clone(), 1),
            [PathBuf::from("mid.jpg"), PathBuf::from("old.jpg")]
        );
        assert_eq!(prune_targets(files, 2), [PathBuf::from("old.jpg")]);
    }

    #[test]
    fn prune_targets_returns_nothing_when_under_the_limit() {
        let files = vec![(PathBuf::from("a.jpg"), at(10))];
        assert!(prune_targets(files.clone(), 1).is_empty());
        assert!(prune_targets(files, 5).is_empty());
        assert!(prune_targets(Vec::new(), 0).is_empty());
    }

    #[test]
    fn reset_drops_entries_that_left_the_result_set() {
        let mut thumbs = Thumbs::default();
        ready(&mut thumbs, "a", TARGET);
        ready(&mut thumbs, "b", TARGET);
        assert!(thumbs.get("a").is_some());

        thumbs.reset(&ids(&["b", "c"]));
        assert!(thumbs.get("a").is_none());
        assert!(thumbs.get("b").is_some());
        // 新しく入った ID は未取得として並ぶ。
        assert_eq!(thumbs.wanted(&ids(&["b", "c"]), TARGET), ids(&["c"]));
    }

    #[test]
    fn wanted_skips_ids_that_are_already_ready_at_the_same_size() {
        let mut thumbs = Thumbs::default();
        thumbs.reset(&ids(&["a", "b"]));
        assert_eq!(thumbs.wanted(&ids(&["a", "b"]), TARGET), ids(&["a", "b"]));

        ready(&mut thumbs, "a", TARGET);
        assert_eq!(thumbs.wanted(&ids(&["a", "b"]), TARGET), ids(&["b"]));
    }

    #[test]
    fn wanted_includes_everything_again_when_the_target_size_changes() {
        let mut thumbs = Thumbs::default();
        ready(&mut thumbs, "a", TARGET);
        thumbs.apply(vec![("b".to_string(), Err(()))], TARGET);
        assert!(thumbs.wanted(&ids(&["a", "b"]), TARGET).is_empty());

        assert_eq!(
            thumbs.wanted(&ids(&["a", "b"]), (160, 90)),
            ids(&["a", "b"])
        );
    }

    #[test]
    fn wanted_skips_ids_that_already_failed() {
        // 失敗を毎周取りに行くと、落ちている URL へ延々と当たり続ける。
        let mut thumbs = Thumbs::default();
        thumbs.apply(vec![("a".to_string(), Err(()))], TARGET);
        assert!(thumbs.wanted(&ids(&["a"]), TARGET).is_empty());
        assert!(thumbs.get("a").is_none());
    }

    #[test]
    fn wanted_is_empty_once_thumbnails_are_disabled() {
        let mut thumbs = Thumbs::default();
        thumbs.reset(&ids(&["a"]));
        thumbs.disable(MISSING_CURL.to_string());
        assert!(thumbs.wanted(&ids(&["a"]), TARGET).is_empty());
        assert_eq!(thumbs.disabled(), Some(MISSING_CURL));
    }

    #[test]
    fn apply_marks_dirty_only_when_something_changed() {
        let mut thumbs = Thumbs::default();
        thumbs.apply(Vec::new(), TARGET);
        assert!(!thumbs.take_dirty());

        thumbs.apply(vec![("a".to_string(), Ok(image(2, 2)))], TARGET);
        assert!(thumbs.take_dirty());

        // 同じ内容をもう一度入れても貼り直す理由にはならない。
        thumbs.apply(vec![("a".to_string(), Ok(image(2, 2)))], TARGET);
        assert!(!thumbs.take_dirty());

        thumbs.apply(vec![("a".to_string(), Ok(image(4, 4)))], TARGET);
        assert!(thumbs.take_dirty());
    }

    #[test]
    fn take_dirty_clears_the_flag() {
        let mut thumbs = Thumbs::default();
        assert!(!thumbs.take_dirty());
        thumbs.mark_dirty();
        assert!(thumbs.take_dirty());
        assert!(!thumbs.take_dirty());
    }

    #[tokio::test]
    async fn a_download_is_decoded_and_shrunk_to_the_target() {
        let dir = temp_dir("download");
        let curl = FakeCurl::new(CurlResult::Wrote, TINY_8X4);
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["abc"]),
            (4, 4),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(curl.urls(), [url_for("abc")]);
        let (id, image) = &outcome.images[0];
        assert_eq!(id, "abc");
        let image = image.as_ref().expect("縮小まで通る");
        // 8x4 を 4x4 の箱へ入れるとアスペクトを保って 4x2。
        assert_eq!((image.width, image.height), (4, 2));
        assert!(outcome.disable.is_none());
        assert!(dir.join("abc.jpg").exists(), "キャッシュに残す");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cached_ids_are_not_passed_to_curl() {
        let dir = temp_dir("cached");
        std::fs::write(dir.join("abc.jpg"), TINY_8X4).expect("書ける");
        let curl = FakeCurl::new(CurlResult::Wrote, TINY_8X4);
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["abc", "xyz"]),
            (8, 8),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(
            curl.urls(),
            [url_for("xyz")],
            "キャッシュ済みは取りに行かない"
        );
        assert_eq!(outcome.images.len(), 2);
        assert!(outcome.images.iter().all(|(_, image)| image.is_ok()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_resize_re_decodes_from_the_cache_without_calling_curl() {
        let dir = temp_dir("resize");
        std::fs::write(dir.join("abc.jpg"), TINY_8X4).expect("書ける");
        let curl = FakeCurl::new(CurlResult::Failed, b"");
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["abc"]),
            (4, 4),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert!(curl.calls().is_empty(), "ネットワークには行かない");
        let image = outcome.images[0].1.as_ref().expect("キャッシュから読める");
        assert_eq!((image.width, image.height), (4, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_curl_leaves_the_cells_empty_and_does_not_retry() {
        let dir = temp_dir("failed");
        let curl = FakeCurl::new(CurlResult::Failed, b"");
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["abc"]),
            (144, 80),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(outcome.images, [("abc".to_string(), Err(()))]);
        assert!(outcome.disable.is_none(), "curl 自体はあるので止めない");

        // 失敗として控えれば、次の周で取りに行かない。
        let mut thumbs = Thumbs::default();
        thumbs.apply(outcome.images, (144, 80));
        assert!(thumbs.wanted(&ids(&["abc"]), (144, 80)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_curl_binary_disables_thumbnails_with_a_notice() {
        let dir = temp_dir("missing");
        let curl = FakeCurl::new(CurlResult::Missing, b"");
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["abc"]),
            (144, 80),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(outcome.disable.as_deref(), Some(MISSING_CURL));
        assert_eq!(outcome.images, [("abc".to_string(), Err(()))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unsafe_id_never_reaches_the_filesystem_or_curl() {
        let dir = temp_dir("unsafe");
        let curl = FakeCurl::new(CurlResult::Wrote, TINY_8X4);
        let outcome = fetch_thumbnails(
            &curl,
            ids(&["../escape"]),
            (144, 80),
            Some(dir.clone()),
            Duration::from_secs(10),
        )
        .await;

        assert!(curl.calls().is_empty());
        assert_eq!(outcome.images, [("../escape".to_string(), Err(()))]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_cache_removes_all_but_the_newest() {
        let dir = temp_dir("prune");
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            std::fs::write(dir.join(name), b"x").expect("書ける");
            // 更新時刻の差を作る。
            std::thread::sleep(Duration::from_millis(10));
        }
        prune_cache(&dir, 1);
        let left = std::fs::read_dir(&dir).expect("一覧").count();
        assert_eq!(left, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
