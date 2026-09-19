//! 動画/音声ファイルのダウンロード (yt-dlp をそのまま使う。mpv は経由しない)。
//! yt-dlp の起動は Downloader 越しに呼び、テストでは偽物へ差し替える
//! (search::YtDlp や oauth::Backend と同じ考え方)。

use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Output;
use tokio::process::Command;

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
/// 返り値はそのまま画面へ出す文言 (Ok は set_temporary_notice、Err は set_error へ)。
pub async fn run<D: Downloader>(
    downloader: &D,
    dir_text: &str,
    filename: &str,
    audio_only: bool,
    url: &str,
) -> Result<String, String> {
    let dir = crate::settings::expand_home(dir_text);
    let args = download_args(url, &dir, filename, audio_only);
    let output = downloader.run(args).await.map_err(|e| {
        let reason = if e.kind() == ErrorKind::NotFound {
            "yt-dlp が見つかりません (PATH を確認してください)".to_string()
        } else {
            format!("yt-dlp の起動に失敗しました: {e}")
        };
        format!("ダウンロードに失敗しました: {reason}")
    })?;
    if output.status.success() {
        let path = extract_saved_path(&String::from_utf8_lossy(&output.stdout))
            .unwrap_or_else(|| dir.display().to_string());
        Ok(format!("保存しました: {path}"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("ダウンロードに失敗しました: {}", stderr.trim()))
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
        let notice = run(&downloader, "/tmp/out", "title", false, "https://x/v=1")
            .await
            .expect("成功");
        assert_eq!(notice, "保存しました: /tmp/out/title.mp4");
        assert_eq!(downloader.calls().len(), 1);
    }

    #[tokio::test]
    async fn run_falls_back_to_the_save_dir_when_no_path_was_printed() {
        let downloader = FakeDownloader::new([done(0, "", "")]);
        let notice = run(&downloader, "/tmp/out", "title", false, "https://x/v=1")
            .await
            .expect("成功");
        assert_eq!(notice, "保存しました: /tmp/out");
    }

    #[tokio::test]
    async fn run_expands_a_leading_tilde_in_the_directory() {
        let home = std::env::var("HOME").expect("HOME");
        let downloader = FakeDownloader::new([done(0, "", "")]);
        let notice = run(&downloader, "~/Movies", "title", false, "https://x/v=1")
            .await
            .expect("成功");
        assert_eq!(notice, format!("保存しました: {home}/Movies"));
    }

    #[tokio::test]
    async fn run_reports_the_stderr_on_a_failed_exit() {
        let downloader = FakeDownloader::new([done(1, "", "ERROR: ffmpeg not found\n")]);
        let error = run(&downloader, "/tmp/out", "title", false, "https://x/v=1")
            .await
            .expect_err("失敗");
        assert_eq!(error, "ダウンロードに失敗しました: ERROR: ffmpeg not found");
    }

    #[tokio::test]
    async fn run_reports_a_launch_failure_when_yt_dlp_is_missing() {
        let downloader = FakeDownloader::new([missing()]);
        let error = run(&downloader, "/tmp/out", "title", false, "https://x/v=1")
            .await
            .expect_err("失敗");
        assert!(error.contains("見つかりません"), "{error}");
    }

    #[tokio::test]
    async fn run_asks_for_mp3_when_audio_only_is_set() {
        let downloader = FakeDownloader::new([done(0, "/tmp/out/title.mp3\n", "")]);
        run(&downloader, "/tmp/out", "title", true, "https://x/v=1")
            .await
            .expect("成功");
        let calls = downloader.calls();
        assert!(
            calls[0].contains(&"--audio-format".to_string()),
            "{calls:?}"
        );
        assert!(calls[0].contains(&"mp3".to_string()), "{calls:?}");
    }
}
