//! 再生位置の記憶 (resume.toml) のパス決定・読み込み・更新。
//! hidden.rs と違い、この内容は完全に内部状態で手編集する想定がない (手書きの保護は不要)。
//! そのため書き込みは追記でなく、配列を丸ごと読み直して該当 ID を差し替えてから丸ごと書き直す。

use crate::settings;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const RESUME_FILE: &str = "resume.toml";

/// 保持する上限件数。超えたら最も古く更新されたものから捨てる。
const RESUME_CAPACITY: usize = 500;

/// これ以上の割合を見ていたら視聴完了とみなす。
const COMPLETE_RATIO: f64 = 0.95;

/// これより短い動画は途中からの再開に意味が薄いので、常に完了扱いにする。
const SHORT_VIDEO_SECS: f64 = 60.0;

/// これより手前の位置は最初からと変わらないので記憶しない。
const MIN_RESUME_SECS: f64 = 10.0;

/// 置き場を決められない環境 ($HOME も $XDG_CONFIG_HOME も無い) の理由。
pub const NO_RESUME_PATH: &str = "再生位置の置き場が分かりません ($HOME を設定してください)";

/// 再生位置 1 件。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Entry {
    id: String,
    position_secs: f64,
    duration_secs: f64,
}

impl Entry {
    /// 視聴完了とみなすか (95% 以上見た、または 60 秒未満の動画)。
    fn is_complete(&self) -> bool {
        self.position_secs >= self.duration_secs * COMPLETE_RATIO
            || self.duration_secs < SHORT_VIDEO_SECS
    }
}

/// ファイルの生の形。未知のキーは無視する。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
struct ResumeFile {
    #[serde(default)]
    videos: Vec<Entry>,
}

/// 読み込んだ再生位置と、その書き込み先。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Resume {
    entries: Vec<Entry>,
    /// 書き込み先。決められない環境では None。
    path: Option<PathBuf>,
}

impl Resume {
    /// 完了していない場合だけ再開位置を返す。
    pub fn lookup(&self, id: &str) -> Option<f64> {
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        (!entry.is_complete()).then_some(entry.position_secs)
    }

    /// 完了なら消す (既にあれば)、未完了なら上限を見て upsert する。
    /// 他の窓が足した分を消さないよう、書く前にファイルを丸ごと読み直す。
    pub fn remember(
        &mut self,
        id: &str,
        position_secs: f64,
        duration_secs: f64,
    ) -> Result<(), String> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| NO_RESUME_PATH.to_string())?;
        let before = read(&path).videos;
        let mut entries = before.clone();
        entries.retain(|entry| entry.id != id);
        let entry = Entry {
            id: id.to_string(),
            position_secs,
            duration_secs,
        };
        if !entry.is_complete() && position_secs >= MIN_RESUME_SECS {
            entries.push(entry);
            while entries.len() > RESUME_CAPACITY {
                entries.remove(0);
            }
        }
        // 何も変わらないなら書かない (要らないファイルを作らない)。
        if entries != before {
            write(&path, &entries)?;
        }
        self.entries = entries;
        Ok(())
    }
}

/// 設定ファイルと同じ置き場に並べる。
pub fn resume_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(settings::app_config_dir(xdg_config_home, home)?.join(RESUME_FILE))
}

/// 読めない・壊れているファイルは空として扱う。手編集を想定しないので、
/// 起動が止まらないことの方を優先し、hidden.rs の追記時のような判読エラーは返さない。
fn read(path: &Path) -> ResumeFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let file = ResumeFile {
        videos: entries.to_vec(),
    };
    let text = toml::to_string(&file).map_err(|e| e.to_string())?;
    write_atomically(path, &text)
}

/// 途中で落ちても壊れたファイルを残さないよう、一時ファイルへ書いてから置き換える。
/// 一時ファイルの名前はプロセスごとに分ける。2 つ起動して同時に書いても、
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

pub fn load_from(path: Option<&Path>) -> Resume {
    let Some(path) = path else {
        return Resume::default();
    };
    Resume {
        entries: read(path).videos,
        path: Some(path.to_path_buf()),
    }
}

pub fn load() -> Resume {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    load_from(resume_path(xdg.as_deref(), home.as_deref()).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-resume-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
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
    fn resume_path_prefers_xdg_config_home_then_home() {
        assert_eq!(
            resume_path(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/tuitube/resume.toml"))
        );
        assert_eq!(
            resume_path(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/resume.toml"))
        );
        // 空文字は未設定と同じに扱う。
        assert_eq!(
            resume_path(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/tuitube/resume.toml"))
        );
        assert_eq!(resume_path(None, None), None);
    }

    #[test]
    fn the_resume_file_sits_next_to_the_config_file() {
        let config = settings::config_path(None, Some(OsStr::new("/h"))).expect("config");
        let resume = resume_path(None, Some(OsStr::new("/h"))).expect("resume");
        assert_eq!(config.parent(), resume.parent());
    }

    #[test]
    fn a_missing_file_yields_no_resume_position_without_creating_it() {
        let dir = temp_dir("missing");
        let path = dir.join("nested/resume.toml");
        let resume = load_from(Some(&path));

        assert_eq!(resume.lookup("v1"), None);
        // 設定と違い、記憶するまでは要らないファイルなので作らない。
        assert!(!path.exists());
    }

    #[test]
    fn without_a_path_lookup_is_always_none() {
        let resume = load_from(None);
        assert_eq!(resume, Resume::default());
        assert_eq!(resume.lookup("v1"), None);
    }

    #[test]
    fn a_broken_file_yields_no_resume_position() {
        let dir = temp_dir("broken");
        let path = dir.join("resume.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書ける");

        let resume = load_from(Some(&path));
        assert_eq!(resume.lookup("v1"), None, "起動は止めず空で始める");
    }

    #[test]
    fn remembering_a_position_can_be_looked_up_after_reloading() {
        let dir = temp_dir("remember");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));

        resume.remember("v1", 123.4, 600.0).expect("書ける");

        assert_eq!(resume.lookup("v1"), Some(123.4));
        // 次の起動でも読み直せる形になっている。
        assert_eq!(load_from(Some(&path)).lookup("v1"), Some(123.4));
        assert_eq!(leftover_temp_files(&dir), Vec::<String>::new());
    }

    #[test]
    fn a_position_under_ten_seconds_is_not_remembered() {
        let dir = temp_dir("too-early");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));

        resume.remember("v1", 9.9, 600.0).expect("書ける");

        assert_eq!(resume.lookup("v1"), None);
        assert!(!path.exists(), "書くものが無いのでファイルも作らない");
    }

    #[test]
    fn reaching_the_completion_ratio_removes_the_entry() {
        let dir = temp_dir("complete-ratio");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));
        resume.remember("v1", 100.0, 600.0).expect("書ける");

        resume.remember("v1", 570.0, 600.0).expect("書ける");

        assert_eq!(resume.lookup("v1"), None, "95% 以上見たら完了");
        assert_eq!(load_from(Some(&path)).lookup("v1"), None);
    }

    #[test]
    fn a_short_video_is_always_complete() {
        let dir = temp_dir("short-video");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));

        resume.remember("v1", 30.0, 59.0).expect("書ける");

        assert_eq!(
            resume.lookup("v1"),
            None,
            "60 秒未満は途中からの再開の意味が薄い"
        );
    }

    #[test]
    fn updating_an_existing_id_replaces_it_instead_of_duplicating() {
        let dir = temp_dir("upsert");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));
        resume.remember("v1", 50.0, 600.0).expect("書ける");

        resume.remember("v1", 80.0, 600.0).expect("書ける");

        assert_eq!(resume.lookup("v1"), Some(80.0));
        let file: ResumeFile =
            toml::from_str(&fs::read_to_string(&path).expect("読める")).expect("読める");
        assert_eq!(file.videos.len(), 1, "同じ id は 1 件だけ残す");
    }

    #[test]
    fn updating_an_entry_moves_it_to_the_newest_end() {
        let dir = temp_dir("reorder");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));
        resume.remember("v1", 50.0, 600.0).expect("書ける");
        resume.remember("v2", 50.0, 600.0).expect("書ける");

        // v1 を更新し直すと、並びの末尾 (最新) へ移る。
        resume.remember("v1", 60.0, 600.0).expect("書ける");

        let file: ResumeFile =
            toml::from_str(&fs::read_to_string(&path).expect("読める")).expect("読める");
        assert_eq!(file.videos.last().map(|e| e.id.as_str()), Some("v1"));
    }

    #[test]
    fn exceeding_the_capacity_drops_the_oldest_entry() {
        let dir = temp_dir("capacity");
        let path = dir.join("resume.toml");
        let mut resume = load_from(Some(&path));
        for i in 0..RESUME_CAPACITY {
            resume
                .remember(&format!("v{i}"), 50.0, 600.0)
                .expect("書ける");
        }

        resume.remember("new", 50.0, 600.0).expect("書ける");

        assert_eq!(resume.lookup("v0"), None, "一番古い分を先頭から捨てる");
        assert_eq!(resume.lookup("new"), Some(50.0));
        let file: ResumeFile =
            toml::from_str(&fs::read_to_string(&path).expect("読める")).expect("読める");
        assert_eq!(file.videos.len(), RESUME_CAPACITY);
    }

    #[test]
    fn remembering_without_a_path_reports_the_reason() {
        let mut resume = load_from(None);
        let error = resume.remember("v1", 50.0, 600.0).expect_err("書けない");

        assert_eq!(error, NO_RESUME_PATH);
        assert_eq!(resume.lookup("v1"), None, "書けていないので覚えない");
    }

    #[test]
    fn a_failed_write_leaves_the_stored_entries_unchanged() {
        let dir = temp_dir("blocked");
        let blocker = dir.join("blocked");
        fs::write(&blocker, "ファイルなので中に書けない").expect("書ける");
        let mut resume = load_from(Some(&blocker.join("resume.toml")));

        assert!(resume.remember("v1", 50.0, 600.0).is_err());
        assert_eq!(resume.lookup("v1"), None, "書けていないので覚えない");
    }

    #[test]
    fn the_temporary_file_is_named_per_process() {
        let dir = temp_dir("tmp-name");
        let path = dir.join("resume.toml");
        // 隣の窓が置いていった書きかけ。名前がぶつかると、これを本番の位置へ移してしまう。
        let other = dir.join("resume.toml.tmp.999999");
        fs::write(&other, "書きかけ [[[").expect("書ける");
        let mut resume = load_from(Some(&path));

        resume.remember("v1", 50.0, 600.0).expect("書ける");

        assert!(other.exists(), "他のプロセスの一時ファイルを触らない");
        assert_eq!(load_from(Some(&path)).lookup("v1"), Some(50.0));
    }
}
