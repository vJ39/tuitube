//! 動画/チャンネルのローカル非表示リスト (hidden.toml) のパス決定・読み込み・追記。
//! YouTube 側へは何も送らず、ここに溜めた ID を一覧から外すだけ。

use crate::search::SearchResult;
use crate::settings;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const HIDDEN_FILE: &str = "hidden.toml";

/// 置き場を決められない環境 ($HOME も $XDG_CONFIG_HOME も無い) の理由。
pub const NO_HIDDEN_PATH: &str = "非表示リストの置き場が分かりません ($HOME を設定してください)";

/// 読めないファイルへ書き足すと、手で直す前の中身ごと消える。直るまでは触らない。
pub const BROKEN_HIDDEN_FILE: &str = "hidden.toml が壊れています (手で直してください)";

/// 非表示 1 件。title は見返す用で、除外の判定には使わない。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub title: String,
}

/// ファイルの生の形。未知のキーは無視する。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HiddenFile {
    #[serde(default)]
    pub videos: Vec<Entry>,
    #[serde(default)]
    pub channels: Vec<Entry>,
}

/// 足す先。動画とチャンネルで書き込む配列だけが違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Channel,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Video => "videos",
            Kind::Channel => "channels",
        }
    }
}

/// 読み込んだ非表示 ID と、その追記先。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hidden {
    pub videos: HashSet<String>,
    pub channels: HashSet<String>,
    /// 非表示チャンネルの名前。channel_id を持たない行はこれで外す。
    pub channel_names: HashSet<String>,
    /// 追記先。決められない環境では None。
    pub path: Option<PathBuf>,
}

impl Hidden {
    /// 一覧から外す項目か。チャンネルを隠すと、その channel_id を持つ行も外れる。
    pub fn hides(&self, result: &SearchResult) -> bool {
        if self.videos.contains(&result.id) {
            return true;
        }
        if result
            .channel_id
            .as_ref()
            .is_some_and(|id| self.channels.contains(id))
        {
            return true;
        }
        // フィード系とチャンネルタブの行は --flat-playlist なので channel_id を持たない。
        // 名前で照合しないと、隠したチャンネルの動画がそれらの一覧に残り続ける。
        // 同名の別チャンネルまで外れうるが、隠したはずの行が並ぶより実害が小さい。
        result
            .uploader
            .as_ref()
            .is_some_and(|name| self.channel_names.contains(name))
    }

    pub fn add_video(&mut self, id: &str, title: &str) -> Result<(), String> {
        self.add(Kind::Video, id, title)
    }

    pub fn add_channel(&mut self, id: &str, title: &str) -> Result<(), String> {
        self.add(Kind::Channel, id, title)
    }

    /// 書けたときだけ集合へ入れる。ファイルと食い違ったまま隠れ続けないため。
    fn add(&mut self, kind: Kind, id: &str, title: &str) -> Result<(), String> {
        let path = self.path.clone().ok_or(NO_HIDDEN_PATH.to_string())?;
        // 別の窓や手で足された分を消さないよう、書く前に読み直す。
        // 読めないファイルはここで止める。空と見なして書くと中身が消える。
        let text = read_text(&path)?;
        let file: HiddenFile = toml::from_str(&text).map_err(|_| BROKEN_HIDDEN_FILE.to_string())?;
        let entries = match kind {
            Kind::Video => &file.videos,
            Kind::Channel => &file.channels,
        };
        if !entries.iter().any(|entry| entry.id == id) {
            append(&path, &text, kind, id, title)?;
        }
        match kind {
            Kind::Video => {
                self.videos.insert(id.to_string());
            }
            Kind::Channel => {
                self.channels.insert(id.to_string());
                if !title.is_empty() {
                    self.channel_names.insert(title.to_string());
                }
            }
        }
        Ok(())
    }
}

/// 設定ファイルと同じ置き場に並べる。
pub fn hidden_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(settings::app_config_dir(xdg_config_home, home)?.join(HIDDEN_FILE))
}

/// 読めない・壊れているファイルは空として扱う。隠せないだけで起動は止めない。
fn read(path: &Path) -> HiddenFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// 書き足す前の中身。まだ無いファイルは空、読めないファイルは理由を返す。
fn read_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// 末尾へ 1 件足す。全文を書き直すと、手で書いたコメントや知らないキーが消える。
fn append(path: &Path, current: &str, kind: Kind, id: &str, title: &str) -> Result<(), String> {
    let entry = Entry {
        id: id.to_string(),
        title: title.to_string(),
    };
    let body = toml::to_string(&entry).map_err(|e| e.to_string())?;
    let mut text = current.to_string();
    if !text.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(&format!("[[{}]]\n{body}", kind.key()));
    write_atomically(path, &text)
}

/// 途中で落ちても壊れたファイルを残さないよう、一時ファイルへ書いてから置き換える。
/// 一時ファイルの名前はプロセスごとに分ける。2 つ起動して同時に隠しても、
/// 片方の書きかけをもう片方が本番の位置へ移さないため。
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
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

pub fn load_from(path: Option<&Path>) -> Hidden {
    let Some(path) = path else {
        return Hidden::default();
    };
    let file = read(path);
    let mut channels = HashSet::new();
    let mut channel_names = HashSet::new();
    for entry in file.channels {
        channels.insert(entry.id);
        if !entry.title.is_empty() {
            channel_names.insert(entry.title);
        }
    }
    Hidden {
        videos: file.videos.into_iter().map(|entry| entry.id).collect(),
        channels,
        channel_names,
        path: Some(path.to_path_buf()),
    }
}

pub fn load() -> Hidden {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    load_from(hidden_path(xdg.as_deref(), home.as_deref()).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-hidden-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn result(id: &str, channel_id: Option<&str>) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
            channel_id: channel_id.map(str::to_string),
            is_live: false,
        }
    }

    #[test]
    fn hidden_path_prefers_xdg_config_home_then_home() {
        assert_eq!(
            hidden_path(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/tuitube/hidden.toml"))
        );
        assert_eq!(
            hidden_path(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/hidden.toml"))
        );
        // 空文字は未設定と同じに扱う。
        assert_eq!(
            hidden_path(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/hidden.toml"))
        );
        assert_eq!(hidden_path(None, None), None);
    }

    #[test]
    fn the_hidden_list_sits_next_to_the_config_file() {
        let config = settings::config_path(None, Some(OsStr::new("/h"))).expect("config");
        let hidden = hidden_path(None, Some(OsStr::new("/h"))).expect("hidden");
        assert_eq!(config.parent(), hidden.parent());
    }

    #[test]
    fn a_missing_file_yields_an_empty_list_without_creating_it() {
        let dir = temp_dir("missing");
        let path = dir.join("nested/hidden.toml");
        let hidden = load_from(Some(&path));

        assert!(hidden.videos.is_empty());
        assert!(hidden.channels.is_empty());
        assert_eq!(hidden.path.as_deref(), Some(path.as_path()));
        // 設定と違い、隠すまでは要らないファイルなので作らない。
        assert!(!path.exists());
    }

    #[test]
    fn without_a_path_the_list_is_empty() {
        let hidden = load_from(None);
        assert_eq!(hidden, Hidden::default());
        assert!(hidden.path.is_none());
    }

    #[test]
    fn a_broken_file_yields_an_empty_list() {
        let dir = temp_dir("broken");
        let path = dir.join("hidden.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書ける");

        let hidden = load_from(Some(&path));
        assert!(hidden.videos.is_empty(), "起動は止めず空で始める");
        assert!(hidden.channels.is_empty());
        assert_eq!(hidden.path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn ids_are_read_from_both_lists() {
        let dir = temp_dir("read");
        let path = dir.join("hidden.toml");
        fs::write(
            &path,
            "[[videos]]\nid = \"v1\"\ntitle = \"動画\"\n\n[[videos]]\nid = \"v2\"\ntitle = \"\"\n\n[[channels]]\nid = \"UC1\"\ntitle = \"ch\"\n",
        )
        .expect("書ける");

        let hidden = load_from(Some(&path));
        assert_eq!(
            hidden.videos,
            HashSet::from(["v1".to_string(), "v2".to_string()])
        );
        assert_eq!(hidden.channels, HashSet::from(["UC1".to_string()]));
    }

    #[test]
    fn a_missing_title_and_unknown_keys_are_tolerated() {
        let dir = temp_dir("tolerant");
        let path = dir.join("hidden.toml");
        // 古い版が将来のキーを拒まないように。title 無しの手書きも読む。
        fs::write(&path, "[[videos]]\nid = \"v1\"\nnote = \"手で書いた\"\n").expect("書ける");

        let hidden = load_from(Some(&path));
        assert_eq!(hidden.videos, HashSet::from(["v1".to_string()]));
    }

    #[test]
    fn hides_matches_the_video_id_and_the_channel_id() {
        let hidden = Hidden {
            videos: HashSet::from(["v1".to_string()]),
            channels: HashSet::from(["UC1".to_string()]),
            ..Hidden::default()
        };

        assert!(hidden.hides(&result("v1", None)), "動画 ID で外す");
        assert!(
            hidden.hides(&result("v9", Some("UC1"))),
            "チャンネル ID で外す"
        );
        assert!(!hidden.hides(&result("v9", Some("UC9"))));
        assert!(
            !hidden.hides(&result("v9", None)),
            "channel_id の無い行は残す"
        );
    }

    #[test]
    fn adding_writes_the_file_and_updates_the_set() {
        let dir = temp_dir("add");
        let path = dir.join("hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v1", "動画のタイトル").expect("書ける");
        hidden.add_channel("UC1", "チャンネル名").expect("書ける");

        assert_eq!(hidden.videos, HashSet::from(["v1".to_string()]));
        assert_eq!(hidden.channels, HashSet::from(["UC1".to_string()]));
        // 次の起動で読み直せる形になっている。
        let reloaded = load_from(Some(&path));
        assert_eq!(reloaded.videos, hidden.videos);
        assert_eq!(reloaded.channels, hidden.channels);
        let written = fs::read_to_string(&path).expect("読める");
        assert!(written.contains("動画のタイトル"), "{written}");
        assert!(written.contains("チャンネル名"), "{written}");
        assert_eq!(leftover_temp_files(&dir), Vec::<String>::new());
    }

    #[test]
    fn a_hand_written_file_without_a_final_newline_stays_readable_after_adding() {
        // 手で書いて最後の改行を付けずに保存したファイル。
        let dir = temp_dir("no-final-newline");
        let path = dir.join("hidden.toml");
        fs::write(&path, "[[videos]]\nid = \"v1\"\ntitle = \"手で足した\"").expect("書ける");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v2", "あとから").expect("書ける");

        let reloaded = load_from(Some(&path));
        assert_eq!(
            reloaded.videos,
            HashSet::from(["v1".to_string(), "v2".to_string()])
        );
        let written = fs::read_to_string(&path).expect("読める");
        assert!(
            written.contains("手で足した\"\n\n[[videos]]"),
            "間を 1 行空けて足す: {written}"
        );
    }

    /// 置き換えに使った一時ファイルの残り。名前はプロセスごとに変わる。
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
    fn the_temporary_file_is_named_per_process() {
        let dir = temp_dir("tmp-name");
        let path = dir.join("hidden.toml");
        // 隣の窓が置いていった書きかけ。名前がぶつかると、これを本番の位置へ移してしまう。
        let other = dir.join("hidden.toml.tmp.999999");
        fs::write(&other, "書きかけ [[[").expect("書ける");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v1", "動画").expect("書ける");

        assert!(other.exists(), "他のプロセスの一時ファイルを触らない");
        assert_eq!(
            load_from(Some(&path)).videos,
            HashSet::from(["v1".to_string()])
        );
    }

    #[test]
    fn a_broken_file_is_left_alone_instead_of_being_overwritten() {
        let dir = temp_dir("broken-add");
        let path = dir.join("hidden.toml");
        // 手で解除したときの打ち間違い。ここで全文を書き直すと v1/UC1 ごと消える。
        let text =
            "[[videos]]\nid = \"v1\"\n\n[[videos]\nid = \"v2\"\n\n[[channels]]\nid = \"UC1\"\n";
        fs::write(&path, text).expect("書ける");
        let mut hidden = load_from(Some(&path));

        let error = hidden.add_video("v9", "動画").expect_err("書かない");

        assert_eq!(error, BROKEN_HIDDEN_FILE);
        assert_eq!(fs::read_to_string(&path).expect("読める"), text);
        assert!(hidden.videos.is_empty(), "書けていないので覚えない");
    }

    #[test]
    fn adding_keeps_handwritten_comments_and_unknown_keys() {
        let dir = temp_dir("handwritten");
        let path = dir.join("hidden.toml");
        let text = "# 手で足した分\n[[videos]]\nid = \"v1\"\nnote = \"あとで見る\"\n";
        fs::write(&path, text).expect("書ける");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v2", "動画").expect("書ける");

        let written = fs::read_to_string(&path).expect("読める");
        assert!(
            written.starts_with(text),
            "元の中身をそのまま残す: {written}"
        );
        assert!(written.contains("v2"), "{written}");
        assert_eq!(
            load_from(Some(&path)).videos,
            HashSet::from(["v1".to_string(), "v2".to_string()])
        );
    }

    #[test]
    fn a_title_with_quotes_survives_the_round_trip() {
        let dir = temp_dir("quotes");
        let path = dir.join("hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden
            .add_channel("UC1", "引用符 \" と \\ を含む名前")
            .expect("書ける");

        let file: HiddenFile =
            toml::from_str(&fs::read_to_string(&path).expect("読める")).expect("読める");
        assert_eq!(file.channels[0].title, "引用符 \" と \\ を含む名前");
    }

    #[test]
    fn hides_falls_back_to_the_uploader_when_the_row_has_no_channel_id() {
        let mut hidden = Hidden {
            channels: HashSet::from(["UC1".to_string()]),
            channel_names: HashSet::from(["One Channel".to_string()]),
            ..Hidden::default()
        };
        let feed_row = SearchResult {
            uploader: Some("One Channel".to_string()),
            ..result("v9", None)
        };

        assert!(hidden.hides(&feed_row), "フィードの行も名前で外す");
        assert!(!hidden.hides(&result("v9", None)), "名前の無い行は残す");

        hidden.channel_names.clear();
        assert!(!hidden.hides(&feed_row), "隠していない名前は残す");
    }

    #[test]
    fn hiding_a_channel_remembers_its_name_for_rows_without_an_id() {
        let dir = temp_dir("channel-name");
        let path = dir.join("hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden.add_channel("UC1", "One Channel").expect("書ける");

        let feed_row = SearchResult {
            uploader: Some("One Channel".to_string()),
            ..result("v9", None)
        };
        assert!(hidden.hides(&feed_row));
        // 次の起動でも同じように外す。
        assert!(load_from(Some(&path)).hides(&feed_row));
    }

    #[test]
    fn a_channel_without_a_name_does_not_hide_rows_without_an_uploader() {
        let dir = temp_dir("channel-noname");
        let path = dir.join("hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden.add_channel("UC1", "").expect("書ける");

        assert!(hidden.channel_names.is_empty());
        assert!(!hidden.hides(&result("v9", None)));
        assert!(load_from(Some(&path)).channel_names.is_empty());
    }

    #[test]
    fn adding_the_same_id_twice_keeps_one_entry() {
        let dir = temp_dir("twice");
        let path = dir.join("hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v1", "1 回目").expect("書ける");
        hidden.add_video("v1", "2 回目").expect("書ける");

        let file: HiddenFile =
            toml::from_str(&fs::read_to_string(&path).expect("読める")).expect("読める");
        assert_eq!(file.videos.len(), 1);
        assert_eq!(file.videos[0].title, "1 回目", "先に書いた方を残す");
    }

    #[test]
    fn adding_keeps_the_entries_already_in_the_file() {
        let dir = temp_dir("keep");
        let path = dir.join("hidden.toml");
        fs::write(&path, "[[videos]]\nid = \"v0\"\ntitle = \"前からある\"\n").expect("書ける");
        let mut hidden = load_from(Some(&path));

        hidden.add_channel("UC1", "後から足す").expect("書ける");

        let reloaded = load_from(Some(&path));
        assert_eq!(reloaded.videos, HashSet::from(["v0".to_string()]));
        assert_eq!(reloaded.channels, HashSet::from(["UC1".to_string()]));
    }

    #[test]
    fn adding_creates_the_parent_directory() {
        let dir = temp_dir("nested");
        let path = dir.join("nested/hidden.toml");
        let mut hidden = load_from(Some(&path));

        hidden.add_video("v1", "動画").expect("親ごと作る");
        assert!(path.exists());
    }

    #[test]
    fn adding_without_a_path_reports_the_reason() {
        let mut hidden = load_from(None);
        let error = hidden.add_video("v1", "動画").expect_err("書けない");

        assert_eq!(error, NO_HIDDEN_PATH);
        assert!(hidden.videos.is_empty(), "書けていないので覚えない");
    }

    #[test]
    fn a_failed_write_leaves_the_set_unchanged() {
        let dir = temp_dir("blocked");
        let blocker = dir.join("blocked");
        fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");
        let mut hidden = load_from(Some(&blocker.join("hidden.toml")));

        assert!(hidden.add_video("v1", "動画").is_err());
        assert!(hidden.videos.is_empty(), "書けていないので覚えない");
    }
}
