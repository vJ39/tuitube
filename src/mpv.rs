use crate::app::AppEvent;
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
/// ソースの fps。上限を超えるときだけフィルタを足すので、値が取れるまで聞き続ける。
pub const REQ_CONTAINER_FPS: u64 = 5;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// quit を送ってから SIGKILL に切り替えるまでの猶予。
const QUIT_GRACE: Duration = Duration::from_secs(2);
/// fps 上限の既定値。kitty 出力は 1 フレームごとに画素を CPU で作るため、
/// 制限しないと再生が重くなる (実測 CPU 122% → 40%)。
pub const DEFAULT_FPS_LIMIT: u32 = 15;
/// 受け付ける上限値。桁を打ち間違えた値をそのまま渡すと、複製フレームで端末とパイプが詰まる。
const MAX_FPS_LIMIT: u32 = 120;
/// fps 上限を指定する環境変数。0 か unlimited で制限を外す。
const FPS_LIMIT_VAR: &str = "TUITUBE_FPS_LIMIT";

/// 環境変数 TUITUBE_FPS_LIMIT の解釈結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpsLimit {
    /// None は制限なし。
    pub limit: Option<u32>,
    /// 指定値をそのまま採らなかったときだけ入る、利用者への表示文。
    pub notice: Option<String>,
}

impl Default for FpsLimit {
    fn default() -> Self {
        Self {
            limit: Some(DEFAULT_FPS_LIMIT),
            notice: None,
        }
    }
}

impl FpsLimit {
    pub fn from_env() -> Self {
        Self::parse(std::env::var(FPS_LIMIT_VAR).ok().as_deref())
    }

    /// 壊れた値で再生できなくなる方が困るので、起動は止めず既定値に倒す。
    /// 黙って倒すと「設定したのに効かない」に気づけないので、理由を notice に残す。
    fn parse(raw: Option<&str>) -> Self {
        let raw = raw.unwrap_or_default().trim();
        if raw.is_empty() {
            return Self::default();
        }
        if raw.eq_ignore_ascii_case("unlimited") {
            return Self::unlimited();
        }
        match raw.parse::<u32>() {
            Ok(0) => Self::unlimited(),
            Ok(fps) if fps <= MAX_FPS_LIMIT => Self {
                limit: Some(fps),
                notice: None,
            },
            Ok(fps) => Self {
                limit: Some(MAX_FPS_LIMIT),
                notice: Some(format!(
                    "{FPS_LIMIT_VAR}={fps} は上限の {MAX_FPS_LIMIT} に丸めました"
                )),
            },
            Err(_) => Self {
                limit: Some(DEFAULT_FPS_LIMIT),
                notice: Some(format!(
                    "{FPS_LIMIT_VAR}={raw} を数値として読めません。{DEFAULT_FPS_LIMIT} fps で再生します"
                )),
            },
        }
    }

    fn unlimited() -> Self {
        Self {
            limit: None,
            notice: None,
        }
    }
}

/// fps 上限の適用状態。ソースの fps が分かるまで判定を持ち越し、判定は 1 回だけ行う。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FpsFilter {
    limit: Option<u32>,
    decided: bool,
}

impl FpsFilter {
    fn new(limit: Option<u32>) -> Self {
        Self {
            limit,
            decided: limit.is_none(),
        }
    }

    /// ソースの fps をまだ聞く必要があるか。
    fn wants_source_fps(&self) -> bool {
        !self.decided
    }

    /// ソースの fps が分かった時点で、足すべきコマンドがあれば返す。
    fn decide(&mut self, source_fps: f64) -> Option<MpvCommand> {
        if self.decided {
            return None;
        }
        self.decided = true;
        let limit = self.limit?;
        // fps フィルタは上限ではなく定レート変換で、上限より遅いソースではフレームを複製する
        // (実測: 2fps のソースを 2 秒再生して 5 → 25 フレーム)。速いソースにだけ付ける。
        (source_fps > f64::from(limit)).then(|| add_fps_filter(limit))
    }
}

/// 利用者の mpv.conf にある vf 設定を消さないよう、置換 (--vf=) ではなく追加で入れる。
fn add_fps_filter(fps: u32) -> MpvCommand {
    MpvCommand {
        command: vec![json!("vf"), json!("add"), json!(format!("fps={fps}"))],
        request_id: None,
    }
}

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

fn set_property(name: &str, value: Value) -> MpvCommand {
    MpvCommand {
        command: vec![json!("set_property"), json!(name), value],
        request_id: None,
    }
}

/// VO のオプションは生成時にしか読まれないので、書き換えるだけでは反映されない (実測)。
/// 映像トラックを外して入れ直すと VO が作り直され、新しい寸法で描き始める。
pub fn resize_video(geometry: Geometry) -> [MpvCommand; 6] {
    [
        set_property("vo-kitty-cols", json!(geometry.area.width.max(1))),
        set_property("vo-kitty-rows", json!(geometry.area.height.max(1))),
        set_property("vo-kitty-width", json!(geometry.frame_px.0)),
        set_property("vo-kitty-height", json!(geometry.frame_px.1)),
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

/// 起動引数の組み立て。extra は URL の直前に入る (cookie 指定など)。
pub fn launch_args(
    socket: &Path,
    log: &Path,
    geometry: Geometry,
    extra: &[String],
    url: &str,
) -> Vec<String> {
    // --no-terminal: mpv shares this terminal and its status line would corrupt the TUI.
    // --log-file: そのぶん失われる失敗理由の受け皿。
    // --vo-kitty-*: stdout がパイプだと端末サイズを取得できず既定値に落ちるため全て明示する。
    let mut args = vec![
        format!("--input-ipc-server={}", socket.display()),
        format!("--log-file={}", log.display()),
        "--no-terminal".to_string(),
        "--vo=kitty".to_string(),
    ];
    args.extend(geometry.mpv_args());
    args.extend_from_slice(extra);
    args.push(url.to_string());
    args
}

pub struct MpvController {
    writer: OwnedWriteHalf,
    socket_path: PathBuf,
    log_path: PathBuf,
    fps: FpsFilter,
    /// 送信側を落とすと終了待ちタスクが猶予後に mpv を kill する。
    _kill: oneshot::Sender<()>,
}

impl MpvController {
    pub async fn launch(
        url: &str,
        nonce: u64,
        events: UnboundedSender<AppEvent>,
        video: VideoSink,
        fps_limit: Option<u32>,
        extra: &[String],
    ) -> Result<Self, String> {
        let dir = socket_dir()?;
        let socket_path = socket_path(&dir, nonce);
        let log_path = log_path(&dir, nonce);
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&log_path);

        let mut child = Command::new("mpv")
            .args(launch_args(
                &socket_path,
                &log_path,
                video.geometry(),
                extra,
                url,
            ))
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
            fps: FpsFilter::new(fps_limit),
            _kill: kill_tx,
        })
    }

    pub async fn send(&mut self, command: &MpvCommand) -> Result<(), String> {
        self.writer
            .write_all(command.to_line().as_bytes())
            .await
            .map_err(|e| format!("mpv への送信に失敗しました: {e}"))
    }

    pub async fn poll_properties(&mut self) -> Result<(), String> {
        for (name, id) in [
            ("time-pos", REQ_TIME_POS),
            ("duration", REQ_DURATION),
            ("pause", REQ_PAUSE),
            ("volume", REQ_VOLUME),
        ] {
            self.send(&get_property(name, id)).await?;
        }
        // 読み込み前は値が返らないので、決まるまで毎回聞く。決まったら聞かない。
        if self.fps.wants_source_fps() {
            self.send(&get_property("container-fps", REQ_CONTAINER_FPS))
                .await?;
        }
        Ok(())
    }

    /// ソースの fps が分かった時点で、上限を超えるときだけ fps フィルタを足す。
    /// 判定は 1 回だけなので、二重に足さない。
    pub async fn limit_fps(&mut self, source_fps: f64) -> Result<(), String> {
        let Some(command) = self.fps.decide(source_fps) else {
            return Ok(());
        };
        self.send(&command).await
    }

    /// 端末リサイズに合わせて映像の寸法を作り直す。
    pub async fn resize_video(&mut self, geometry: Geometry) -> Result<(), String> {
        for command in resize_video(geometry) {
            self.send(&command).await?;
        }
        Ok(())
    }

    /// quit を送って手放す。届かなくても終了待ちタスクが猶予後に kill するので取り残さない。
    pub async fn shutdown(mut self) {
        let _ = self.send(&quit()).await;
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
    fn fps_limit_defaults_when_the_variable_is_unset_or_empty() {
        assert_eq!(DEFAULT_FPS_LIMIT, 15);
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(FpsLimit::parse(raw), FpsLimit::default());
        }
        assert_eq!(FpsLimit::default().limit, Some(DEFAULT_FPS_LIMIT));
        assert!(FpsLimit::default().notice.is_none());
    }

    #[test]
    fn fps_limit_reads_an_explicit_value_without_a_notice() {
        for (raw, fps) in [("30", 30), (" 24 ", 24), ("120", MAX_FPS_LIMIT)] {
            let parsed = FpsLimit::parse(Some(raw));
            assert_eq!(parsed.limit, Some(fps));
            assert_eq!(parsed.notice, None, "{raw} で注意書きは要らない");
        }
    }

    #[test]
    fn fps_limit_is_disabled_by_zero_or_unlimited() {
        for raw in ["0", "unlimited", "UNLIMITED"] {
            let parsed = FpsLimit::parse(Some(raw));
            assert_eq!(parsed.limit, None, "{raw} は制限なしのはず");
            assert_eq!(parsed.notice, None);
        }
    }

    #[test]
    fn fps_limit_clamps_a_value_above_the_maximum_and_says_so() {
        // 桁を打ち間違えた値を素通しすると、複製フレームで端末とパイプが飽和する。
        for raw in ["121", "4294967295"] {
            let parsed = FpsLimit::parse(Some(raw));
            assert_eq!(parsed.limit, Some(MAX_FPS_LIMIT), "{raw} は丸めるはず");
            let notice = parsed.notice.expect("丸めた旨を出す");
            assert!(notice.contains(raw), "指定値が読み取れない: {notice}");
            assert!(notice.contains("120"), "丸めた先が読み取れない: {notice}");
        }
    }

    #[test]
    fn fps_limit_falls_back_to_the_default_on_invalid_values_and_says_so() {
        // 3O のような打ち間違いが黙って既定値になると、効かない理由に気づけない。
        for raw in ["abc", "3O", "-5", "12.5", "99999999999999999999"] {
            let parsed = FpsLimit::parse(Some(raw));
            assert_eq!(
                parsed.limit,
                Some(DEFAULT_FPS_LIMIT),
                "{raw} は既定値に落ちるはず"
            );
            let notice = parsed.notice.expect("読めなかった旨を出す");
            assert!(notice.contains(raw), "指定値が読み取れない: {notice}");
            assert!(notice.contains("15"), "採用値が読み取れない: {notice}");
        }
    }

    #[test]
    fn fps_filter_is_added_only_for_sources_faster_than_the_limit() {
        // fps フィルタは定レート変換なので、上限より遅いソースに付けるとフレームが増える。
        let mut slow = FpsFilter::new(Some(15));
        assert!(slow.wants_source_fps());
        assert_eq!(slow.decide(2.0), None);
        assert!(!slow.wants_source_fps());

        // ちょうど上限も付けない (複製は起きないが変換を挟む意味がない)。
        assert_eq!(FpsFilter::new(Some(15)).decide(15.0), None);

        let command = FpsFilter::new(Some(15)).decide(30.0).expect("足すはず");
        assert_eq!(
            command.to_line(),
            "{\"command\":[\"vf\",\"add\",\"fps=15\"]}\n"
        );
    }

    #[test]
    fn fps_filter_replaces_nothing_and_decides_only_once() {
        let mut filter = FpsFilter::new(Some(15));
        assert!(filter.decide(60.0).is_some());
        // 2 回目の container-fps で二重に足さない。
        assert_eq!(filter.decide(60.0), None);
        assert!(!filter.wants_source_fps());
        // vf add は追加なので、利用者の mpv.conf の vf 設定を置き換えない。
        assert!(add_fps_filter(15).to_line().contains("\"add\""));
    }

    #[test]
    fn fps_filter_never_asks_for_the_source_fps_without_a_limit() {
        let mut unlimited = FpsFilter::new(None);
        assert!(!unlimited.wants_source_fps());
        assert_eq!(unlimited.decide(240.0), None);
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
    fn launch_args_place_extra_once_right_before_the_url() {
        let extra = ["--ytdl-raw-options-append=cookies-from-browser=chrome:Profile 1".to_string()];
        let args = launch_args(
            Path::new("/tmp/mpv-1.sock"),
            Path::new("/tmp/mpv-1.log"),
            test_geometry(80, 22),
            &extra,
            "https://www.youtube.com/watch?v=abc",
        );

        assert_eq!(args[0], "--input-ipc-server=/tmp/mpv-1.sock");
        assert_eq!(args[1], "--log-file=/tmp/mpv-1.log");
        assert!(args.contains(&"--vo=kitty".to_string()));
        assert!(args.contains(&"--vo-kitty-cols=80".to_string()));
        let url = args.len() - 1;
        assert_eq!(args[url], "https://www.youtube.com/watch?v=abc");
        assert_eq!(args[url - 1], extra[0]);
        assert_eq!(args.iter().filter(|a| *a == &extra[0]).count(), 1);

        // extra が無ければ従来の引数のまま。
        let plain = launch_args(
            Path::new("/tmp/mpv-1.sock"),
            Path::new("/tmp/mpv-1.log"),
            test_geometry(80, 22),
            &[],
            "url",
        );
        assert_eq!(plain.len(), args.len() - 1);
        assert_eq!(plain.last().map(String::as_str), Some("url"));
        assert!(!plain.iter().any(|a| a.contains("cookies-from-browser")));
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
