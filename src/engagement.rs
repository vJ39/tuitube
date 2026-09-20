//! いいね済み/チャンネル登録済み状態の控えと、その再確認の要否 (TTL)・ディスク保存。
//! ここでは YouTube へ問い合わせない。取得した結果とこのアプリでの操作を溜めるだけ。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_DIR: &str = "tuitube";
const ENGAGEMENT_FILE: &str = "engagement.json";

/// 登録チャンネルの控えの上限件数。超えたら最終確認が最も古いものから捨てる
/// (resume.rs の RESUME_CAPACITY と同じ考え方。いいね一覧は丸ごと入れ替えるので対象外)。
const MAX_CHANNELS: usize = 500;

/// 置き場を決められない環境 ($HOME も $XDG_CACHE_HOME も無い) の理由。
pub const NO_ENGAGEMENT_PATH: &str =
    "いいね済み/登録済みの控えの置き場が分かりません ($HOME を設定してください)";

/// いいね済み動画とチャンネル登録の控え。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EngagementCache {
    /// 自分がいいねした動画 ID の集合。
    liked_videos: HashSet<String>,
    /// 上の集合を丸ごと確かめた最終時刻。まだ一度も取っていなければ None。
    liked_last_confirmed: Option<SystemTime>,
    /// 問い合わせ済みチャンネルの登録有無。キーが無い = 判定不能。
    subscribed_channels: HashMap<String, bool>,
    /// チャンネルごとの最終確認時刻。
    channel_last_confirmed: HashMap<String, SystemTime>,
}

impl EngagementCache {
    /// いいね済みか。控えに無ければ「いいねしていない」として扱う
    /// (一覧は丸ごと取る前提なので、未確認と未いいねを分けない)。
    pub fn is_liked(&self, video_id: &str) -> bool {
        self.liked_videos.contains(video_id)
    }

    /// 登録済みか。未確認のチャンネルは None (印を出さない)。
    pub fn is_subscribed(&self, channel_id: &str) -> Option<bool> {
        self.subscribed_channels.get(channel_id).copied()
    }

    /// 何も溜まっていないか。保存で要らないファイルを作らない判定に使う。
    pub fn is_empty(&self) -> bool {
        self.liked_videos.is_empty()
            && self.liked_last_confirmed.is_none()
            && self.subscribed_channels.is_empty()
            && self.channel_last_confirmed.is_empty()
    }

    /// いいね一覧を丸ごと取り直す必要があるか (未取得・TTL 切れ)。
    pub fn liked_needs_refresh(&self, now: SystemTime, ttl: Duration) -> bool {
        match self.liked_last_confirmed {
            None => true,
            Some(last) => expired(last, now, ttl),
        }
    }

    /// 渡した中で確認が要るチャンネル ID (未確認・TTL 切れ)。並びは渡された順、重複は 1 度だけ。
    pub fn channels_needing_refresh(
        &self,
        channel_ids: &[String],
        now: SystemTime,
        ttl: Duration,
    ) -> Vec<String> {
        let mut seen = HashSet::new();
        channel_ids
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter(|id| match self.channel_last_confirmed.get(id.as_str()) {
                None => true,
                Some(last) => expired(*last, now, ttl),
            })
            .cloned()
            .collect()
    }

    /// API から取った一覧で丸ごと入れ替える。手元にしか無かった ID は消える。
    pub fn replace_liked<I>(&mut self, video_ids: I, now: SystemTime)
    where
        I: IntoIterator<Item = String>,
    {
        self.liked_videos = video_ids.into_iter().collect();
        self.liked_last_confirmed = Some(now);
    }

    /// このアプリでいいね/取り消しをした分を即時反映する。
    pub fn remember_like(&mut self, video_id: &str, liked: bool, now: SystemTime) {
        if liked {
            self.liked_videos.insert(video_id.to_string());
        } else {
            self.liked_videos.remove(video_id);
        }
        // 一度も一覧を取っていない状態でここを now にすると、他のいいね済み動画に
        // 印が出ないまま TTL のあいだ取得が止まる。初回の取得は残す。
        if self.liked_last_confirmed.is_some() {
            self.liked_last_confirmed = Some(now);
        }
    }

    /// このアプリで登録/解除をした分を即時反映する。
    pub fn remember_subscription(&mut self, channel_id: &str, subscribed: bool, now: SystemTime) {
        self.subscribed_channels
            .insert(channel_id.to_string(), subscribed);
        self.channel_last_confirmed
            .insert(channel_id.to_string(), now);
        self.enforce_channel_capacity();
    }

    /// バッチ確認の結果を反映する。問い合わせたのに返ってこなかった ID は未登録として確定させる。
    pub fn remember_channels(&mut self, asked: &[String], subscribed: &[String], now: SystemTime) {
        let subscribed: HashSet<&str> = subscribed.iter().map(String::as_str).collect();
        for id in asked {
            self.subscribed_channels
                .insert(id.clone(), subscribed.contains(id.as_str()));
            self.channel_last_confirmed.insert(id.clone(), now);
        }
        self.enforce_channel_capacity();
    }

    /// MAX_CHANNELS を超えた分だけ、最終確認が最も古いものから捨てる。
    fn enforce_channel_capacity(&mut self) {
        while self.subscribed_channels.len() > MAX_CHANNELS {
            let Some(oldest) = self
                .channel_last_confirmed
                .iter()
                .min_by_key(|(_, time)| **time)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.subscribed_channels.remove(&oldest);
            self.channel_last_confirmed.remove(&oldest);
        }
    }
}

/// 最終確認からの経過が TTL を超えたか。時計が巻き戻って未来の時刻が残っている場合も
/// 切れた扱いにする (そうしないと再確認が永久に止まる)。
fn expired(last: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    now.duration_since(last)
        .map_or(true, |elapsed| elapsed > ttl)
}

/// `$XDG_CACHE_HOME/tuitube/engagement.json` か `$HOME/.cache/tuitube/engagement.json`。
/// サムネイルのディスクキャッシュ (thumbs::cache_dir) と同じ置き場に並べる。
pub fn cache_path(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_cache_home.filter(|v| !v.is_empty()) {
        return Some(Path::new(xdg).join(APP_DIR).join(ENGAGEMENT_FILE));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(
        Path::new(home)
            .join(".cache")
            .join(APP_DIR)
            .join(ENGAGEMENT_FILE),
    )
}

/// ファイルの生の形。時刻は環境をまたげるよう UNIX 秒で持つ。未知のキーは無視する。
#[derive(Debug, Default, Deserialize, Serialize)]
struct EngagementFile {
    #[serde(default)]
    liked_videos: Vec<String>,
    #[serde(default)]
    liked_last_confirmed: Option<u64>,
    #[serde(default)]
    subscribed_channels: HashMap<String, bool>,
    #[serde(default)]
    channel_last_confirmed: HashMap<String, u64>,
}

fn to_unix(time: SystemTime) -> Option<u64> {
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn from_unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// 読めない・壊れているファイルは空として扱う。手編集を想定しない控えなので、
/// 起動が止まらないことを優先する (resume.rs と同じ考え方)。
pub fn load_from(path: Option<&Path>) -> EngagementCache {
    let Some(path) = path else {
        return EngagementCache::default();
    };
    let file: EngagementFile = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    EngagementCache {
        liked_videos: file.liked_videos.into_iter().collect(),
        liked_last_confirmed: file.liked_last_confirmed.map(from_unix),
        subscribed_channels: file.subscribed_channels,
        channel_last_confirmed: file
            .channel_last_confirmed
            .into_iter()
            .map(|(id, secs)| (id, from_unix(secs)))
            .collect(),
    }
}

pub fn load() -> EngagementCache {
    let xdg = std::env::var_os("XDG_CACHE_HOME");
    let home = std::env::var_os("HOME");
    load_from(cache_path(xdg.as_deref(), home.as_deref()).as_deref())
}

/// 空の控えは書かない (要らないファイルを作らない)。
pub fn save_to(path: Option<&Path>, cache: &EngagementCache) -> Result<(), String> {
    let path = path.ok_or_else(|| NO_ENGAGEMENT_PATH.to_string())?;
    if cache.is_empty() {
        return Ok(());
    }
    let mut liked_videos: Vec<String> = cache.liked_videos.iter().cloned().collect();
    // HashSet/HashMap の並びは実行ごとに変わる。差分を見て分かるよう並べてから書く。
    liked_videos.sort();
    let file = EngagementFile {
        liked_videos,
        liked_last_confirmed: cache.liked_last_confirmed.and_then(to_unix),
        subscribed_channels: cache.subscribed_channels.clone(),
        channel_last_confirmed: cache
            .channel_last_confirmed
            .iter()
            .filter_map(|(id, time)| Some((id.clone(), to_unix(*time)?)))
            .collect(),
    };
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    write_atomically(path, &text)
}

pub fn save(cache: &EngagementCache) -> Result<(), String> {
    let xdg = std::env::var_os("XDG_CACHE_HOME");
    let home = std::env::var_os("HOME");
    save_to(
        cache_path(xdg.as_deref(), home.as_deref()).as_deref(),
        cache,
    )
}

/// 途中で落ちても壊れたファイルを残さないよう、一時ファイルへ書いてから置き換える。
/// 一時ファイルの名前はプロセスごとに分ける。2 つ起動して同時に書いても、
/// 片方の書きかけをもう片方が本番の位置へ移さないため。
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if let Err(e) = write_and_sync(&tmp, text) {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 置き換える前にディスクまで届けておく。電源が落ちても空のファイルにならないため。
fn write_and_sync(tmp: &Path, text: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(604_800);

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-engagement-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 置き換えに使った一時ファイルの残り。
    fn leftover_temp_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("読める")
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|name| name.contains(".tmp"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn cache_path_prefers_xdg_cache_home_then_home() {
        assert_eq!(
            cache_path(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/tuitube/engagement.json"))
        );
        assert_eq!(
            cache_path(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.cache/tuitube/engagement.json"))
        );
        // 空文字は未設定と同じに扱う。
        assert_eq!(
            cache_path(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.cache/tuitube/engagement.json"))
        );
        assert_eq!(cache_path(None, None), None);
    }

    #[test]
    fn the_engagement_file_sits_next_to_the_thumbnail_cache() {
        let thumbs = crate::thumbs::cache_dir(None, Some(OsStr::new("/h"))).expect("thumbs");
        let engagement = cache_path(None, Some(OsStr::new("/h"))).expect("engagement");
        assert_eq!(thumbs.parent(), engagement.parent());
    }

    #[test]
    fn an_unknown_video_is_not_liked_and_an_unknown_channel_is_undecided() {
        let cache = EngagementCache::default();
        assert!(!cache.is_liked("v1"));
        assert_eq!(cache.is_subscribed("c1"), None, "未確認は印を出さない");
        assert!(cache.is_empty());
    }

    #[test]
    fn the_liked_list_needs_a_refresh_until_it_is_confirmed_once() {
        let mut cache = EngagementCache::default();
        assert!(cache.liked_needs_refresh(at(0), TTL));

        cache.replace_liked(ids(&["v1", "v2"]), at(1_000));

        assert!(!cache.liked_needs_refresh(at(1_000), TTL));
        assert!(cache.is_liked("v1"));
        assert!(cache.is_liked("v2"));
    }

    #[test]
    fn an_expired_liked_list_needs_a_refresh_again() {
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v1"]), at(1_000));

        assert!(
            !cache.liked_needs_refresh(at(1_000) + TTL, TTL),
            "ちょうど TTL までは取り直さない"
        );
        assert!(cache.liked_needs_refresh(at(1_001) + TTL, TTL));
    }

    #[test]
    fn a_confirmation_time_in_the_future_counts_as_expired() {
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v1"]), at(10_000));
        cache.remember_subscription("c1", true, at(10_000));

        // 時計が巻き戻っても再確認が止まらないこと。
        assert!(cache.liked_needs_refresh(at(500), TTL));
        assert_eq!(
            cache.channels_needing_refresh(&ids(&["c1"]), at(500), TTL),
            ids(&["c1"])
        );
    }

    #[test]
    fn replacing_the_liked_list_drops_ids_that_are_gone() {
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v1", "v2"]), at(1_000));

        cache.replace_liked(ids(&["v2"]), at(2_000));

        assert!(!cache.is_liked("v1"), "本家で外れた分は消す");
        assert!(cache.is_liked("v2"));
    }

    #[test]
    fn a_local_like_shows_up_immediately() {
        let mut cache = EngagementCache::default();
        cache.replace_liked(Vec::new(), at(1_000));

        cache.remember_like("v1", true, at(1_500));

        assert!(cache.is_liked("v1"));
        // 操作で確定したので、直後に取り直さない。
        assert!(!cache.liked_needs_refresh(at(1_000) + TTL + Duration::from_secs(1), TTL));
    }

    #[test]
    fn unliking_removes_the_video() {
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v1"]), at(1_000));

        cache.remember_like("v1", false, at(1_500));

        assert!(!cache.is_liked("v1"));
    }

    #[test]
    fn a_local_like_before_the_first_fetch_does_not_skip_it() {
        let mut cache = EngagementCache::default();

        cache.remember_like("v1", true, at(1_500));

        assert!(cache.is_liked("v1"));
        assert!(
            cache.liked_needs_refresh(at(1_500), TTL),
            "一覧を一度も取っていないので、他の動画の印はまだ出せない"
        );
    }

    #[test]
    fn a_local_subscription_shows_up_immediately_and_is_confirmed() {
        let mut cache = EngagementCache::default();

        cache.remember_subscription("c1", true, at(1_000));

        assert_eq!(cache.is_subscribed("c1"), Some(true));
        assert!(
            cache
                .channels_needing_refresh(&ids(&["c1"]), at(1_000), TTL)
                .is_empty()
        );
    }

    #[test]
    fn unsubscribing_locally_is_remembered_as_not_subscribed() {
        let mut cache = EngagementCache::default();
        cache.remember_subscription("c1", true, at(1_000));

        cache.remember_subscription("c1", false, at(2_000));

        assert_eq!(
            cache.is_subscribed("c1"),
            Some(false),
            "未確認 (None) に戻さない"
        );
    }

    #[test]
    fn only_unconfirmed_or_expired_channels_are_asked_again() {
        let mut cache = EngagementCache::default();
        cache.remember_subscription("fresh", true, at(1_000));
        cache.remember_subscription("stale", true, at(1_000));
        // stale だけ TTL を過ぎた状態にする。
        let now = at(1_001) + TTL;
        cache.remember_subscription("fresh", true, now);

        let wanted = cache.channels_needing_refresh(&ids(&["fresh", "stale", "new"]), now, TTL);

        assert_eq!(wanted, ids(&["stale", "new"]));
    }

    #[test]
    fn a_repeated_channel_id_is_asked_only_once() {
        let cache = EngagementCache::default();

        let wanted = cache.channels_needing_refresh(&ids(&["c1", "c2", "c1"]), at(0), TTL);

        assert_eq!(wanted, ids(&["c1", "c2"]), "並びは渡された順");
    }

    #[test]
    fn asked_channels_missing_from_the_answer_are_confirmed_as_not_subscribed() {
        let mut cache = EngagementCache::default();

        cache.remember_channels(&ids(&["c1", "c2"]), &ids(&["c1"]), at(1_000));

        assert_eq!(cache.is_subscribed("c1"), Some(true));
        assert_eq!(cache.is_subscribed("c2"), Some(false));
        assert!(
            cache
                .channels_needing_refresh(&ids(&["c1", "c2"]), at(1_000), TTL)
                .is_empty(),
            "答えが返った分は確認済み"
        );
    }

    #[test]
    fn exceeding_the_channel_capacity_drops_the_oldest_one_via_remember_subscription() {
        let mut cache = EngagementCache::default();
        for i in 0..MAX_CHANNELS {
            cache.remember_subscription(&format!("c{i}"), true, at(i as u64));
        }

        cache.remember_subscription("new", true, at(MAX_CHANNELS as u64));

        assert_eq!(cache.is_subscribed("c0"), None, "最も古い c0 が捨てられる");
        assert_eq!(cache.is_subscribed("c1"), Some(true), "c1 は残る");
        assert_eq!(cache.is_subscribed("new"), Some(true));
    }

    #[test]
    fn exceeding_the_channel_capacity_drops_the_oldest_ones_via_remember_channels() {
        let mut cache = EngagementCache::default();
        for i in 0..MAX_CHANNELS {
            cache.remember_subscription(&format!("c{i}"), true, at(i as u64));
        }

        let extra = ids(&["new1", "new2"]);
        cache.remember_channels(&extra, &extra, at(MAX_CHANNELS as u64));

        assert_eq!(cache.is_subscribed("c0"), None);
        assert_eq!(cache.is_subscribed("c1"), None);
        assert_eq!(cache.is_subscribed("c2"), Some(true), "c2 以降は残る");
        assert_eq!(cache.is_subscribed("new1"), Some(true));
        assert_eq!(cache.is_subscribed("new2"), Some(true));
    }

    #[test]
    fn saving_then_loading_keeps_the_state() {
        let dir = temp_dir("round-trip");
        let path = dir.join("engagement.json");
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v2", "v1"]), at(1_000));
        cache.remember_channels(&ids(&["c1", "c2"]), &ids(&["c2"]), at(2_000));

        save_to(Some(&path), &cache).expect("書ける");

        assert_eq!(load_from(Some(&path)), cache);
        assert_eq!(leftover_temp_files(&dir), Vec::<String>::new());
    }

    #[test]
    fn a_reloaded_cache_keeps_the_remaining_ttl() {
        let dir = temp_dir("ttl-survives");
        let path = dir.join("engagement.json");
        let mut cache = EngagementCache::default();
        cache.replace_liked(ids(&["v1"]), at(1_000));
        save_to(Some(&path), &cache).expect("書ける");

        let loaded = load_from(Some(&path));

        // 起動し直しても TTL が最初からにならないこと。
        assert!(!loaded.liked_needs_refresh(at(1_000) + TTL, TTL));
        assert!(loaded.liked_needs_refresh(at(1_001) + TTL, TTL));
    }

    #[test]
    fn a_missing_file_loads_an_empty_cache_without_creating_it() {
        let dir = temp_dir("missing");
        let path = dir.join("nested/engagement.json");

        let cache = load_from(Some(&path));

        assert!(cache.is_empty());
        assert!(!path.exists(), "読むだけでファイルを作らない");
    }

    #[test]
    fn a_broken_file_loads_an_empty_cache() {
        let dir = temp_dir("broken");
        let path = dir.join("engagement.json");
        fs::write(&path, "これは JSON ではない {{{").expect("書ける");

        assert_eq!(
            load_from(Some(&path)),
            EngagementCache::default(),
            "起動は止めず空で始める"
        );
    }

    #[test]
    fn unknown_keys_and_partial_files_are_read_as_far_as_possible() {
        let dir = temp_dir("partial");
        let path = dir.join("engagement.json");
        fs::write(
            &path,
            r#"{"liked_videos":["v1"],"unknown":1,"subscribed_channels":{"c1":true}}"#,
        )
        .expect("書ける");

        let cache = load_from(Some(&path));

        assert!(cache.is_liked("v1"));
        assert_eq!(cache.is_subscribed("c1"), Some(true));
        // 時刻が無いので、いいね一覧は取り直しから始める。
        assert!(cache.liked_needs_refresh(at(0), TTL));
    }

    #[test]
    fn without_a_path_loading_gives_an_empty_cache_and_saving_reports_the_reason() {
        assert_eq!(load_from(None), EngagementCache::default());

        let mut cache = EngagementCache::default();
        cache.remember_like("v1", true, at(1_000));
        assert_eq!(save_to(None, &cache), Err(NO_ENGAGEMENT_PATH.to_string()));
    }

    #[test]
    fn saving_an_empty_cache_does_not_create_the_file() {
        let dir = temp_dir("empty");
        let path = dir.join("engagement.json");

        save_to(Some(&path), &EngagementCache::default()).expect("書くものが無いので成功扱い");

        assert!(!path.exists());
    }

    #[test]
    fn a_failed_write_is_reported() {
        let dir = temp_dir("blocked");
        let blocker = dir.join("blocked");
        fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");
        let mut cache = EngagementCache::default();
        cache.remember_like("v1", true, at(1_000));

        assert!(save_to(Some(&blocker.join("engagement.json")), &cache).is_err());
    }

    #[test]
    fn the_temporary_file_is_named_per_process() {
        let dir = temp_dir("tmp-name");
        let path = dir.join("engagement.json");
        // 隣の窓が置いていった書きかけ。名前がぶつかると、これを本番の位置へ移してしまう。
        let other = dir.join("engagement.json.tmp.999999");
        fs::write(&other, "書きかけ {{{").expect("書ける");
        let mut cache = EngagementCache::default();
        cache.remember_like("v1", true, at(1_000));

        save_to(Some(&path), &cache).expect("書ける");

        assert!(other.exists(), "他のプロセスの一時ファイルを触らない");
        assert!(load_from(Some(&path)).is_liked("v1"));
    }
}
