//! 動画/音声ファイルのダウンロード (yt-dlp をそのまま使う。mpv は経由しない)。
//! yt-dlp の起動は Downloader 越しに呼び、テストでは偽物へ差し替える
//! (search::YtDlp や oauth::Backend と同じ考え方)。

use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::SystemTime;
use tokio::process::Command;

const DEBUG_LOG_FILE: &str = "download-debug.log";

/// 開始時に出す知らせの接頭辞。終わったときにこの知らせだけを畳むための目印。
pub const DOWNLOADING_PREFIX: &str = "ダウンロード中: ";

/// 本番は tokio Command、テストは台本どおりの Output を返す偽物。
pub trait Downloader {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send;
}

pub struct RealYtDlp;

impl Downloader for RealYtDlp {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
        // ダウンロード中にアプリを終了しても yt-dlp を孤児にしない。
        Command::new("yt-dlp")
            .args(args)
            .kill_on_drop(true)
            .output()
    }
}

/// 保存先の初期値。`[download] dir` があればそれ、無ければ `$HOME/Downloads`、
/// `$HOME` も無ければ None (空欄で始まる)。
pub fn default_dir(settings_dir: Option<&Path>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(dir) = settings_dir {
        return Some(dir.to_path_buf());
    }
    let home = home.filter(|value| !value.is_empty())?;
    Some(Path::new(home).join("Downloads"))
}

/// タイトルをそのままファイル名にすると `/` `\` でディレクトリを飛び出せてしまうため、
/// パス区切りを `_` に置き換える。
pub fn sanitize_filename(title: &str) -> String {
    title.replace(['/', '\\'], "_")
}

/// yt-dlp の引数。dir は展開済みの保存先、filename は拡張子を含まない。
pub fn download_args(url: &str, dir: &Path, filename: &str, audio_only: bool) -> Vec<String> {
    let template = dir
        .join(format!("{filename}.%(ext)s"))
        .to_string_lossy()
        .into_owned();
    let mut args = if audio_only {
        vec![
            "-x".to_string(),
            "--audio-format".to_string(),
            "mp3".to_string(),
        ]
    } else {
        vec!["-f".to_string(), "bestvideo+bestaudio/best".to_string()]
    };
    args.extend([
        "-o".to_string(),
        template,
        "--print".to_string(),
        "after_move:filepath".to_string(),
        url.to_string(),
    ]);
    args
}

/// `[download] debug` が有効なときのログの置き場。設定ファイルと同じ置き場に並べる。
pub fn debug_log_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    Some(crate::settings::app_config_dir(xdg_config_home, home)?.join(DEBUG_LOG_FILE))
}

/// ログへ書く 1 エントリ。yt-dlp が起動すらできなかった場合も書けるよう、
/// downloader.run の生の返り値をそのまま受け取る。
pub fn format_debug_entry(
    now: SystemTime,
    args: &[String],
    output: &std::io::Result<Output>,
) -> String {
    let timestamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (result, stdout, stderr) = match output {
        Ok(output) if output.status.success() => (
            "success (exit 0)".to_string(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Ok(output) => (
            format!("failed (exit {})", output.status.code().unwrap_or(-1)),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(e) => (format!("launch error: {e}"), String::new(), String::new()),
    };
    format!(
        "##### {timestamp} #####\nargs: {}\nresult: {result}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n\n",
        args.join(" ")
    )
}

/// 追記のみ。調べ物用のログなので、書けなくてもダウンロード自体は続行してよい
/// (呼び出し側が結果を無視できる形にする)。親ディレクトリが無ければ作る。
pub fn append_debug_log(path: &Path, entry: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(entry.as_bytes()).map_err(|e| e.to_string())
}

/// 失敗の文言にログの場所を添える。debug_log_path が None の間は今までと同じ文言のまま。
fn with_log_hint(message: String, debug_log_path: Option<&Path>) -> String {
    match debug_log_path {
        Some(path) => format!("{message} (詳細: {})", path.display()),
        None => message,
    }
}

/// 失敗時の文言に使う1行。yt-dlp は半年以上更新していないと必ず先頭に
/// バージョン警告を出すため、stderr をそのまま使うとステータス行(高さ1行)には
/// 警告だけが出て、後ろにある本当の理由 (ERROR: ...) が画面から消える。
/// "ERROR" を含む最初の行を優先し、無ければ最後の空でない行を使う。
fn failure_reason(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).collect();
    lines
        .iter()
        .find(|line| line.contains("ERROR"))
        .or_else(|| lines.iter().rev().find(|line| !line.is_empty()))
        .copied()
        .unwrap_or("")
        .to_string()
}

/// `--print after_move:filepath` が出す最後の行を保存済みパスとして読む。
/// 空行が続いても、最後に出た値のある行を採る。
pub fn extract_saved_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// 認証等を挟まない 1 回きりの保存。dir_text は入力欄の生文字列
/// (先頭 `~/` は settings::expand_home と同じ規則で `$HOME` へ展開する)。
/// debug_log_path が Some の間は、結果 (成功・失敗・起動エラーのいずれも) を
/// そこへ 1 エントリ追記する。書けなくてもダウンロード自体は続行する。
/// 返り値はそのまま画面へ出す文言 (Ok は set_temporary_notice、Err は set_error へ)。
pub async fn run<D: Downloader>(
    downloader: &D,
    dir_text: &str,
    filename: &str,
    audio_only: bool,
    url: &str,
    debug_log_path: Option<&Path>,
) -> Result<String, String> {
    let dir = crate::settings::expand_home(dir_text);
    let args = download_args(url, &dir, filename, audio_only);
    // debug が無効な間は今までと同じ挙動 (args の所有権をそのまま渡すだけ) にする。
    // ログ用に clone が要るのは debug_log_path が Some のときだけ。
    let raw = match debug_log_path {
        Some(path) => {
            let raw = downloader.run(args.clone()).await;
            let entry = format_debug_entry(SystemTime::now(), &args, &raw);
            let _ = append_debug_log(path, &entry);
            raw
        }
        None => downloader.run(args).await,
    };
    let output = raw.map_err(|e| {
        let reason = if e.kind() == ErrorKind::NotFound {
            "yt-dlp が見つかりません (PATH を確認してください)".to_string()
        } else {
            format!("yt-dlp の起動に失敗しました: {e}")
        };
        with_log_hint(
            format!("ダウンロードに失敗しました: {reason}"),
            debug_log_path,
        )
    })?;
    if output.status.success() {
        let path = extract_saved_path(&String::from_utf8_lossy(&output.stdout))
            .unwrap_or_else(|| dir.display().to_string());
        Ok(format!("保存しました: {path}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(with_log_hint(
            format!("ダウンロードに失敗しました: {}", failure_reason(&stderr)),
            debug_log_path,
        ))
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    pub fn done(code: i32, stdout: &str, stderr: &str) -> std::io::Result<Output> {
        Ok(Output {
            // ExitStatus は生の wait ステータスから作る (下位 8 bit はシグナル用)。
            status: ExitStatusExt::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    /// yt-dlp そのものが PATH に無い。
    pub fn missing() -> std::io::Result<Output> {
        Err(std::io::Error::from(ErrorKind::NotFound))
    }

    /// 起動せず台本どおりに振る舞う yt-dlp。呼ばれた引数を溜める。
    #[derive(Clone, Default)]
    pub struct FakeDownloader {
        script: Arc<Mutex<VecDeque<std::io::Result<Output>>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeDownloader {
        pub fn new(steps: impl IntoIterator<Item = std::io::Result<Output>>) -> Self {
            Self {
                script: Arc::new(Mutex::new(steps.into_iter().collect())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl Downloader for FakeDownloader {
        fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<Output>> + Send {
            self.calls.lock().expect("溜め込み先").push(args);
            let step = self.script.lock().expect("台本").pop_front();
            async move { step.unwrap_or_else(|| panic!("台本に無い呼び出し")) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn sanitize_filename_replaces_both_kinds_of_path_separator() {
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("普通のタイトル"), "普通のタイトル");
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn default_dir_prefers_the_configured_setting() {
        let configured = Path::new("/configured/dir");
        assert_eq!(
            default_dir(Some(configured), Some(OsStr::new("/home/x"))),
            Some(configured.to_path_buf())
        );
    }

    #[test]
    fn default_dir_falls_back_to_home_downloads() {
        assert_eq!(
            default_dir(None, Some(OsStr::new("/home/x"))),
            Some(PathBuf::from("/home/x/Downloads"))
        );
    }

    #[test]
    fn default_dir_is_none_without_a_setting_or_home() {
        assert_eq!(default_dir(None, None), None);
        // 空文字の HOME は無いものとして扱う。
        assert_eq!(default_dir(None, Some(OsStr::new(""))), None);
    }

    #[test]
    fn download_args_for_video_asks_for_the_best_combined_streams() {
        let args = download_args("https://x/watch?v=1", Path::new("/tmp/out"), "title", false);
        assert_eq!(
            args,
            vec![
                "-f",
                "bestvideo+bestaudio/best",
                "-o",
                "/tmp/out/title.%(ext)s",
                "--print",
                "after_move:filepath",
                "https://x/watch?v=1",
            ]
        );
    }

    #[test]
    fn download_args_for_audio_only_extracts_mp3() {
        let args = download_args("https://x/watch?v=1", Path::new("/tmp/out"), "title", true);
        assert_eq!(
            args,
            vec![
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                "/tmp/out/title.%(ext)s",
                "--print",
                "after_move:filepath",
                "https://x/watch?v=1",
            ]
        );
    }

    #[test]
    fn extract_saved_path_reads_the_last_non_empty_line() {
        assert_eq!(
            extract_saved_path("[download] done\n/tmp/out/title.mp4\n"),
            Some("/tmp/out/title.mp4".to_string())
        );
        // 末尾の空行に惑わされない。
        assert_eq!(
            extract_saved_path("/tmp/out/title.mp4\n\n\n"),
            Some("/tmp/out/title.mp4".to_string())
        );
    }

    #[test]
    fn extract_saved_path_is_none_for_blank_output() {
        assert_eq!(extract_saved_path(""), None);
        assert_eq!(extract_saved_path("\n  \n"), None);
    }

    #[tokio::test]
    async fn run_reports_the_printed_path_on_success() {
        let downloader = FakeDownloader::new([done(0, "/tmp/out/title.mp4\n", "")]);
        let notice = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect("成功");
        assert_eq!(notice, "保存しました: /tmp/out/title.mp4");
        assert_eq!(downloader.calls().len(), 1);
    }

    #[tokio::test]
    async fn run_falls_back_to_the_save_dir_when_no_path_was_printed() {
        let downloader = FakeDownloader::new([done(0, "", "")]);
        let notice = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect("成功");
        assert_eq!(notice, "保存しました: /tmp/out");
    }

    #[tokio::test]
    async fn run_expands_a_leading_tilde_in_the_directory() {
        let home = std::env::var("HOME").expect("HOME");
        let downloader = FakeDownloader::new([done(0, "", "")]);
        let notice = run(
            &downloader,
            "~/Movies",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect("成功");
        assert_eq!(notice, format!("保存しました: {home}/Movies"));
    }

    #[tokio::test]
    async fn run_reports_the_stderr_on_a_failed_exit() {
        let downloader = FakeDownloader::new([done(1, "", "ERROR: ffmpeg not found\n")]);
        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect_err("失敗");
        assert_eq!(error, "ダウンロードに失敗しました: ERROR: ffmpeg not found");
    }

    #[tokio::test]
    async fn run_surfaces_the_error_line_even_behind_an_update_warning() {
        // yt-dlp は半年以上更新していないと必ず先頭にこの警告を出す。ステータス行は
        // 1行しか出せないので、先頭行のままだと本当の理由 (ERROR:) が画面から消える。
        let stderr = "WARNING: Your yt-dlp version (2026.03.17) is older than 90 days!\n\
                       It is strongly recommended to always use the latest version.\n\
                       ERROR: unable to download video data: HTTP Error 403: Forbidden\n";
        let downloader = FakeDownloader::new([done(1, "", stderr)]);
        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect_err("失敗");
        assert_eq!(
            error,
            "ダウンロードに失敗しました: ERROR: unable to download video data: HTTP Error 403: Forbidden"
        );
    }

    #[tokio::test]
    async fn run_falls_back_to_the_last_line_without_an_error_marker() {
        let stderr = "WARNING: 何かの警告\n続きの説明行\n";
        let downloader = FakeDownloader::new([done(1, "", stderr)]);
        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect_err("失敗");
        assert_eq!(error, "ダウンロードに失敗しました: 続きの説明行");
    }

    #[tokio::test]
    async fn run_reports_a_launch_failure_when_yt_dlp_is_missing() {
        let downloader = FakeDownloader::new([missing()]);
        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect_err("失敗");
        assert!(error.contains("見つかりません"), "{error}");
    }

    #[tokio::test]
    async fn run_asks_for_mp3_when_audio_only_is_set() {
        let downloader = FakeDownloader::new([done(0, "/tmp/out/title.mp3\n", "")]);
        run(
            &downloader,
            "/tmp/out",
            "title",
            true,
            "https://x/v=1",
            None,
        )
        .await
        .expect("成功");
        let calls = downloader.calls();
        assert!(
            calls[0].contains(&"--audio-format".to_string()),
            "{calls:?}"
        );
        assert!(calls[0].contains(&"mp3".to_string()), "{calls:?}");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tuitube-download-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn debug_log_path_sits_next_to_the_config_file() {
        let config = crate::settings::config_path(None, Some(OsStr::new("/h"))).expect("config");
        let log = debug_log_path(None, Some(OsStr::new("/h"))).expect("log");
        assert_eq!(config.parent(), log.parent());
        assert_eq!(log.file_name(), Some(OsStr::new("download-debug.log")));
    }

    #[test]
    fn format_debug_entry_reports_success() {
        let args = vec!["-f".to_string(), "bestvideo+bestaudio/best".to_string()];
        let entry = format_debug_entry(
            SystemTime::UNIX_EPOCH,
            &args,
            &done(0, "/tmp/out/title.mp4\n", ""),
        );
        assert!(entry.starts_with("##### 0 #####\n"), "{entry}");
        assert!(
            entry.contains("args: -f bestvideo+bestaudio/best\n"),
            "{entry}"
        );
        assert!(entry.contains("result: success (exit 0)\n"), "{entry}");
        assert!(
            entry.contains("--- stdout ---\n/tmp/out/title.mp4"),
            "{entry}"
        );
        assert!(entry.contains("--- stderr ---\n"), "{entry}");
    }

    #[test]
    fn format_debug_entry_reports_a_failed_exit_code() {
        let entry = format_debug_entry(
            SystemTime::UNIX_EPOCH,
            &[],
            &done(1, "", "ERROR: ffmpeg not found\n"),
        );
        assert!(entry.contains("result: failed (exit 1)\n"), "{entry}");
        assert!(entry.contains("ERROR: ffmpeg not found"), "{entry}");
    }

    #[test]
    fn format_debug_entry_reports_a_launch_error() {
        let entry = format_debug_entry(SystemTime::UNIX_EPOCH, &[], &missing());
        assert!(entry.contains("result: launch error:"), "{entry}");
    }

    #[test]
    fn append_debug_log_creates_the_parent_directory_and_appends() {
        let dir = temp_dir("append");
        let path = dir.join("nested/download-debug.log");

        append_debug_log(&path, "first\n").expect("書ける");
        append_debug_log(&path, "second\n").expect("書ける");

        let written = fs::read_to_string(&path).expect("読める");
        assert_eq!(written, "first\nsecond\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_writes_a_debug_entry_when_a_path_is_given() {
        let dir = temp_dir("run-debug");
        let path = dir.join("download-debug.log");
        let downloader = FakeDownloader::new([done(0, "/tmp/out/title.mp4\n", "")]);

        run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            Some(&path),
        )
        .await
        .expect("成功");

        let written = fs::read_to_string(&path).expect("読める");
        assert!(written.contains("result: success (exit 0)"), "{written}");
        assert!(written.contains("https://x/v=1"), "{written}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_does_not_touch_the_filesystem_without_a_debug_path() {
        let dir = temp_dir("run-no-debug");
        let path = dir.join("download-debug.log");
        let downloader = FakeDownloader::new([done(0, "", "")]);

        run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            None,
        )
        .await
        .expect("成功");

        assert!(!path.exists(), "debug_log_path が無ければ書かない");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_adds_the_log_location_to_the_error_message_when_debugging() {
        let dir = temp_dir("run-debug-error");
        let path = dir.join("download-debug.log");
        let downloader = FakeDownloader::new([done(1, "", "ERROR: boom\n")]);

        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            Some(&path),
        )
        .await
        .expect_err("失敗");

        assert_eq!(
            error,
            format!(
                "ダウンロードに失敗しました: ERROR: boom (詳細: {})",
                path.display()
            )
        );
        let written = fs::read_to_string(&path).expect("読める");
        assert!(written.contains("result: failed (exit 1)"), "{written}");
        assert!(written.contains("ERROR: boom"), "{written}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_writes_a_debug_entry_for_a_launch_error_too() {
        let dir = temp_dir("run-debug-missing");
        let path = dir.join("download-debug.log");
        let downloader = FakeDownloader::new([missing()]);

        let error = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            Some(&path),
        )
        .await
        .expect_err("失敗");

        assert!(error.contains("見つかりません"), "{error}");
        assert!(
            error.contains(&format!("詳細: {}", path.display())),
            "{error}"
        );
        let written = fs::read_to_string(&path).expect("読める");
        assert!(written.contains("result: launch error:"), "{written}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_still_reports_the_download_result_when_the_debug_log_cannot_be_written() {
        let dir = temp_dir("run-debug-unwritable");
        // ログの親ディレクトリになるはずの場所に、あえてファイルを置いて
        // fs::create_dir_all (append_debug_log 内) を失敗させる。
        let blocker = dir.join("blocker");
        fs::write(&blocker, b"not a directory").expect("blocker file");
        let path = blocker.join("nested").join("download-debug.log");

        let downloader = FakeDownloader::new([done(0, "/tmp/out/title.mp4\n", "")]);
        let notice = run(
            &downloader,
            "/tmp/out",
            "title",
            false,
            "https://x/v=1",
            Some(&path),
        )
        .await
        .expect("ログ書き込みに失敗してもダウンロード自体の成否には影響しない");

        assert_eq!(notice, "保存しました: /tmp/out/title.mp4");
        assert!(
            !path.exists(),
            "書き込みに失敗しているのでログファイルは作られていないはず"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
