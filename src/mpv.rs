use crate::app::AppEvent;
use crate::display::LaunchPlan;
use crate::video::{Geometry, VideoSink};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};

pub const REQ_TIME_POS: u64 = 1;
pub const REQ_DURATION: u64 = 2;
pub const REQ_PAUSE: u64 = 3;
pub const REQ_VOLUME: u64 = 4;
/// 実際に使われている VO。5 は container-fps に使っていたので再利用しない。
pub const REQ_CURRENT_VO: u64 = 6;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// quit を送ってから SIGKILL に切り替えるまでの猶予。
const QUIT_GRACE: Duration = Duration::from_secs(2);
#[derive(Debug, Serialize, PartialEq)]
pub struct MpvCommand {
    command: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
}

impl MpvCommand {
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("MpvCommand is always serializable");
        line.push('\n');
        line
    }
}

pub fn cycle_pause() -> MpvCommand {
    MpvCommand {
        command: vec![json!("cycle"), json!("pause")],
        request_id: None,
    }
}

pub fn seek(seconds: i64) -> MpvCommand {
    MpvCommand {
        command: vec![json!("seek"), json!(seconds)],
        request_id: None,
    }
}

/// 絶対シーク。負値は「末尾から」、duration ちょうどは終了を意味するので、
/// 呼び出し側で seekbar::clamp_target を通した値を渡す。
pub fn seek_absolute(seconds: f64) -> MpvCommand {
    MpvCommand {
        command: vec![json!("seek"), json!(seconds), json!("absolute")],
        request_id: None,
    }
}

pub fn add_volume(delta: i64) -> MpvCommand {
    MpvCommand {
        command: vec![json!("add"), json!("volume"), json!(delta)],
        request_id: None,
    }
}

pub fn quit() -> MpvCommand {
    MpvCommand {
        command: vec![json!("quit")],
        request_id: None,
    }
}

pub fn set_property(name: &str, value: Value) -> MpvCommand {
    MpvCommand {
        command: vec![json!("set_property"), json!(name), value],
        request_id: None,
    }
}

/// ラベル付きで足したフィルタは、同じラベルで外せる。
pub fn vf_add(spec: &str) -> MpvCommand {
    MpvCommand {
        command: vec![json!("vf"), json!("add"), json!(spec)],
        request_id: None,
    }
}

pub fn vf_remove(label: &str) -> MpvCommand {
    MpvCommand {
        command: vec![json!("vf"), json!("remove"), json!(format!("@{label}"))],
        request_id: None,
    }
}

/// 毎秒のポーリングで聞くプロパティ。
pub fn poll_commands() -> Vec<MpvCommand> {
    [
        ("time-pos", REQ_TIME_POS),
        ("duration", REQ_DURATION),
        ("pause", REQ_PAUSE),
        ("volume", REQ_VOLUME),
        ("current-vo", REQ_CURRENT_VO),
    ]
    .iter()
    .map(|(name, id)| get_property(name, *id))
    .collect()
}

/// kitty VO のオプションは、起動引数 (--vo-kitty-cols) とプロパティ名 (vo-kitty-cols) が同じ綴り。
pub fn set_kitty_option(key: &str, value: Value) -> MpvCommand {
    set_property(&format!("vo-kitty-{key}"), value)
}

/// VO のオプションは生成時にしか読まれないので、書き換えるだけでは反映されない (実測)。
/// 映像トラックを外して入れ直すと VO が作り直され、新しい寸法で描き始める。
pub fn resize_video(geometry: Geometry) -> [MpvCommand; 6] {
    let [cols, rows, width, height] = geometry
        .size_options()
        .map(|(key, value)| set_kitty_option(key, value));
    [
        cols,
        rows,
        width,
        height,
        set_property("vid", json!("no")),
        set_property("vid", json!("auto")),
    ]
}

pub fn get_property(name: &str, request_id: u64) -> MpvCommand {
    MpvCommand {
        command: vec![json!("get_property"), json!(name)],
        request_id: Some(request_id),
    }
}

pub fn parse_response(line: &str) -> Option<(u64, Option<Value>)> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let request_id = value.get("request_id")?.as_u64()?;
    if value.get("error").and_then(Value::as_str) != Some("success") {
        return Some((request_id, None));
    }
    Some((request_id, value.get("data").cloned()))
}

fn runtime_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// 他ユーザーが先回りしてパスを作れないよう、ソケットは 0700 の専用ディレクトリに置く。
fn socket_dir_in(base: &Path) -> Result<PathBuf, String> {
    let dir = base.join(format!("tuitube-{}", std::process::id()));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => Ok(dir),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(&dir)
                .map_err(|e| format!("IPC ソケット用ディレクトリを確認できません: {e}"))?;
            if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
                return Err(format!(
                    "IPC ソケット用ディレクトリ {} が安全ではありません",
                    dir.display()
                ));
            }
            Ok(dir)
        }
        Err(e) => Err(format!("IPC ソケット用ディレクトリを作成できません: {e}")),
    }
}

pub fn socket_dir() -> Result<PathBuf, String> {
    socket_dir_in(&runtime_base())
}

pub fn socket_path(dir: &Path, nonce: u64) -> PathBuf {
    dir.join(format!("mpv-{nonce}.sock"))
}

pub fn log_path(dir: &Path, nonce: u64) -> PathBuf {
    dir.join(format!("mpv-{nonce}.log"))
}

/// SIGKILL やパニックで Drop が走らなかった過去プロセスの残骸を掃除する。
pub fn sweep_stale_sockets() {
    sweep_stale_sockets_in(&runtime_base());
}

fn sweep_stale_sockets_in(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("tuitube-")) else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if pid == std::process::id().to_string() {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| {
                SystemTime::now()
                    .duration_since(m)
                    .map(|age| age > STALE_AFTER)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("終了コード {code}"),
        None => "シグナルにより終了".to_string(),
    }
}

fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn with_detail(message: String, detail: &str) -> String {
    if detail.is_empty() {
        message
    } else {
        format!("{message}: {detail}")
    }
}

/// `[   0.04][e][stream] Failed to open x` から本文だけを取り出す。
fn strip_log_prefix(line: &str) -> &str {
    let mut rest = line.trim();
    while let Some(after) = rest.strip_prefix('[') {
        match after.find(']') {
            Some(end) => rest = after[end + 1..].trim_start(),
            None => break,
        }
    }
    rest
}

/// --no-terminal の mpv は stderr に何も出さないため、失敗理由はログファイルから拾う。
fn log_detail(path: &Path) -> String {
    const TAIL: u64 = 64 * 1024;
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TAIL {
        let _ = file.seek(SeekFrom::Start(len - TAIL));
    }
    let mut buffer = Vec::new();
    if file.read_to_end(&mut buffer).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buffer);
    // ytdl_hook の失敗は最後に「Failed to recognize file format.」へ化けるので、
    // 原因が書かれている最初の ERROR 行を先に探す。
    text.lines()
        .find(|line| line.contains("][ytdl_hook]") && strip_log_prefix(line).starts_with("ERROR:"))
        .or_else(|| {
            text.lines()
                .rfind(|line| line.contains("][e][") || line.contains("][fatal]["))
        })
        .map(|line| strip_log_prefix(line).to_string())
        .unwrap_or_default()
}

fn failure_detail(stderr_tail: String, log_path: &Path) -> String {
    if stderr_tail.is_empty() {
        log_detail(log_path)
    } else {
        stderr_tail
    }
}

/// 起動引数の組み立て。表示モードで変わるぶんは plan が持つ。
pub fn launch_args(socket: &Path, log: &Path, plan: &LaunchPlan, url: &str) -> Vec<String> {
    // --no-terminal: mpv shares this terminal and its status line would corrupt the TUI.
    // --log-file: そのぶん失われる失敗理由の受け皿。
    let mut args = vec![
        format!("--input-ipc-server={}", socket.display()),
        format!("--log-file={}", log.display()),
        "--no-terminal".to_string(),
    ];
    args.extend(plan.args());
    args.push(url.to_string());
    args
}

pub struct MpvController {
    writer: OwnedWriteHalf,
    socket_path: PathBuf,
    log_path: PathBuf,
    /// 送信側を落とすと終了待ちタスクが猶予後に mpv を kill する。
    _kill: oneshot::Sender<()>,
}

impl MpvController {
    pub async fn launch(
        url: &str,
        nonce: u64,
        events: UnboundedSender<AppEvent>,
        video: VideoSink,
        plan: &LaunchPlan,
    ) -> Result<Self, String> {
        let dir = socket_dir()?;
        let socket_path = socket_path(&dir, nonce);
        let log_path = log_path(&dir, nonce);
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&log_path);

        let mut child = Command::new("mpv")
            .args(launch_args(&socket_path, &log_path, plan, url))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    "mpv が見つかりません (PATH を確認してください)".to_string()
                } else {
                    format!("mpv の起動に失敗しました: {e}")
                }
            })?;

        // IPC 接続を待つ間にパイプが詰まって mpv が止まらないよう、先に読み手を立てる。
        spawn_video_reader(child.stdout.take(), video, events.clone(), nonce);

        let stream = match connect_with_retry(&socket_path, &mut child).await {
            Ok(stream) => stream,
            Err(e) => {
                let detail = failure_detail(kill_and_collect_stderr(child).await, &log_path);
                let _ = fs::remove_file(&socket_path);
                let _ = fs::remove_file(&log_path);
                return Err(with_detail(e, &detail));
            }
        };

        let (reader, writer) = stream.into_split();
        let reader_events = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some((id, data)) = parse_response(&line)
                    && reader_events
                        .send(AppEvent::MpvProperty { nonce, id, data })
                        .is_err()
                {
                    break;
                }
            }
        });

        // 終了要求を待てるよう、stderr の吸い出しは別タスクにする。
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut stderr_tail = String::new();
            if let Some(stderr) = stderr {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    if !line.is_empty() {
                        stderr_tail = line.to_string();
                    }
                }
            }
            stderr_tail
        });

        let (kill_tx, kill_rx) = oneshot::channel();
        let exit_socket = socket_path.clone();
        let exit_log = log_path.clone();
        tokio::spawn(async move {
            let status = wait_or_kill(&mut child, kill_rx).await;
            let stderr_tail = stderr_task.await.unwrap_or_default();
            let error = match status {
                Ok(status) if !status.success() => Some(with_detail(
                    format!("mpv が異常終了しました ({})", describe_status(status)),
                    &failure_detail(stderr_tail, &exit_log),
                )),
                Ok(_) => None,
                Err(e) => Some(format!("mpv の終了を確認できません: {e}")),
            };
            // Drop が走らない経路でも残骸を置かない。
            let _ = fs::remove_file(&exit_socket);
            let _ = fs::remove_file(&exit_log);
            let _ = events.send(AppEvent::MpvExited { nonce, error });
        });

        Ok(Self {
            writer,
            socket_path,
            log_path,
            _kill: kill_tx,
        })
    }

    pub async fn send(&mut self, command: &MpvCommand) -> Result<(), String> {
        self.writer
            .write_all(command.to_line().as_bytes())
            .await
            .map_err(|e| format!("mpv への送信に失敗しました: {e}"))
    }
}

/// 終了要求 (kill 送信側の drop を含む) が来たら、猶予を置いてから確実に落とす。
async fn wait_or_kill(
    child: &mut Child,
    mut kill: oneshot::Receiver<()>,
) -> std::io::Result<ExitStatus> {
    tokio::select! {
        status = child.wait() => return status,
        _ = &mut kill => {}
    }
    if let Ok(status) = timeout(QUIT_GRACE, child.wait()).await {
        return status;
    }
    let _ = child.start_kill();
    child.wait().await
}

impl Drop for MpvController {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.log_path);
        if let Some(dir) = self.socket_path.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// `--vo=kitty` の APC チャンクを解釈し続ける。実端末へはメインループだけが書く。
/// フレームが揃うたびに再描画を促す (これが無いと画面はティッカー任せの毎秒1回になる)。
fn spawn_video_reader(
    stdout: Option<ChildStdout>,
    video: VideoSink,
    events: UnboundedSender<AppEvent>,
    nonce: u64,
) {
    let Some(mut stdout) = stdout else {
        return;
    };
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buffer).await {
                // mpv が stdout を閉じた = 再生終了。終了は wait 側が通知する。
                Ok(0) => break,
                Ok(n) => {
                    if video.feed(&buffer[..n])
                        && video.request_redraw()
                        && events.send(AppEvent::VideoFrame { nonce }).is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // 黙って抜けると mpv だけが生き残るので、停止の判断は UI 側へ渡す。
                Err(e) => {
                    let _ = events.send(AppEvent::VideoError {
                        nonce,
                        error: format!("mpv の映像出力を読めません: {e}"),
                    });
                    break;
                }
            }
        }
    });
}

async fn kill_and_collect_stderr(mut child: Child) -> String {
    let _ = child.start_kill();
    match child.wait_with_output().await {
        Ok(output) => last_line(&String::from_utf8_lossy(&output.stderr)),
        Err(_) => String::new(),
    }
}

async fn connect_with_retry(path: &Path, child: &mut Child) -> Result<UnixStream, String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let connect_error = match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => e,
        };
        // mpv は終了してもソケットファイルを消さないので、接続エラーだけでは死活が分からない。
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "mpv が起動直後に終了しました ({})",
                    describe_status(status)
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("mpv の状態を確認できません: {e}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "mpv の IPC ソケットに接続できません: {connect_error}"
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayMode, FpsCap, WindowOptions};
    use crate::kitty::fixtures::frame;
    use crate::video::{CellSize, MAX_FRAME_PIXELS};
    use ratatui::layout::Rect;

    fn test_geometry(width: u16, height: u16) -> Geometry {
        Geometry::new(
            Rect::new(0, 0, width, height),
            CellSize {
                width_px: 8,
                height_px: 16,
            },
            MAX_FRAME_PIXELS,
        )
    }

    fn temp_base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tuitube-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp base");
        dir
    }

    #[test]
    fn serializes_pause_toggle() {
        assert_eq!(
            cycle_pause().to_line(),
            "{\"command\":[\"cycle\",\"pause\"]}\n"
        );
    }

    #[test]
    fn serializes_seek_both_directions() {
        assert_eq!(seek(-5).to_line(), "{\"command\":[\"seek\",-5]}\n");
        assert_eq!(seek(5).to_line(), "{\"command\":[\"seek\",5]}\n");
    }

    #[test]
    fn seek_absolute_serializes_with_the_absolute_flag() {
        assert_eq!(
            seek_absolute(83.5).to_line(),
            "{\"command\":[\"seek\",83.5,\"absolute\"]}\n"
        );
        assert_eq!(
            seek_absolute(30.0).to_line(),
            "{\"command\":[\"seek\",30.0,\"absolute\"]}\n"
        );
    }

    #[test]
    fn serializes_volume_change() {
        assert_eq!(
            add_volume(5).to_line(),
            "{\"command\":[\"add\",\"volume\",5]}\n"
        );
        assert_eq!(
            add_volume(-5).to_line(),
            "{\"command\":[\"add\",\"volume\",-5]}\n"
        );
    }

    #[test]
    fn serializes_quit() {
        assert_eq!(quit().to_line(), "{\"command\":[\"quit\"]}\n");
    }

    #[test]
    fn resize_sets_kitty_sizes_then_reinitializes_the_video() {
        let lines: Vec<String> = resize_video(test_geometry(80, 22))
            .iter()
            .map(|c| c.to_line())
            .collect();
        assert_eq!(
            lines,
            [
                "{\"command\":[\"set_property\",\"vo-kitty-cols\",80]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-rows\",22]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-width\",640]}\n",
                "{\"command\":[\"set_property\",\"vo-kitty-height\",352]}\n",
                // 寸法だけ変えても VO は作り直されないので、映像トラックを入れ直す。
                "{\"command\":[\"set_property\",\"vid\",\"no\"]}\n",
                "{\"command\":[\"set_property\",\"vid\",\"auto\"]}\n",
            ]
        );
    }

    #[test]
    fn get_property_carries_request_id() {
        assert_eq!(
            get_property("time-pos", REQ_TIME_POS).to_line(),
            "{\"command\":[\"get_property\",\"time-pos\"],\"request_id\":1}\n"
        );
    }

    #[test]
    fn parses_successful_response() {
        let parsed = parse_response(r#"{"data":12.5,"error":"success","request_id":1}"#);
        assert_eq!(parsed, Some((1, Some(json!(12.5)))));
    }

    #[test]
    fn parses_error_response_as_no_data() {
        let parsed = parse_response(r#"{"error":"property unavailable","request_id":2}"#);
        assert_eq!(parsed, Some((2, None)));
    }

    #[test]
    fn ignores_event_lines_and_garbage() {
        assert_eq!(parse_response(r#"{"event":"file-loaded"}"#), None);
        assert_eq!(parse_response("not json"), None);
        assert_eq!(parse_response(""), None);
    }

    #[test]
    fn socket_paths_are_unique_per_nonce() {
        let dir = PathBuf::from("/run/user/1000/tuitube-1");
        assert_ne!(socket_path(&dir, 1), socket_path(&dir, 2));
        assert_eq!(socket_path(&dir, 1), dir.join("mpv-1.sock"));
    }

    #[test]
    fn socket_dir_is_private_and_reusable() {
        let base = temp_base("private");
        let dir = socket_dir_in(&base).expect("first create");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(socket_dir_in(&base).expect("reuse"), dir);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn socket_dir_rejects_world_writable_directory() {
        let base = temp_base("hostile");
        let dir = base.join(format!("tuitube-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(socket_dir_in(&base).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn socket_dir_rejects_symlink() {
        let base = temp_base("symlink");
        let target = base.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        let dir = base.join(format!("tuitube-{}", std::process::id()));
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        assert!(socket_dir_in(&base).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sweep_keeps_fresh_and_unrelated_directories() {
        let base = temp_base("sweep");
        let fresh = base.join("tuitube-999999");
        let unrelated = base.join("other-1");
        fs::create_dir_all(&fresh).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        sweep_stale_sockets_in(&base);
        assert!(fresh.exists());
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_failure_reason_from_mpv_log() {
        let base = temp_base("log");
        let path = base.join("mpv-1.log");
        fs::write(
            &path,
            "[   0.00][v][cplayer] mpv v0.41.0\n\
             [   0.04][e][file] Cannot open file '/x.mkv': No such file or directory\n\
             [   0.04][e][stream] Failed to open /x.mkv.\n\
             [   0.05][v][cplayer] finished\n",
        )
        .unwrap();
        assert_eq!(log_detail(&path), "Failed to open /x.mkv.");
        assert_eq!(log_detail(&base.join("absent.log")), "");
        assert_eq!(
            failure_detail("direct stderr".to_string(), &path),
            "direct stderr"
        );
        assert_eq!(
            failure_detail(String::new(), &path),
            "Failed to open /x.mkv."
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn launch_args_put_the_plan_between_the_fixed_options_and_the_url() {
        let extra = ["--ytdl-raw-options-append=cookies-from-browser=chrome:Profile 1".to_string()];
        let plan = LaunchPlan {
            mode: DisplayMode::Embedded,
            geometry: test_geometry(80, 22),
            fps_cap: FpsCap::new(15),
            window: WindowOptions::default(),
            extra_args: extra.to_vec(),
        };
        let args = launch_args(
            Path::new("/tmp/mpv-1.sock"),
            Path::new("/tmp/mpv-1.log"),
            &plan,
            "https://www.youtube.com/watch?v=abc",
        );

        assert_eq!(args[0], "--input-ipc-server=/tmp/mpv-1.sock");
        assert_eq!(args[1], "--log-file=/tmp/mpv-1.log");
        assert_eq!(args[2], "--no-terminal");
        assert_eq!(args[3..args.len() - 1], plan.args()[..]);
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=abc")
        );
        // cookie 連携の引数は plan 経由で 1 回だけ入る。
        assert_eq!(args.iter().filter(|a| *a == &extra[0]).count(), 1);
    }

    #[test]
    fn poll_asks_for_current_vo_every_time() {
        let lines: Vec<String> = poll_commands().iter().map(|c| c.to_line()).collect();
        assert!(
            lines.contains(
                &"{\"command\":[\"get_property\",\"current-vo\"],\"request_id\":6}\n".to_string()
            ),
            "{lines:?}"
        );
        // 判定を持ち越すプロパティは無くなったので、毎回同じ列になる。
        let again: Vec<String> = poll_commands().iter().map(|c| c.to_line()).collect();
        assert_eq!(lines, again);
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn log_detail_prefers_the_ytdl_hook_error_line() {
        let base = temp_base("log-ytdl");
        let path = base.join("mpv-1.log");
        fs::write(
            &path,
            "[   0.03][v][cplayer] mpv v0.41.0\n\
             [   0.50][e][ytdl_hook] ERROR: could not find firefox cookies database in '/x/Profiles'\n\
             [   0.50][e][ytdl_hook] youtube-dl failed: unexpected error occurred\n\
             [   0.51][e][cplayer] Failed to recognize file format.\n",
        )
        .unwrap();
        assert_eq!(
            log_detail(&path),
            "ERROR: could not find firefox cookies database in '/x/Profiles'"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn log_without_error_lines_yields_no_detail() {
        let base = temp_base("log-clean");
        let path = base.join("mpv-1.log");
        fs::write(&path, "[   0.00][v][cplayer] mpv v0.41.0\n").unwrap();
        assert_eq!(log_detail(&path), "");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn keeps_last_non_empty_stderr_line() {
        assert_eq!(last_line("a\n\nb\n  \n"), "b");
        assert_eq!(last_line(""), "");
        assert_eq!(
            with_detail("mpv が異常終了しました".to_string(), "no such file"),
            "mpv が異常終了しました: no such file"
        );
        assert_eq!(with_detail("boom".to_string(), ""), "boom");
    }

    #[tokio::test]
    async fn video_reader_asks_for_a_redraw_once_per_frame() {
        // mpv が出すのと同じ APC チャンク列で2フレーム。
        let stream = [frame(2, 1, b"AAAA"), frame(4, 2, b"BBBB")].concat();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '%s' '{}'",
                String::from_utf8(stream).expect("ascii fixture")
            ))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn printf");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let video = VideoSink::new(test_geometry(80, 22));
        spawn_video_reader(child.stdout.take(), video.clone(), tx, 7);

        let event = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("redraw request must arrive")
            .expect("channel stays open");
        assert!(matches!(event, AppEvent::VideoFrame { nonce: 7 }));

        let _ = child.wait().await;
        // 通知を受けた側が片付けるまで、次の要求は畳まれる。
        assert!(!video.request_redraw());
        let frame = video
            .take()
            .and_then(|pending| pending.frame)
            .expect("最新フレームが残っているはず");
        assert_eq!((frame.width_px, frame.height_px), (4, 2));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_kills_a_child_that_ignores_the_request() {
        let mut child = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let (kill_tx, kill_rx) = oneshot::channel();
        // MpvController を手放した状況 (quit が届かなかった場合) を模す。
        drop(kill_tx);
        let status = wait_or_kill(&mut child, kill_rx).await.expect("wait");
        assert!(!status.success());
        assert_eq!(status.code(), None, "SIGKILL で落ちるはず");
    }

    #[tokio::test]
    async fn wait_or_kill_reports_the_real_exit_status() {
        let mut child = Command::new("false").spawn().expect("spawn false");
        let (kill_tx, kill_rx) = oneshot::channel();
        let status = wait_or_kill(&mut child, kill_rx).await.expect("wait");
        assert_eq!(status.code(), Some(1));
        drop(kill_tx);
    }

    #[tokio::test]
    async fn connect_gives_up_immediately_when_child_is_dead() {
        let base = temp_base("dead-child");
        let path = base.join("missing.sock");
        // ソケットファイルは残っているが mpv は既に落ちている状況を模す。
        fs::write(&path, b"").unwrap();
        let mut child = Command::new("false").spawn().expect("spawn false");
        let started = std::time::Instant::now();
        let error = connect_with_retry(&path, &mut child)
            .await
            .expect_err("dead child must not be retried");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "retried too long"
        );
        assert!(
            error.contains("起動直後に終了"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
