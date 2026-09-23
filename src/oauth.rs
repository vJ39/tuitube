//! YouTube の書き込み系 API (チャンネル登録・いいね) のための OAuth 2.0 (PKCE)。
//! curl / openssl / open とローカル TCP は Backend 越しに呼び、テストでは偽物へ差し替える。

use crate::rgb::base64_into;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

pub const SCOPE: &str = "https://www.googleapis.com/auth/youtube.force-ssl";
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const SUBSCRIPTIONS_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/subscriptions";
pub const RATE_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/videos/rate";
pub const VIDEOS_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/videos";
pub const PLAYLISTS_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/playlists";
pub const PLAYLIST_ITEMS_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/playlistItems";

const APP_DIR: &str = "tuitube";
const CLIENT_FILE: &str = "oauth_client.toml";
const TOKEN_FILE: &str = "oauth_token.toml";
/// code_verifier と state の元になる乱数の長さ。
const RANDOM_BYTES: usize = 32;
/// 1 回の HTTP 呼び出しの上限。ブラウザでの認可待ちはこれとは別。
const HTTP_TIMEOUT_SECS: u32 = 30;
/// ブラウザからのリダイレクトを待つ上限。これを過ぎたら待ち受けを畳む。
const AUTH_WAIT_SECS: u64 = 300;
/// リダイレクトのリクエスト行として読む上限。
const REQUEST_LINE_MAX: u64 = 8192;
/// いいね一覧 1 ページの件数。API の上限。
const LIKED_PAGE_SIZE: u32 = 50;
/// 辿るページ数の上限。myRating の一覧は 1000 件までしか返らないので、
/// 次ページが返り続けてもここで打ち切る。
const LIKED_PAGE_MAX: usize = 20;
/// 登録確認 1 回で渡すチャンネル ID の数。forChannelId の上限が公表されていないので安全側。
pub const CHANNEL_CHUNK: usize = 50;
/// 保存先のプレイリスト名。名前が完全に同じものを使い、無ければ作る。
const SAVE_PLAYLIST_TITLE: &str = "tuitube";
/// 自分のプレイリスト一覧 1 ページの件数。API の上限。
const PLAYLIST_PAGE_SIZE: u32 = 50;
/// 保存先を探すときに辿るページ数の上限。
const PLAYLIST_PAGE_MAX: usize = 20;

pub const SUBSCRIBE_NOTICE: &str = "チャンネル登録中…";
pub const LIKE_NOTICE: &str = "いいねを送信中…";
pub const SUBSCRIBED_NOTICE: &str = "チャンネル登録しました";
pub const LIKED_NOTICE: &str = "いいねしました";
pub const SAVE_NOTICE: &str = "tuitube に保存中…";
pub const SAVED_NOTICE: &str = "tuitube に保存しました";
pub const ALREADY_SAVED_NOTICE: &str = "既に tuitube に保存しています";
pub const STATE_MISMATCH: &str = "認可の応答が要求と一致しません";
pub const NO_CODE: &str = "認可コードを受け取れませんでした";
pub const NO_CONFIG_PATH: &str = "設定ファイルの置き場が分かりません ($HOME を設定してください)";
pub const TRANSPORT_FAILED: &str = "通信に失敗しました";
pub const AUTH_TIMED_OUT: &str = "認可の応答がありませんでした";
/// 資格情報のファイルが壊れているときの文言。中身は秘密なので理由に出さない。
pub const BROKEN_CLIENT: &str = "形式が不正です";

/// ブラウザから戻ってきたリダイレクトの中身。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// curl 1 回ぶんの応答。--fail を使わないので、エラーでも本文が入る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// curl 1 回ぶんの呼び出し。トークンや client_secret は argv に置くと同じ利用者の
/// 他プロセスから ps で読めるので、設定ファイルとして stdin から渡す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// そのまま argv に並べる引数。秘密は入れない。
    pub args: Vec<String>,
    /// `--config -` へ流すオプションと値の組。
    pub secrets: Vec<(String, String)>,
}

impl Request {
    /// stdin へ書く設定ファイルの中身。1 行 1 オプションで値は二重引用符で囲む。
    pub fn config(&self) -> String {
        self.secrets
            .iter()
            .map(|(option, value)| format!("{option} \"{}\"\n", config_escape(value)))
            .collect()
    }

    /// 宛先。引数の最後に置く。curl には args のまま渡すので、読むのは検証だけ。
    #[cfg(test)]
    pub fn url(&self) -> &str {
        self.args.last().map(String::as_str).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: String,
    pub secret: String,
}

/// トークン応答。refresh は初回の認可でしか返らない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Subscribe(String),
    Like(String),
    /// 動画を tuitube のプレイリストへ足す。
    Save(String),
}

impl Action {
    /// 認証・送信を待っている間に出す文言。
    pub fn notice(&self) -> &'static str {
        match self {
            Action::Subscribe(_) => SUBSCRIBE_NOTICE,
            Action::Like(_) => LIKE_NOTICE,
            Action::Save(_) => SAVE_NOTICE,
        }
    }

    fn done_notice(&self) -> &'static str {
        match self {
            Action::Subscribe(_) => SUBSCRIBED_NOTICE,
            Action::Like(_) => LIKED_NOTICE,
            Action::Save(_) => SAVED_NOTICE,
        }
    }
}

/// 資格情報とトークンの置き場。テストは一時ディレクトリを指す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub client: PathBuf,
    pub token: PathBuf,
}

impl Paths {
    pub fn from_env() -> Option<Self> {
        let xdg = std::env::var_os("XDG_CONFIG_HOME");
        let home = std::env::var_os("HOME");
        Some(Self {
            client: client_path(xdg.as_deref(), home.as_deref())?,
            token: token_path(xdg.as_deref(), home.as_deref())?,
        })
    }
}

/// 外側に触る操作の一式。本番は実プロセスとソケット、テストは台本どおりの偽物。
pub trait Backend: Send + Sync {
    fn random(&self, len: usize) -> Result<Vec<u8>, String>;
    fn sha256(&self, input: Vec<u8>) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
    fn open(&self, url: String) -> impl Future<Output = Result<(), String>> + Send;
    fn curl(&self, request: Request) -> impl Future<Output = Result<Response, String>> + Send;
    /// 待ち受けを始めて割り当てられたポートを返す。ブラウザを開く前に呼ぶ。
    fn bind(&self) -> Result<u16, String>;
    fn wait(&self) -> impl Future<Output = Result<Callback, String>> + Send;
}

// ---- 符号化 ----

/// RFC 4648 §5 の base64url。既存の base64 に通してから置換と除去をする。
pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::new();
    base64_into(bytes, &mut out);
    out.retain(|c| c != '=');
    out.replace('+', "-").replace('/', "_")
}

pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' if at + 2 < bytes.len() => match hex_pair(bytes[at + 1], bytes[at + 2]) {
                Some(byte) => {
                    out.push(byte);
                    at += 3;
                }
                None => {
                    out.push(bytes[at]);
                    at += 1;
                }
            },
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(high: u8, low: u8) -> Option<u8> {
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    Some(digit(high)? << 4 | digit(low)?)
}

pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ---- 認可 ----

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

pub fn auth_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = form_encode(&[
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        // 毎回 refresh_token を受け取るため。既に許可済みでも発行させる。
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    format!("{AUTH_ENDPOINT}?{query}")
}

/// `GET /callback?code=...&state=... HTTP/1.1` の 1 行から値を取る。
pub fn parse_callback(line: &str) -> Callback {
    let mut callback = Callback::default();
    let Some(target) = line.split_whitespace().nth(1) else {
        return callback;
    };
    let Some((_, query)) = target.split_once('?') else {
        return callback;
    };
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key {
            "code" => callback.code = Some(value),
            "state" => callback.state = Some(value),
            "error" => callback.error = Some(value),
            _ => {}
        }
    }
    callback
}

/// ブラウザへ返す 1 回きりの応答。
pub fn callback_response(ok: bool) -> Vec<u8> {
    let message = if ok {
        "認可が完了しました。このタブは閉じて構いません。"
    } else {
        "認可できませんでした。このタブは閉じて構いません。"
    };
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>tuitube</title><p>{message}</p>");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

// ---- curl ----

fn base_args() -> Vec<String> {
    vec![
        "-sS".to_string(),
        "--max-time".to_string(),
        HTTP_TIMEOUT_SECS.to_string(),
        // 秘密は argv に置かず、設定ファイルとして stdin から読ませる。
        "--config".to_string(),
        "-".to_string(),
        // エラー時の JSON 本文を読むので --fail は付けない。末尾にステータスを足して切り分ける。
        "-w".to_string(),
        "\n%{http_code}".to_string(),
    ]
}

/// curl の設定ファイルの二重引用符に収まる形にする。
fn config_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{b}' => out.push_str("\\v"),
            _ => out.push(c),
        }
    }
    out
}

fn bearer(access_token: &str) -> (String, String) {
    (
        "--header".to_string(),
        format!("Authorization: Bearer {access_token}"),
    )
}

pub fn token_request(body: &str) -> Request {
    let mut args = base_args();
    args.extend([
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        "Content-Type: application/x-www-form-urlencoded".to_string(),
        TOKEN_ENDPOINT.to_string(),
    ]);
    Request {
        args,
        // client_secret と refresh_token / 認可コードが入る。
        secrets: vec![("--data".to_string(), body.to_string())],
    }
}

pub fn subscribe_request(access_token: &str, channel_id: &str) -> Request {
    let body = serde_json::json!({
        "snippet": {"resourceId": {"kind": "youtube#channel", "channelId": channel_id}}
    })
    .to_string();
    let mut args = base_args();
    args.extend([
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        "Content-Type: application/json".to_string(),
        format!("{SUBSCRIPTIONS_ENDPOINT}?part=snippet"),
    ]);
    Request {
        args,
        secrets: vec![bearer(access_token), ("--data".to_string(), body)],
    }
}

pub fn rate_request(access_token: &str, video_id: &str) -> Request {
    let mut args = base_args();
    args.extend([
        "-X".to_string(),
        "POST".to_string(),
        "--data".to_string(),
        String::new(),
        format!(
            "{RATE_ENDPOINT}?id={}&rating=like",
            percent_encode(video_id)
        ),
    ]);
    Request {
        args,
        secrets: vec![bearer(access_token)],
    }
}

/// 自分のプレイリスト一覧の 1 ページ。
pub fn list_my_playlists_request(access_token: &str, page_token: Option<&str>) -> Request {
    let mut url =
        format!("{PLAYLISTS_ENDPOINT}?part=snippet&mine=true&maxResults={PLAYLIST_PAGE_SIZE}");
    if let Some(token) = page_token {
        url.push_str(&format!("&pageToken={}", percent_encode(token)));
    }
    let mut args = base_args();
    args.push(url);
    Request {
        args,
        secrets: vec![bearer(access_token)],
    }
}

/// 保存先のプレイリストを非公開で作る。
pub fn create_playlist_request(access_token: &str) -> Request {
    let body = serde_json::json!({
        "snippet": {"title": SAVE_PLAYLIST_TITLE},
        "status": {"privacyStatus": "private"}
    })
    .to_string();
    json_post(
        access_token,
        format!("{PLAYLISTS_ENDPOINT}?part=snippet,status"),
        body,
    )
}

/// プレイリストに動画が入っているかを見る。入っていれば 1 件返る。
pub fn find_playlist_item_request(
    access_token: &str,
    playlist_id: &str,
    video_id: &str,
) -> Request {
    let mut args = base_args();
    args.push(format!(
        "{PLAYLIST_ITEMS_ENDPOINT}?part=id&playlistId={}&videoId={}&maxResults=1",
        percent_encode(playlist_id),
        percent_encode(video_id)
    ));
    Request {
        args,
        secrets: vec![bearer(access_token)],
    }
}

/// プレイリストの末尾へ動画を足す。
pub fn insert_playlist_item_request(
    access_token: &str,
    playlist_id: &str,
    video_id: &str,
) -> Request {
    let body = serde_json::json!({
        "snippet": {
            "playlistId": playlist_id,
            "resourceId": {"kind": "youtube#video", "videoId": video_id}
        }
    })
    .to_string();
    json_post(
        access_token,
        format!("{PLAYLIST_ITEMS_ENDPOINT}?part=snippet"),
        body,
    )
}

/// JSON の本文を POST する。本文とトークンは設定ファイルとして stdin から渡す。
fn json_post(access_token: &str, url: String, body: String) -> Request {
    let mut args = base_args();
    args.extend([
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        "Content-Type: application/json".to_string(),
        url,
    ]);
    Request {
        args,
        secrets: vec![bearer(access_token), ("--data".to_string(), body)],
    }
}

/// 自分がいいねした動画一覧の 1 ページ。myRating は id と同時に指定できないので、
/// 動画 ID を渡して 1 件だけ確認する経路が無く、一覧を辿ることになる。
pub fn list_liked_videos_request(access_token: &str, page_token: Option<&str>) -> Request {
    let mut url = format!("{VIDEOS_ENDPOINT}?part=id&myRating=like&maxResults={LIKED_PAGE_SIZE}");
    if let Some(token) = page_token {
        url.push_str(&format!("&pageToken={}", percent_encode(token)));
    }
    let mut args = base_args();
    args.push(url);
    Request {
        args,
        secrets: vec![bearer(access_token)],
    }
}

/// 渡したチャンネルのうち自分が登録しているものを返させる。
pub fn list_subscriptions_request(access_token: &str, channel_ids: &[String]) -> Request {
    // 区切りのカンマを残すため ID ごとに符号化する。
    let ids: Vec<String> = channel_ids.iter().map(|id| percent_encode(id)).collect();
    let mut args = base_args();
    args.push(format!(
        "{SUBSCRIPTIONS_ENDPOINT}?part=snippet&mine=true&forChannelId={}",
        ids.join(",")
    ));
    Request {
        args,
        secrets: vec![bearer(access_token)],
    }
}

/// -w で足した末尾のステータス行を本文から切り離す。
pub fn split_status(stdout: &[u8]) -> Result<Response, String> {
    let text = String::from_utf8_lossy(stdout);
    let (body, status) = text
        .trim_end_matches('\n')
        .rsplit_once('\n')
        .unwrap_or(("", text.trim_end_matches('\n')));
    let status: u16 = status
        .trim()
        .parse()
        .map_err(|_| "HTTP 応答を読めませんでした".to_string())?;
    // 繋がらなかったときも書式だけは出て 000 になる。応答として扱うと失敗が成功に化ける。
    if status == 0 {
        return Err(TRANSPORT_FAILED.to_string());
    }
    Ok(Response {
        status,
        body: body.to_string(),
    })
}

/// curl の終了状態と出力から応答を組み立てる。異常終了は本文があっても失敗として扱う。
fn curl_result(ok: bool, stdout: &[u8], stderr: &[u8]) -> Result<Response, String> {
    if !ok {
        let text = String::from_utf8_lossy(stderr);
        let detail = text.trim();
        return Err(match detail.is_empty() {
            true => TRANSPORT_FAILED.to_string(),
            false => format!("{TRANSPORT_FAILED}: {detail}"),
        });
    }
    split_status(stdout)
}

// ---- JSON ----

pub fn tokens_of(response: &Response) -> Result<Tokens, String> {
    if response.status >= 400 {
        return Err(api_error(response));
    }
    let value: serde_json::Value =
        serde_json::from_str(&response.body).map_err(|e| format!("トークンを読めません: {e}"))?;
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "アクセストークンが返りませんでした".to_string())?;
    Ok(Tokens {
        access: access.to_string(),
        refresh: value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// 既に登録済み / いいね済みを表すエラーか。理由に duplicate を含むものを見る。
pub fn is_duplicate(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(errors) = value.pointer("/error/errors").and_then(|v| v.as_array()) else {
        return false;
    };
    errors.iter().any(|e| {
        e.get("reason")
            .and_then(|v| v.as_str())
            .is_some_and(|r| r.to_ascii_lowercase().contains("duplicate"))
    })
}

/// いいね一覧 1 ページの動画 ID と、続きがあればそのトークン。
fn liked_page_of(response: &Response) -> Result<(Vec<String>, Option<String>), String> {
    let value = api_json(response, "いいね一覧")?;
    let ids = value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let next = value
        .get("nextPageToken")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok((ids, next))
}

/// プレイリスト一覧の 1 ページから、保存先と同じ名前のものの ID と次のページを取り出す。
fn save_playlist_page_of(response: &Response) -> Result<(Option<String>, Option<String>), String> {
    let value = api_json(response, "プレイリスト一覧")?;
    let found = value
        .get("items")
        .and_then(|v| v.as_array())
        .and_then(|items| {
            items.iter().find(|item| {
                item.pointer("/snippet/title").and_then(|v| v.as_str()) == Some(SAVE_PLAYLIST_TITLE)
            })
        })
        .and_then(|item| item.get("id").and_then(|v| v.as_str()))
        .map(str::to_string);
    let next = value
        .get("nextPageToken")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok((found, next))
}

/// 作ったプレイリストの ID。
fn created_playlist_of(response: &Response) -> Result<String, String> {
    api_json(response, "作ったプレイリスト")?
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "作ったプレイリストの ID が返りませんでした".to_string())
}

/// 1 件でも返ってきたか。
fn has_items(response: &Response) -> Result<bool, String> {
    Ok(api_json(response, "プレイリストの中身")?
        .get("items")
        .and_then(|v| v.as_array())
        .is_some_and(|items| !items.is_empty()))
}

/// 登録済みとして返ってきたチャンネル ID。
fn subscribed_channels_of(response: &Response) -> Result<Vec<String>, String> {
    let value = api_json(response, "チャンネル登録")?;
    Ok(value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.pointer("/snippet/resourceId/channelId")
                        .and_then(|v| v.as_str())
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

fn api_json(response: &Response, what: &str) -> Result<serde_json::Value, String> {
    if response.status >= 400 {
        return Err(api_error(response));
    }
    serde_json::from_str(&response.body).map_err(|e| format!("{what}を読めません: {e}"))
}

pub fn api_error(response: &Response) -> String {
    let message = serde_json::from_str::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error_description"))
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str().map(str::to_string))
        });
    match message {
        Some(message) => format!(
            "YouTube から拒否されました ({}): {message}",
            response.status
        ),
        None => format!("YouTube から拒否されました ({})", response.status),
    }
}

// ---- 資格情報とトークンの保存 ----

fn config_file(xdg: Option<&OsStr>, home: Option<&OsStr>, file: &str) -> Option<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Some(Path::new(xdg).join(APP_DIR).join(file));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(".config").join(APP_DIR).join(file))
}

pub fn client_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    config_file(xdg, home, CLIENT_FILE)
}

pub fn token_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    config_file(xdg, home, TOKEN_FILE)
}

#[derive(Debug, Default, Deserialize)]
struct RawClient {
    client_id: Option<String>,
    client_secret: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawToken {
    refresh_token: Option<String>,
}

pub fn parse_client(text: &str) -> Result<Client, String> {
    // toml のエラーは壊れた行をそのまま引用する。client_secret の行だったときに
    // 秘密が画面へ出るので、理由は落として固定の文言にする。
    let raw: RawClient = toml::from_str(text).map_err(|_| BROKEN_CLIENT.to_string())?;
    match (raw.client_id, raw.client_secret) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
            Ok(Client { id, secret })
        }
        _ => Err("client_id と client_secret を書いてください".to_string()),
    }
}

pub fn read_client(path: &Path) -> Result<Client, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("{} を読めません: {e}", path.display()))?;
    parse_client(&text).map_err(|e| format!("{} を読めません: {e}", path.display()))
}

pub fn load_refresh_token(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str::<RawToken>(&text)
        .ok()?
        .refresh_token
        .filter(|t| !t.is_empty())
}

/// 途中で落ちても壊れたファイルを残さないよう一時ファイルへ書いてから置き換える。
/// 他の利用者から読めないよう 600 で作る。
pub fn save_refresh_token(path: &Path, refresh_token: &str) -> Result<(), String> {
    let raw = RawToken {
        refresh_token: Some(refresh_token.to_string()),
    };
    let text = toml::to_string(&raw).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, &text)?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 作ってから chmod すると、その間は umask 次第で他の利用者から読める。
/// 中身は書き込む前から秘密なので、モードを指定して作る。
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // 前回の書き損じが残っていると create_new が通らない。モードも当てにできない。
    let _ = fs::remove_file(path);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            // open のモードは umask で削られうるので、最後に 600 そのものにする。
            file.set_permissions(fs::Permissions::from_mode(0o600))
        });
    written.map_err(|e| {
        let _ = fs::remove_file(path);
        e.to_string()
    })
}

/// `https://www.youtube.com/watch?v=<id>` から動画 ID を取る。
pub fn video_id_from_url(url: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "v")
        .map(|(_, value)| percent_decode(value))
        .filter(|id| !id.is_empty())
}

// ---- フロー ----

/// 認証してから 1 回だけ API を呼ぶ。返すのは画面に出す文言。
pub async fn run<B: Backend>(
    backend: &B,
    paths: &Paths,
    action: &Action,
) -> Result<String, String> {
    let client = read_client(&paths.client)?;
    let (access, warning) = match load_refresh_token(&paths.token) {
        Some(refresh) => (refresh_access(backend, &client, &refresh).await?, None),
        None => authorize(backend, &client, &paths.token).await?,
    };
    let done = call_api(backend, &access, action).await?;
    Ok(match warning {
        Some(warning) => format!("{done} ({warning})"),
        None => done,
    })
}

/// ブラウザでの認可から access_token まで。refresh_token を保存できなかった事情も返す。
async fn authorize<B: Backend>(
    backend: &B,
    client: &Client,
    token_path: &Path,
) -> Result<(String, Option<String>), String> {
    let verifier = base64url(&backend.random(RANDOM_BYTES)?);
    let state = base64url(&backend.random(RANDOM_BYTES)?);
    let challenge = base64url(&backend.sha256(verifier.as_bytes().to_vec()).await?);
    let port = backend.bind()?;
    let redirect = redirect_uri(port);
    backend
        .open(auth_url(&client.id, &redirect, &challenge, &state))
        .await?;

    let callback = backend.wait().await?;
    if let Some(error) = callback.error {
        return Err(format!("認可されませんでした: {error}"));
    }
    if callback.state.as_deref() != Some(state.as_str()) {
        return Err(STATE_MISMATCH.to_string());
    }
    let code = callback.code.ok_or_else(|| NO_CODE.to_string())?;

    let body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("client_id", &client.id),
        ("client_secret", &client.secret),
        ("redirect_uri", &redirect),
        ("code_verifier", &verifier),
    ]);
    let tokens = tokens_of(&backend.curl(token_request(&body)).await?)?;
    // 保存に失敗しても認可自体は通っているので、操作は続けて文言に理由を添える。
    let warning = match &tokens.refresh {
        Some(refresh) => save_refresh_token(token_path, refresh).err(),
        None => None,
    };
    Ok((tokens.access, warning))
}

async fn refresh_access<B: Backend>(
    backend: &B,
    client: &Client,
    refresh_token: &str,
) -> Result<String, String> {
    let body = form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &client.id),
        ("client_secret", &client.secret),
    ]);
    Ok(tokens_of(&backend.curl(token_request(&body)).await?)?.access)
}

async fn call_api<B: Backend>(
    backend: &B,
    access_token: &str,
    action: &Action,
) -> Result<String, String> {
    let request = match action {
        Action::Subscribe(channel_id) => subscribe_request(access_token, channel_id),
        Action::Like(video_id) => rate_request(access_token, video_id),
        Action::Save(video_id) => return save_to_playlist(backend, access_token, video_id).await,
    };
    let response = backend.curl(request).await?;
    // 既に登録済み / いいね済みは操作としては望みどおりなので成功と同じ扱いにする。
    // 下限を置くのは、届かなかったときの 0 を成功として通さないため。
    if (100..400).contains(&response.status) || is_duplicate(&response.body) {
        return Ok(action.done_notice().to_string());
    }
    Err(api_error(&response))
}

/// tuitube のプレイリストへ足す。無ければ作り、既に入っていれば足さない。
async fn save_to_playlist<B: Backend>(
    backend: &B,
    access_token: &str,
    video_id: &str,
) -> Result<String, String> {
    let (playlist_id, created) = match find_save_playlist(backend, access_token).await? {
        Some(id) => (id, false),
        None => (
            created_playlist_of(&backend.curl(create_playlist_request(access_token)).await?)?,
            true,
        ),
    };
    // 作った直後のプレイリストは中身を聞くと見つからないと返る (実測)。空なので聞かずに足す。
    if !created {
        let found = backend
            .curl(find_playlist_item_request(
                access_token,
                &playlist_id,
                video_id,
            ))
            .await?;
        if has_items(&found)? {
            return Ok(ALREADY_SAVED_NOTICE.to_string());
        }
    }
    let inserted = backend
        .curl(insert_playlist_item_request(
            access_token,
            &playlist_id,
            video_id,
        ))
        .await?;
    // call_api と同じく、届かなかったときの 0 を成功として通さない。
    if !(100..400).contains(&inserted.status) {
        return Err(api_error(&inserted));
    }
    Ok(SAVED_NOTICE.to_string())
}

/// 自分のプレイリストから保存先を探す。見つからなければ None。
async fn find_save_playlist<B: Backend>(
    backend: &B,
    access_token: &str,
) -> Result<Option<String>, String> {
    let mut page: Option<String> = None;
    for _ in 0..PLAYLIST_PAGE_MAX {
        let response = backend
            .curl(list_my_playlists_request(access_token, page.as_deref()))
            .await?;
        let (found, next) = save_playlist_page_of(&response)?;
        if found.is_some() {
            return Ok(found);
        }
        match next {
            Some(token) => page = Some(token),
            None => break,
        }
    }
    Ok(None)
}

// ---- 状態確認 ----

/// 背景での状態確認に使うアクセストークン。保存済みの refresh_token だけを使う。
/// 認証がまだなら None を返す (印を出すためにブラウザを開かない)。
pub async fn access_token_for_refresh<B: Backend>(
    backend: &B,
    paths: &Paths,
) -> Result<Option<String>, String> {
    let Some(refresh) = load_refresh_token(&paths.token) else {
        return Ok(None);
    };
    let client = read_client(&paths.client)?;
    Ok(Some(refresh_access(backend, &client, &refresh).await?))
}

/// いいね済み動画 ID を全ページ集める。前のページのトークンが無いと次を呼べないので直列。
pub async fn refresh_liked_videos<B: Backend>(
    backend: &B,
    access_token: &str,
) -> Result<Vec<String>, String> {
    let mut liked = Vec::new();
    let mut page: Option<String> = None;
    for _ in 0..LIKED_PAGE_MAX {
        let request = list_liked_videos_request(access_token, page.as_deref());
        let (mut found, next) = liked_page_of(&backend.curl(request).await?)?;
        liked.append(&mut found);
        match next {
            Some(token) => page = Some(token),
            None => break,
        }
    }
    Ok(liked)
}

/// 渡したチャンネルのうち登録済みのものを返す。チャンクに分け、同時に走らせる数を
/// max_concurrent で抑える。1 つでも失敗したら全体を失敗にする。途中までを反映すると、
/// 返らなかった ID を未登録として確定させてしまうため。
pub async fn refresh_subscriptions<B: Backend + 'static>(
    backend: std::sync::Arc<B>,
    access_token: &str,
    channel_ids: &[String],
    max_concurrent: usize,
) -> Result<Vec<String>, String> {
    let chunks: Vec<&[String]> = channel_ids.chunks(CHANNEL_CHUNK).collect();
    let mut subscribed = Vec::new();
    for wave in chunks.chunks(max_concurrent.max(1)) {
        let mut running = Vec::with_capacity(wave.len());
        for chunk in wave {
            let backend = std::sync::Arc::clone(&backend);
            let request = list_subscriptions_request(access_token, chunk);
            running.push(tokio::spawn(async move {
                subscribed_channels_of(&backend.curl(request).await?)
            }));
        }
        for task in running {
            let mut found = task.await.map_err(|e| format!("確認できません: {e}"))??;
            subscribed.append(&mut found);
        }
    }
    Ok(subscribed)
}

// ---- 本番の Backend ----

pub struct RealBackend {
    listener: std::sync::Mutex<Option<tokio::net::TcpListener>>,
}

impl Default for RealBackend {
    fn default() -> Self {
        Self {
            listener: std::sync::Mutex::new(None),
        }
    }
}

impl Backend for RealBackend {
    fn random(&self, len: usize) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let mut buf = vec![0u8; len];
        fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .map_err(|e| format!("乱数を作れません: {e}"))?;
        Ok(buf)
    }

    fn sha256(&self, input: Vec<u8>) -> impl Future<Output = Result<Vec<u8>, String>> + Send {
        openssl_sha256(input)
    }

    async fn open(&self, url: String) -> Result<(), String> {
        let status = tokio::process::Command::new("open")
            .arg(url)
            .status()
            .await
            .map_err(|e| format!("ブラウザを開けません: {e}"))?;
        match status.success() {
            true => Ok(()),
            false => Err("ブラウザを開けません".to_string()),
        }
    }

    async fn curl(&self, request: Request) -> Result<Response, String> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new("curl")
            .args(&request.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("curl を実行できません: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("curl へ書き込めません")?;
        stdin
            .write_all(request.config().as_bytes())
            .await
            .map_err(|e| format!("curl へ書き込めません: {e}"))?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("curl を実行できません: {e}"))?;
        curl_result(output.status.success(), &output.stdout, &output.stderr)
    }

    fn bind(&self) -> Result<u16, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("待ち受けを開始できません: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("待ち受けポートが分かりません: {e}"))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("待ち受けを開始できません: {e}"))?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("待ち受けを開始できません: {e}"))?;
        *self.listener.lock().expect("待ち受け") = Some(listener);
        Ok(port)
    }

    fn wait(&self) -> impl Future<Output = Result<Callback, String>> + Send {
        // await を跨いでロックを持たないよう、先に取り出しておく。
        let listener = self.listener.lock().expect("待ち受け").take();
        async move {
            let listener = listener.ok_or_else(|| "待ち受けが開始されていません".to_string())?;
            // 認可をやめられてもポートを掴んだままにしない。
            let wait = std::time::Duration::from_secs(AUTH_WAIT_SECS);
            match tokio::time::timeout(wait, accept_callback(listener)).await {
                Ok(callback) => callback,
                Err(_) => Err(AUTH_TIMED_OUT.to_string()),
            }
        }
    }
}

async fn openssl_sha256(input: Vec<u8>) -> Result<Vec<u8>, String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("openssl を実行できません: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("openssl へ書き込めません")?;
    stdin
        .write_all(&input)
        .await
        .map_err(|e| format!("openssl へ書き込めません: {e}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("openssl を実行できません: {e}"))?;
    match output.status.success() {
        true => Ok(output.stdout),
        false => Err(format!(
            "ハッシュを計算できません: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

async fn accept_callback(listener: tokio::net::TcpListener) -> Result<Callback, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| format!("リダイレクトを受け取れません: {e}"))?;
    let mut line = String::new();
    {
        let mut reader = BufReader::new((&mut stream).take(REQUEST_LINE_MAX));
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("リダイレクトを読めません: {e}"))?;
    }
    let callback = parse_callback(&line);
    let _ = stream
        .write_all(&callback_response(callback.code.is_some()))
        .await;
    let _ = stream.shutdown().await;
    Ok(callback)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// 台本どおりに答える Backend。呼ばれた引数と URL を溜める。
    #[derive(Clone, Default)]
    pub struct FakeBackend {
        random: Arc<Mutex<VecDeque<Vec<u8>>>>,
        sha256: Arc<Mutex<Option<Vec<u8>>>>,
        curl: Arc<Mutex<VecDeque<Result<Response, String>>>>,
        callback: Arc<Mutex<Option<Result<Callback, String>>>>,
        port: Arc<Mutex<u16>>,
        opened: Arc<Mutex<Vec<String>>>,
        calls: Arc<Mutex<Vec<Request>>>,
        bound: Arc<Mutex<bool>>,
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self {
                port: Arc::new(Mutex::new(51234)),
                ..Self::default()
            }
        }

        pub fn with_random(self, bytes: &[&[u8]]) -> Self {
            *self.random.lock().expect("台本") = bytes.iter().map(|b| b.to_vec()).collect();
            self
        }

        pub fn with_sha256(self, digest: &[u8]) -> Self {
            *self.sha256.lock().expect("台本") = Some(digest.to_vec());
            self
        }

        pub fn with_callback(self, callback: Callback) -> Self {
            *self.callback.lock().expect("台本") = Some(Ok(callback));
            self
        }

        pub fn with_responses(self, responses: Vec<Result<Response, String>>) -> Self {
            *self.curl.lock().expect("台本") = responses.into();
            self
        }

        pub fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("溜め込み先").clone()
        }

        pub fn calls(&self) -> Vec<Request> {
            self.calls.lock().expect("溜め込み先").clone()
        }

        pub fn bound(&self) -> bool {
            *self.bound.lock().expect("溜め込み先")
        }
    }

    impl Backend for FakeBackend {
        fn random(&self, len: usize) -> Result<Vec<u8>, String> {
            match self.random.lock().expect("台本").pop_front() {
                Some(bytes) => Ok(bytes),
                None => Ok(vec![0xab; len]),
            }
        }

        fn sha256(&self, input: Vec<u8>) -> impl Future<Output = Result<Vec<u8>, String>> + Send {
            let scripted = self.sha256.lock().expect("台本").clone();
            async move { Ok(scripted.unwrap_or(input)) }
        }

        fn open(&self, url: String) -> impl Future<Output = Result<(), String>> + Send {
            self.opened.lock().expect("溜め込み先").push(url);
            async move { Ok(()) }
        }

        fn curl(&self, request: Request) -> impl Future<Output = Result<Response, String>> + Send {
            self.calls.lock().expect("溜め込み先").push(request);
            let step = self.curl.lock().expect("台本").pop_front();
            async move { step.unwrap_or_else(|| panic!("台本に無い呼び出し")) }
        }

        fn bind(&self) -> Result<u16, String> {
            *self.bound.lock().expect("溜め込み先") = true;
            Ok(*self.port.lock().expect("台本"))
        }

        fn wait(&self) -> impl Future<Output = Result<Callback, String>> + Send {
            let scripted = self.callback.lock().expect("台本").take();
            async move { scripted.unwrap_or_else(|| Err("台本に無い待ち受け".to_string())) }
        }
    }

    pub fn ok(body: &str) -> Result<Response, String> {
        Ok(Response {
            status: 200,
            body: body.to_string(),
        })
    }

    pub fn failed(status: u16, body: &str) -> Result<Response, String> {
        Ok(Response {
            status,
            body: body.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const CLIENT_TOML: &str =
        "client_id = \"dummy-id.apps.googleusercontent.com\"\nclient_secret = \"dummy-secret\"\n";
    const ACCESS_JSON: &str = r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3599}"#;
    const REFRESHED_JSON: &str = r#"{"access_token":"at-2","expires_in":3599}"#;
    const DUPLICATE_JSON: &str = r#"{"error":{"code":400,"message":"duplicate","errors":[{"reason":"subscriptionDuplicate"}]}}"#;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tuitube-oauth-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// client だけ置いたディレクトリ。token はまだ無い。
    fn paths(name: &str) -> Paths {
        let dir = temp_dir(name);
        let client = dir.join(CLIENT_FILE);
        fs::write(&client, CLIENT_TOML).expect("書ける");
        Paths {
            client,
            token: dir.join(TOKEN_FILE),
        }
    }

    fn callback(code: &str, state: &str) -> Callback {
        Callback {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
            error: None,
        }
    }

    /// 台本の乱数から実際に組み立てられる state。
    fn state_of(bytes: &[u8]) -> String {
        base64url(bytes)
    }

    /// stdin へ渡す値を 1 つ取り出す。
    fn secret_of(request: &Request, option: &str) -> String {
        request
            .secrets
            .iter()
            .find(|(name, _)| name == option)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("{option} が無い: {request:?}"))
    }

    fn body_of(calls: &[Request], at: usize) -> String {
        secret_of(&calls[at], "--data")
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn url_of(calls: &[Request], at: usize) -> String {
        calls[at].url().to_string()
    }

    /// argv に秘密が混じっていないか。ps から読めるのはここだけ。
    fn assert_argv_is_clean(request: &Request, secrets: &[&str]) {
        let argv = request.args.join(" ");
        for secret in secrets {
            assert!(
                !argv.contains(secret),
                "{secret} が argv に出ている: {argv}"
            );
        }
    }

    // ---- 符号化 ----

    #[test]
    fn base64url_swaps_the_unsafe_characters_and_drops_padding() {
        // 0xfb 0xff は標準 base64 で + と / を出すバイト列。
        assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert!(!base64url(&[0xff; 32]).contains(['+', '/', '=']));
    }

    #[test]
    fn base64url_is_the_length_of_a_padless_encoding() {
        assert_eq!(base64url(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn percent_encode_keeps_unreserved_characters() {
        assert_eq!(percent_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(
            percent_encode("https://x/y?z=1&w"),
            "https%3A%2F%2Fx%2Fy%3Fz%3D1%26w"
        );
    }

    #[test]
    fn percent_decode_reads_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%E3%81%82"), "あ");
        // 壊れた並びはそのまま残す。
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn percent_encode_round_trips_through_decode() {
        for text in ["plain", "a b&c=d", "日本語", "https://example.com/?a=1"] {
            assert_eq!(percent_decode(&percent_encode(text)), text, "{text}");
        }
    }

    #[test]
    fn form_encode_joins_escaped_pairs() {
        assert_eq!(
            form_encode(&[("grant_type", "authorization_code"), ("code", "a/b+c")]),
            "grant_type=authorization_code&code=a%2Fb%2Bc"
        );
    }

    // ---- 認可 URL とコールバック ----

    #[test]
    fn redirect_uri_points_at_the_bound_loopback_port() {
        assert_eq!(redirect_uri(51234), "http://127.0.0.1:51234/callback");
    }

    #[test]
    fn auth_url_carries_the_pkce_parameters() {
        let url = auth_url("cid", &redirect_uri(4242), "chal", "st");
        assert!(url.starts_with(&format!("{AUTH_ENDPOINT}?")), "{url}");
        for expected in [
            "client_id=cid",
            "response_type=code",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "state=st",
            "access_type=offline",
            "prompt=consent",
        ] {
            assert!(url.contains(expected), "{expected} が無い: {url}");
        }
    }

    #[test]
    fn auth_url_escapes_the_scope_and_redirect_uri() {
        let url = auth_url("cid", &redirect_uri(4242), "chal", "st");
        assert!(url.contains(&percent_encode(SCOPE)), "{url}");
        assert!(url.contains(&percent_encode(&redirect_uri(4242))), "{url}");
        // 生のままでは入らない (クエリが途中で切れる)。
        assert!(!url.contains("://www.googleapis.com/auth"), "{url}");
    }

    #[test]
    fn parse_callback_reads_the_code_and_state() {
        let got = parse_callback("GET /callback?code=abc&state=xyz HTTP/1.1");
        assert_eq!(got, callback("abc", "xyz"));
    }

    #[test]
    fn parse_callback_reads_a_denial() {
        let got = parse_callback("GET /callback?error=access_denied&state=xyz HTTP/1.1");
        assert_eq!(got.error.as_deref(), Some("access_denied"));
        assert_eq!(got.code, None);
    }

    #[test]
    fn parse_callback_decodes_escaped_values() {
        let got = parse_callback("GET /callback?code=a%2Fb&state=s%20t HTTP/1.1");
        assert_eq!(got.code.as_deref(), Some("a/b"));
        assert_eq!(got.state.as_deref(), Some("s t"));
    }

    #[test]
    fn parse_callback_is_empty_without_a_query_or_a_target() {
        assert_eq!(
            parse_callback("GET /callback HTTP/1.1"),
            Callback::default()
        );
        assert_eq!(parse_callback("GET"), Callback::default());
        assert_eq!(parse_callback(""), Callback::default());
    }

    #[test]
    fn parse_callback_ignores_parameters_it_does_not_know() {
        let got = parse_callback("GET /callback?scope=x&code=c&state=s&prompt=none HTTP/1.1");
        assert_eq!(got, callback("c", "s"));
    }

    #[test]
    fn callback_response_is_a_complete_http_reply() {
        let text = String::from_utf8(callback_response(true)).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        let (head, body) = text.split_once("\r\n\r\n").expect("空行で区切る");
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("長さ")
            .trim()
            .parse()
            .expect("数値");
        assert_eq!(length, body.len());
        assert!(body.contains("閉じて構いません"), "{body}");
    }

    #[test]
    fn callback_response_says_it_failed_when_no_code_arrived() {
        let text = String::from_utf8(callback_response(false)).expect("utf-8");
        assert!(text.contains("認可できませんでした"), "{text}");
    }

    // ---- curl 引数と応答 ----

    #[test]
    fn every_request_asks_for_the_status_and_never_uses_fail() {
        for request in [
            token_request("grant_type=refresh_token"),
            subscribe_request("at", "UC1"),
            rate_request("at", "vid"),
            list_liked_videos_request("at", None),
            list_subscriptions_request("at", &ids(&["UC1"])),
        ] {
            let args = &request.args;
            let at = args.iter().position(|a| a == "-w").expect("書式");
            assert_eq!(args[at + 1], "\n%{http_code}");
            assert!(!args.contains(&"--fail".to_string()), "{args:?}");
            let timeout = args.iter().position(|a| a == "--max-time").expect("上限");
            assert_eq!(args[timeout + 1], "30");
            let config = args.iter().position(|a| a == "--config").expect("設定");
            assert_eq!(args[config + 1], "-", "秘密は stdin から読ませる");
        }
    }

    #[test]
    fn every_request_keeps_its_secrets_out_of_the_argv() {
        // ps -Ao args は同じ利用者の他プロセスからも読める。
        let token = token_request("client_secret=s3cret&refresh_token=rt-1");
        assert_argv_is_clean(&token, &["s3cret", "rt-1"]);
        assert!(token.config().contains("s3cret"), "{}", token.config());

        for request in [
            subscribe_request("at-1", "UC123"),
            rate_request("at-1", "vid1"),
            list_liked_videos_request("at-1", Some("page-1")),
            list_subscriptions_request("at-1", &ids(&["UC123"])),
        ] {
            assert_argv_is_clean(&request, &["at-1", "Authorization"]);
            assert_eq!(
                secret_of(&request, "--header"),
                "Authorization: Bearer at-1"
            );
        }
    }

    #[test]
    fn the_config_quotes_every_value_and_escapes_what_would_break_it() {
        let request = Request {
            args: Vec::new(),
            secrets: vec![
                ("--data".to_string(), r#"{"a":"b\c"}"#.to_string()),
                ("--header".to_string(), "X: 1\n--evil 2".to_string()),
            ],
        };
        let config = request.config();
        assert_eq!(
            config,
            "--data \"{\\\"a\\\":\\\"b\\\\c\\\"}\"\n--header \"X: 1\\n--evil 2\"\n"
        );
        // 行が割れないので、値の中の改行が別のオプションとして読まれることはない。
        assert_eq!(config.lines().count(), 2);
    }

    #[test]
    fn token_request_posts_the_form_to_the_token_endpoint() {
        let request = token_request("grant_type=authorization_code&code=abc");
        assert_eq!(request.url(), TOKEN_ENDPOINT);
        assert!(
            request
                .args
                .contains(&"Content-Type: application/x-www-form-urlencoded".to_string())
        );
        assert_eq!(
            body_of(&[request], 0),
            "grant_type=authorization_code&code=abc"
        );
    }

    #[test]
    fn subscribe_request_carries_the_bearer_token_and_the_channel() {
        let request = subscribe_request("at-1", "UC123");
        assert_eq!(
            request.url(),
            format!("{SUBSCRIPTIONS_ENDPOINT}?part=snippet")
        );
        assert_eq!(
            secret_of(&request, "--header"),
            "Authorization: Bearer at-1"
        );
        let body = body_of(&[request], 0);
        let value: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(
            value
                .pointer("/snippet/resourceId/channelId")
                .and_then(|v| v.as_str()),
            Some("UC123")
        );
        assert_eq!(
            value
                .pointer("/snippet/resourceId/kind")
                .and_then(|v| v.as_str()),
            Some("youtube#channel")
        );
    }

    #[test]
    fn subscribe_request_escapes_a_channel_id_that_would_break_the_json() {
        // channel_id は yt-dlp の出力由来で、こちらが作った値ではない。
        let id = r#"UC"x\y"#;
        let body = body_of(&[subscribe_request("at-1", id)], 0);
        let value: serde_json::Value = serde_json::from_str(&body).expect("壊れない JSON");
        assert_eq!(
            value
                .pointer("/snippet/resourceId/channelId")
                .and_then(|v| v.as_str()),
            Some(id)
        );
    }

    #[test]
    fn rate_request_asks_to_like_the_video() {
        let request = rate_request("at-1", "vid 1");
        assert_eq!(
            request.url(),
            format!("{RATE_ENDPOINT}?id=vid%201&rating=like")
        );
        assert_eq!(
            secret_of(&request, "--header"),
            "Authorization: Bearer at-1"
        );
    }

    #[test]
    fn list_liked_videos_request_asks_for_a_page_of_the_liked_list() {
        let request = list_liked_videos_request("at-1", None);
        assert_eq!(
            request.url(),
            format!("{VIDEOS_ENDPOINT}?part=id&myRating=like&maxResults=50")
        );
        assert_eq!(
            secret_of(&request, "--header"),
            "Authorization: Bearer at-1"
        );
    }

    #[test]
    fn list_liked_videos_request_carries_the_page_token() {
        let request = list_liked_videos_request("at-1", Some("CAUQAA/=="));
        assert_eq!(
            request.url(),
            format!(
                "{VIDEOS_ENDPOINT}?part=id&myRating=like&maxResults=50&pageToken=CAUQAA%2F%3D%3D"
            )
        );
    }

    #[test]
    fn list_subscriptions_request_asks_about_the_given_channels() {
        let request = list_subscriptions_request("at-1", &ids(&["UC1", "UC2"]));
        assert_eq!(
            request.url(),
            format!("{SUBSCRIPTIONS_ENDPOINT}?part=snippet&mine=true&forChannelId=UC1,UC2")
        );
        assert_eq!(
            secret_of(&request, "--header"),
            "Authorization: Bearer at-1"
        );
    }

    #[test]
    fn list_subscriptions_request_keeps_the_comma_that_separates_the_ids() {
        // ID ごとに符号化する。丸ごと符号化すると区切りのカンマまで %2C になる。
        let request = list_subscriptions_request("at-1", &ids(&["UC a", "UC/b"]));
        assert_eq!(
            request.url(),
            format!("{SUBSCRIPTIONS_ENDPOINT}?part=snippet&mine=true&forChannelId=UC%20a,UC%2Fb")
        );
    }

    #[test]
    fn split_status_separates_the_body_from_the_trailing_code() {
        let got = split_status(b"{\"a\":1}\n200").expect("読める");
        assert_eq!(
            got,
            Response {
                status: 200,
                body: "{\"a\":1}".to_string()
            }
        );
    }

    #[test]
    fn split_status_handles_an_empty_body() {
        let got = split_status(b"\n204").expect("読める");
        assert_eq!(got.status, 204);
        assert!(got.body.is_empty());
    }

    #[test]
    fn split_status_keeps_a_multi_line_body() {
        let got = split_status(b"line1\nline2\n404").expect("読める");
        assert_eq!(got.status, 404);
        assert_eq!(got.body, "line1\nline2");
    }

    #[test]
    fn split_status_rejects_output_without_a_status() {
        assert!(split_status(b"").is_err());
        assert!(split_status(b"{\"a\":1}").is_err());
    }

    #[test]
    fn split_status_refuses_the_zero_curl_prints_when_it_could_not_connect() {
        // 接続できなくても -w の書式は出る。応答 0 を 200 未満として通すと成功に化ける。
        let error = split_status(b"\n000").expect_err("失敗");
        assert_eq!(error, TRANSPORT_FAILED);
    }

    #[test]
    fn a_curl_that_exits_badly_is_a_failure_even_with_output() {
        // 接続拒否は 7、--max-time 超過は 28。どちらも stdout に "\n000" が出る。
        let error =
            curl_result(false, b"\n000", b"curl: (28) Operation timed out").expect_err("失敗");
        assert!(error.starts_with(TRANSPORT_FAILED), "{error}");
        assert!(error.contains("timed out"), "{error}");

        let bare = curl_result(false, b"\n000", b"").expect_err("失敗");
        assert_eq!(bare, TRANSPORT_FAILED);
    }

    #[test]
    fn a_curl_that_exits_cleanly_is_read_as_a_response() {
        let got = curl_result(true, b"{\"a\":1}\n404", b"").expect("読める");
        assert_eq!(got.status, 404);
        assert_eq!(got.body, "{\"a\":1}");
    }

    // ---- JSON ----

    #[test]
    fn tokens_of_reads_the_access_and_refresh_tokens() {
        let got = tokens_of(&Response {
            status: 200,
            body: ACCESS_JSON.to_string(),
        })
        .expect("読める");
        assert_eq!(got.access, "at-1");
        assert_eq!(got.refresh.as_deref(), Some("rt-1"));
    }

    #[test]
    fn tokens_of_allows_a_response_without_a_refresh_token() {
        let got = tokens_of(&Response {
            status: 200,
            body: REFRESHED_JSON.to_string(),
        })
        .expect("読める");
        assert_eq!(got.access, "at-2");
        assert_eq!(got.refresh, None);
    }

    #[test]
    fn tokens_of_reports_an_error_status_with_its_description() {
        let error = tokens_of(&Response {
            status: 400,
            body: r#"{"error":"invalid_grant","error_description":"Bad Request"}"#.to_string(),
        })
        .expect_err("失敗");
        assert!(error.contains("400"), "{error}");
        assert!(error.contains("Bad Request"), "{error}");
    }

    #[test]
    fn tokens_of_rejects_a_body_without_an_access_token() {
        assert!(
            tokens_of(&Response {
                status: 200,
                body: "{}".to_string()
            })
            .is_err()
        );
        assert!(
            tokens_of(&Response {
                status: 200,
                body: "not json".to_string()
            })
            .is_err()
        );
    }

    #[test]
    fn is_duplicate_matches_the_duplicate_reasons() {
        assert!(is_duplicate(DUPLICATE_JSON));
        assert!(is_duplicate(
            r#"{"error":{"errors":[{"reason":"videoRatingDuplicate"}]}}"#
        ));
    }

    #[test]
    fn is_duplicate_is_false_for_other_failures() {
        assert!(!is_duplicate(
            r#"{"error":{"errors":[{"reason":"quotaExceeded"}]}}"#
        ));
        assert!(!is_duplicate(r#"{"error":{"message":"nope"}}"#));
        assert!(!is_duplicate("not json"));
        assert!(!is_duplicate(""));
    }

    #[test]
    fn api_error_uses_the_message_and_falls_back_to_the_status() {
        let with_message = api_error(&Response {
            status: 403,
            body: r#"{"error":{"message":"quota"}}"#.to_string(),
        });
        assert!(
            with_message.contains("403") && with_message.contains("quota"),
            "{with_message}"
        );

        let bare = api_error(&Response {
            status: 500,
            body: String::new(),
        });
        assert!(bare.contains("500"), "{bare}");
    }

    // ---- 置き場と保存 ----

    #[test]
    fn the_config_paths_prefer_xdg_over_home() {
        let xdg = OsStr::new("/x");
        let home = OsStr::new("/h");
        assert_eq!(
            client_path(Some(xdg), Some(home)),
            Some(PathBuf::from("/x/tuitube/oauth_client.toml"))
        );
        assert_eq!(
            token_path(None, Some(home)),
            Some(PathBuf::from("/h/.config/tuitube/oauth_token.toml"))
        );
        // 空の XDG は無いものとして HOME へ落ちる。
        assert_eq!(
            token_path(Some(OsStr::new("")), Some(home)),
            token_path(None, Some(home))
        );
        assert_eq!(token_path(None, None), None);
    }

    #[test]
    fn parse_client_needs_both_keys() {
        let client = parse_client(CLIENT_TOML).expect("読める");
        assert_eq!(client.id, "dummy-id.apps.googleusercontent.com");
        assert_eq!(client.secret, "dummy-secret");

        assert!(parse_client("client_id = \"only\"\n").is_err());
        assert!(parse_client("client_id = \"\"\nclient_secret = \"s\"\n").is_err());
        assert!(parse_client("not toml =").is_err());
    }

    #[test]
    fn a_broken_client_file_never_quotes_what_is_in_it() {
        // toml のエラーは壊れた行をそのまま引用する。その文字列は画面に出る。
        let broken = "client_id = \"id\"\nclient_secret = \"GOCSPX-SUPERSECRET123\n";
        let error = parse_client(broken).expect_err("読めない");
        assert_eq!(error, BROKEN_CLIENT);

        let dir = temp_dir("broken-client");
        let path = dir.join(CLIENT_FILE);
        fs::write(&path, broken).expect("書ける");
        let error = read_client(&path).expect_err("読めない");
        assert!(!error.contains("GOCSPX"), "{error}");
        assert!(error.contains(CLIENT_FILE), "どのファイルかは出す: {error}");
    }

    #[test]
    fn the_refresh_token_round_trips_through_a_private_file() {
        let dir = temp_dir("token-roundtrip");
        let path = dir.join(TOKEN_FILE);
        save_refresh_token(&path, "rt-1").expect("書ける");

        assert_eq!(load_refresh_token(&path).as_deref(), Some("rt-1"));
        let mode = fs::metadata(&path).expect("ある").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "他の利用者から読めてはいけない");
        assert!(
            !dir.join("oauth_token.toml.tmp").exists(),
            "一時ファイルを残さない"
        );
    }

    #[test]
    fn a_private_file_is_never_readable_by_others_even_for_a_moment() {
        // 作ってから chmod だと、その間は umask (既定 022) のぶん他人から読める。
        // 中を覗く窓は作れないので、作成時のモード指定と、緩いファイルを引き継がないことを見る。
        let dir = temp_dir("private-create");
        let path = dir.join("fresh.toml");
        write_private(&path, "refresh_token = \"rt-1\"\n").expect("書ける");
        assert_eq!(
            fs::metadata(&path).expect("ある").permissions().mode() & 0o777,
            0o600
        );

        // 前回の書き損じが 644 で残っていても、その穴を引き継がない。
        let leftover = dir.join("leftover.toml");
        fs::write(&leftover, "old").expect("書ける");
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).expect("当てられる");
        write_private(&leftover, "refresh_token = \"rt-2\"\n").expect("書ける");
        assert_eq!(
            fs::metadata(&leftover).expect("ある").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_refresh_token(&leftover).as_deref(), Some("rt-2"));
    }

    #[test]
    fn saving_the_refresh_token_creates_the_directory() {
        let dir = temp_dir("token-mkdir");
        let path = dir.join("nested").join(TOKEN_FILE);
        save_refresh_token(&path, "rt-1").expect("書ける");
        assert_eq!(load_refresh_token(&path).as_deref(), Some("rt-1"));
    }

    #[test]
    fn saving_the_refresh_token_replaces_an_older_one() {
        let dir = temp_dir("token-replace");
        let path = dir.join(TOKEN_FILE);
        save_refresh_token(&path, "old").expect("書ける");
        save_refresh_token(&path, "new").expect("書ける");
        assert_eq!(load_refresh_token(&path).as_deref(), Some("new"));
    }

    #[test]
    fn a_token_that_is_missing_or_empty_reads_as_none() {
        let dir = temp_dir("token-missing");
        assert_eq!(load_refresh_token(&dir.join(TOKEN_FILE)), None);

        let empty = dir.join("empty.toml");
        fs::write(&empty, "refresh_token = \"\"\n").expect("書ける");
        assert_eq!(load_refresh_token(&empty), None);

        let broken = dir.join("broken.toml");
        fs::write(&broken, "not toml =").expect("書ける");
        assert_eq!(load_refresh_token(&broken), None);
    }

    #[test]
    fn video_id_comes_out_of_the_watch_url() {
        assert_eq!(
            video_id_from_url("https://www.youtube.com/watch?v=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            video_id_from_url("https://www.youtube.com/watch?t=1&v=abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(video_id_from_url("https://www.youtube.com/watch"), None);
        assert_eq!(video_id_from_url("https://www.youtube.com/watch?v="), None);
        assert_eq!(video_id_from_url(""), None);
    }

    // ---- フロー ----

    #[tokio::test]
    async fn the_first_run_opens_the_browser_and_stores_the_refresh_token() {
        let paths = paths("first-run");
        let state = state_of(b"state-bytes");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_sha256(b"digest")
            .with_callback(callback("code-1", &state))
            .with_responses(vec![ok(ACCESS_JSON), failed(204, "")]);

        let notice = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect("通る");

        assert_eq!(notice, SUBSCRIBED_NOTICE);
        assert!(backend.bound(), "ブラウザを開く前に待ち受ける");
        let opened = backend.opened();
        assert_eq!(opened.len(), 1);
        assert!(
            opened[0].contains(&format!("state={}", percent_encode(&state))),
            "{}",
            opened[0]
        );
        assert!(
            opened[0].contains(&format!(
                "code_challenge={}",
                percent_encode(&base64url(b"digest"))
            )),
            "{}",
            opened[0]
        );
        assert!(
            opened[0].contains(&percent_encode(&redirect_uri(51234))),
            "{}",
            opened[0]
        );

        let calls = backend.calls();
        assert_eq!(calls.len(), 2, "トークン交換と API の 2 回");
        let exchange = body_of(&calls, 0);
        assert!(
            exchange.contains("grant_type=authorization_code"),
            "{exchange}"
        );
        assert!(exchange.contains("code=code-1"), "{exchange}");
        assert!(
            exchange.contains(&format!("code_verifier={}", base64url(b"verifier-bytes"))),
            "{exchange}"
        );
        assert_eq!(
            url_of(&calls, 1),
            format!("{SUBSCRIPTIONS_ENDPOINT}?part=snippet")
        );
        assert_eq!(
            secret_of(&calls[1], "--header"),
            "Authorization: Bearer at-1"
        );
        assert_argv_is_clean(&calls[0], &["dummy-secret", "code-1"]);
        assert_argv_is_clean(&calls[1], &["at-1"]);

        assert_eq!(load_refresh_token(&paths.token).as_deref(), Some("rt-1"));
    }

    #[tokio::test]
    async fn a_stored_token_is_refreshed_without_opening_a_browser() {
        let paths = paths("stored-token");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![ok(REFRESHED_JSON), failed(204, "")]);

        let notice = run(&backend, &paths, &Action::Like("vid1".to_string()))
            .await
            .expect("通る");

        assert_eq!(notice, LIKED_NOTICE);
        assert!(backend.opened().is_empty(), "ブラウザは開かない");
        assert!(!backend.bound(), "待ち受けもしない");

        let calls = backend.calls();
        assert_eq!(calls.len(), 2);
        let refresh = body_of(&calls, 0);
        assert!(refresh.contains("grant_type=refresh_token"), "{refresh}");
        assert!(refresh.contains("refresh_token=rt-1"), "{refresh}");
        assert_eq!(
            url_of(&calls, 1),
            format!("{RATE_ENDPOINT}?id=vid1&rating=like")
        );
        assert_eq!(
            secret_of(&calls[1], "--header"),
            "Authorization: Bearer at-2"
        );
        assert_argv_is_clean(&calls[0], &["dummy-secret", "rt-1"]);
        assert_argv_is_clean(&calls[1], &["at-2"]);
    }

    #[tokio::test]
    async fn a_callback_state_that_does_not_match_stops_before_the_exchange() {
        let paths = paths("state-mismatch");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_callback(callback("code-1", "someone-elses-state"));

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");

        assert_eq!(error, STATE_MISMATCH);
        assert!(backend.calls().is_empty(), "認可コードを送らない");
        assert!(load_refresh_token(&paths.token).is_none());
    }

    #[tokio::test]
    async fn a_callback_without_a_code_is_refused() {
        let paths = paths("no-code");
        let state = state_of(b"state-bytes");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_callback(Callback {
                code: None,
                state: Some(state),
                error: None,
            });

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");
        assert_eq!(error, NO_CODE);
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_denied_authorization_is_reported() {
        let paths = paths("denied");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_callback(Callback {
                code: None,
                state: None,
                error: Some("access_denied".to_string()),
            });

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");
        assert!(error.contains("access_denied"), "{error}");
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_failed_code_exchange_is_reported_and_stores_nothing() {
        let paths = paths("exchange-failed");
        let state = state_of(b"state-bytes");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_callback(callback("code-1", &state))
            .with_responses(vec![failed(
                400,
                r#"{"error":"invalid_grant","error_description":"Bad Request"}"#,
            )]);

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");

        assert!(error.contains("400"), "{error}");
        assert_eq!(backend.calls().len(), 1, "API は呼ばない");
        assert!(load_refresh_token(&paths.token).is_none());
    }

    #[tokio::test]
    async fn a_failed_refresh_is_reported_without_calling_the_api() {
        let paths = paths("refresh-failed");
        save_refresh_token(&paths.token, "rt-old").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![failed(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )]);

        let error = run(&backend, &paths, &Action::Like("vid1".to_string()))
            .await
            .expect_err("止まる");

        assert!(
            error.contains("expired") || error.contains("400"),
            "{error}"
        );
        assert_eq!(backend.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_duplicate_subscription_counts_as_done() {
        let paths = paths("duplicate-subscribe");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new()
            .with_responses(vec![ok(REFRESHED_JSON), failed(400, DUPLICATE_JSON)]);

        let notice = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect("成功扱い");
        assert_eq!(notice, SUBSCRIBED_NOTICE);
    }

    #[tokio::test]
    async fn a_duplicate_like_counts_as_done() {
        let paths = paths("duplicate-like");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            failed(
                400,
                r#"{"error":{"errors":[{"reason":"videoRatingDuplicate"}]}}"#,
            ),
        ]);

        let notice = run(&backend, &paths, &Action::Like("vid1".to_string()))
            .await
            .expect("成功扱い");
        assert_eq!(notice, LIKED_NOTICE);
    }

    #[tokio::test]
    async fn an_api_failure_that_is_not_a_duplicate_is_reported() {
        let paths = paths("api-failed");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            failed(
                403,
                r#"{"error":{"message":"quota","errors":[{"reason":"quotaExceeded"}]}}"#,
            ),
        ]);

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");
        assert!(error.contains("403") && error.contains("quota"), "{error}");
    }

    #[tokio::test]
    async fn a_transport_failure_is_reported() {
        let paths = paths("transport-failed");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![Err(TRANSPORT_FAILED.to_string())]);

        let error = run(&backend, &paths, &Action::Like("vid1".to_string()))
            .await
            .expect_err("止まる");
        assert_eq!(error, TRANSPORT_FAILED);
    }

    #[tokio::test]
    async fn a_request_that_never_reached_youtube_is_not_reported_as_done() {
        // curl が繋げなかったときの 0。ここを通すと 1 バイトも送らずに「登録しました」が出る。
        let paths = paths("unreachable");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![ok(REFRESHED_JSON), failed(0, "")]);

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");
        assert_ne!(error, SUBSCRIBED_NOTICE);
    }

    #[tokio::test]
    async fn a_missing_client_file_stops_before_touching_anything() {
        let dir = temp_dir("no-client");
        let paths = Paths {
            client: dir.join(CLIENT_FILE),
            token: dir.join(TOKEN_FILE),
        };
        let backend = FakeBackend::new();

        let error = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect_err("止まる");

        assert!(error.contains(CLIENT_FILE), "{error}");
        assert!(backend.opened().is_empty());
        assert!(backend.calls().is_empty());
        assert!(!backend.bound());
    }

    #[tokio::test]
    async fn the_action_still_goes_through_when_the_token_cannot_be_saved() {
        let paths = paths("unsavable-token");
        // 書き込み先をディレクトリにして保存を失敗させる。
        fs::create_dir_all(&paths.token).expect("作れる");
        let state = state_of(b"state-bytes");
        let backend = FakeBackend::new()
            .with_random(&[b"verifier-bytes", b"state-bytes"])
            .with_callback(callback("code-1", &state))
            .with_responses(vec![ok(ACCESS_JSON), failed(204, "")]);

        let notice = run(&backend, &paths, &Action::Subscribe("UC1".to_string()))
            .await
            .expect("操作自体は通る");

        assert!(notice.starts_with(SUBSCRIBED_NOTICE), "{notice}");
        assert!(
            notice.len() > SUBSCRIBED_NOTICE.len(),
            "理由を添える: {notice}"
        );
        assert_eq!(backend.calls().len(), 2);
    }

    // ---- 状態確認 ----

    /// いいね一覧の 1 ページぶんの応答。
    fn liked_page(video_ids: &[&str], next: Option<&str>) -> Result<Response, String> {
        let items: Vec<serde_json::Value> = video_ids
            .iter()
            .map(|id| serde_json::json!({"kind": "youtube#video", "id": id}))
            .collect();
        let mut body = serde_json::json!({"items": items});
        if let Some(next) = next {
            body["nextPageToken"] = serde_json::json!(next);
        }
        ok(&body.to_string())
    }

    /// 登録済みとして返すチャンネルの応答。
    fn subscription_page(channel_ids: &[&str]) -> Result<Response, String> {
        let items: Vec<serde_json::Value> = channel_ids
            .iter()
            .map(|id| {
                serde_json::json!({"snippet": {"resourceId": {"kind": "youtube#channel", "channelId": id}}})
            })
            .collect();
        ok(&serde_json::json!({"items": items}).to_string())
    }

    /// URL の forChannelId に並んでいる ID。
    fn asked_channels(request: &Request) -> Vec<String> {
        let query = request
            .url()
            .split_once("forChannelId=")
            .expect("問い合わせ")
            .1;
        query.split(',').map(percent_decode).collect()
    }

    #[tokio::test]
    async fn the_liked_list_is_walked_page_by_page() {
        let backend = FakeBackend::new().with_responses(vec![
            liked_page(&["v1", "v2"], Some("page-2")),
            liked_page(&["v3"], None),
        ]);

        let liked = refresh_liked_videos(&backend, "at-1")
            .await
            .expect("取れる");

        assert_eq!(liked, ids(&["v1", "v2", "v3"]));
        let calls = backend.calls();
        assert_eq!(calls.len(), 2);
        assert!(!url_of(&calls, 0).contains("pageToken"), "初回は付けない");
        assert!(
            url_of(&calls, 1).ends_with("&pageToken=page-2"),
            "{}",
            url_of(&calls, 1)
        );
    }

    #[tokio::test]
    async fn the_liked_list_stops_at_the_page_limit() {
        // 本家が次ページを返し続けても止まること (一覧は 1000 件までしか取れない)。
        let pages = (0..LIKED_PAGE_MAX + 5)
            .map(|at| liked_page(&["v"], Some(&format!("page-{at}"))))
            .collect();
        let backend = FakeBackend::new().with_responses(pages);

        let liked = refresh_liked_videos(&backend, "at-1")
            .await
            .expect("取れる");

        assert_eq!(backend.calls().len(), LIKED_PAGE_MAX);
        assert_eq!(liked.len(), LIKED_PAGE_MAX);
    }

    #[tokio::test]
    async fn a_refused_liked_page_is_reported() {
        let backend = FakeBackend::new().with_responses(vec![
            liked_page(&["v1"], Some("page-2")),
            failed(403, r#"{"error":{"code":403,"message":"quotaExceeded"}}"#),
        ]);

        let error = refresh_liked_videos(&backend, "at-1")
            .await
            .expect_err("失敗する");

        assert!(error.contains("quotaExceeded"), "{error}");
    }

    #[tokio::test]
    async fn the_channels_are_asked_in_batches() {
        let wanted: Vec<String> = (0..CHANNEL_CHUNK + 2).map(|at| format!("UC{at}")).collect();
        let backend = std::sync::Arc::new(FakeBackend::new().with_responses(vec![
            subscription_page(&["UC0"]),
            subscription_page(&["UC50"]),
        ]));

        let subscribed = refresh_subscriptions(std::sync::Arc::clone(&backend), "at-1", &wanted, 3)
            .await
            .expect("取れる");

        assert_eq!(subscribed, ids(&["UC0", "UC50"]));
        let calls = backend.calls();
        assert_eq!(calls.len(), 2, "50 件ずつに分ける");
        assert_eq!(asked_channels(&calls[0]).len(), CHANNEL_CHUNK);
        assert_eq!(asked_channels(&calls[1]), ids(&["UC50", "UC51"]));
    }

    #[tokio::test]
    async fn asking_about_no_channel_does_not_call_the_api() {
        let backend = std::sync::Arc::new(FakeBackend::new());

        let subscribed = refresh_subscriptions(std::sync::Arc::clone(&backend), "at-1", &[], 3)
            .await
            .expect("何もしない");

        assert!(subscribed.is_empty());
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn one_refused_batch_fails_the_whole_channel_refresh() {
        // 半分だけ反映すると、返らなかった ID を未登録として確定させてしまう。
        let wanted: Vec<String> = (0..CHANNEL_CHUNK + 1).map(|at| format!("UC{at}")).collect();
        let backend = std::sync::Arc::new(
            FakeBackend::new().with_responses(vec![subscription_page(&["UC0"]), failed(500, "{}")]),
        );

        let error = refresh_subscriptions(std::sync::Arc::clone(&backend), "at-1", &wanted, 3)
            .await
            .expect_err("失敗する");

        assert!(error.contains("500"), "{error}");
    }

    #[tokio::test]
    async fn the_batches_run_no_more_than_the_allowed_number_at_a_time() {
        let wanted: Vec<String> = (0..CHANNEL_CHUNK * 3).map(|at| format!("UC{at}")).collect();
        let backend = std::sync::Arc::new(FakeBackend::new().with_responses(vec![
            subscription_page(&[]),
            subscription_page(&[]),
            subscription_page(&[]),
        ]));

        refresh_subscriptions(std::sync::Arc::clone(&backend), "at-1", &wanted, 1)
            .await
            .expect("取れる");

        assert_eq!(backend.calls().len(), 3);
    }

    #[tokio::test]
    async fn a_background_check_without_a_stored_token_does_not_open_a_browser() {
        let paths = paths("no-token-for-check");
        let backend = FakeBackend::new();

        let token = access_token_for_refresh(&backend, &paths)
            .await
            .expect("認証していないだけなので失敗ではない");

        assert_eq!(token, None);
        assert!(backend.opened().is_empty(), "確認のために認可を求めない");
        assert!(!backend.bound());
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_background_check_uses_the_stored_refresh_token() {
        let paths = paths("token-for-check");
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        let backend = FakeBackend::new().with_responses(vec![ok(REFRESHED_JSON)]);

        let token = access_token_for_refresh(&backend, &paths)
            .await
            .expect("取れる");

        assert_eq!(token.as_deref(), Some("at-2"));
        assert!(backend.opened().is_empty());
        let calls = backend.calls();
        assert_eq!(calls.len(), 1);
        assert_argv_is_clean(&calls[0], &["dummy-secret", "rt-1"]);
    }

    #[test]
    fn the_notice_matches_the_action() {
        assert_eq!(
            Action::Subscribe("UC1".to_string()).notice(),
            SUBSCRIBE_NOTICE
        );
        assert_eq!(Action::Like("v".to_string()).notice(), LIKE_NOTICE);
        assert_eq!(Action::Save("v".to_string()).notice(), SAVE_NOTICE);
    }

    #[test]
    fn config_escape_writes_the_control_characters_as_escapes() {
        assert_eq!(config_escape("a\tb\rc\u{b}d"), "a\\tb\\rc\\vd");
    }

    // ---- tuitube への保存 ----

    /// 自分のプレイリスト一覧の 1 ページ。
    fn playlist_page(items: &[(&str, &str)], next: Option<&str>) -> String {
        let items: Vec<serde_json::Value> = items
            .iter()
            .map(|(id, title)| serde_json::json!({"id": id, "snippet": {"title": title}}))
            .collect();
        let mut page = serde_json::json!({ "items": items });
        if let Some(next) = next {
            page["nextPageToken"] = serde_json::json!(next);
        }
        page.to_string()
    }

    fn json_body(request: &Request) -> serde_json::Value {
        serde_json::from_str(&secret_of(request, "--data")).expect("JSON の本文")
    }

    /// 保存済みの refresh_token を持つ置き場。ブラウザでの認可を飛ばす。
    fn signed_in(name: &str) -> Paths {
        let paths = paths(name);
        save_refresh_token(&paths.token, "rt-1").expect("書ける");
        paths
    }

    const NO_ITEMS: &str = r#"{"items":[]}"#;

    #[test]
    fn list_my_playlists_request_asks_for_a_page_of_my_playlists() {
        let first = list_my_playlists_request("at-1", None);
        assert_eq!(
            first.url(),
            format!("{PLAYLISTS_ENDPOINT}?part=snippet&mine=true&maxResults=50")
        );
        assert_eq!(secret_of(&first, "--header"), "Authorization: Bearer at-1");
        let next = list_my_playlists_request("at-1", Some("p 2"));
        assert!(next.url().ends_with("&pageToken=p%202"), "{}", next.url());
    }

    #[test]
    fn create_playlist_request_makes_a_private_tuitube_playlist() {
        let request = create_playlist_request("at-1");
        assert_eq!(
            request.url(),
            format!("{PLAYLISTS_ENDPOINT}?part=snippet,status")
        );
        assert!(request.args.contains(&"POST".to_string()));
        let body = json_body(&request);
        assert_eq!(
            body.pointer("/snippet/title"),
            Some(&serde_json::json!("tuitube"))
        );
        assert_eq!(
            body.pointer("/status/privacyStatus"),
            Some(&serde_json::json!("private"))
        );
        assert_argv_is_clean(&request, &["at-1"]);
    }

    #[test]
    fn find_playlist_item_request_looks_for_the_video_in_the_playlist() {
        let request = find_playlist_item_request("at-1", "PL 1", "vid 1");
        assert_eq!(
            request.url(),
            format!(
                "{PLAYLIST_ITEMS_ENDPOINT}?part=id&playlistId=PL%201&videoId=vid%201&maxResults=1"
            )
        );
        assert_eq!(
            secret_of(&request, "--header"),
            "Authorization: Bearer at-1"
        );
    }

    #[test]
    fn insert_playlist_item_request_adds_the_video() {
        let request = insert_playlist_item_request("at-1", "PL1", "vid1");
        assert_eq!(
            request.url(),
            format!("{PLAYLIST_ITEMS_ENDPOINT}?part=snippet")
        );
        assert!(request.args.contains(&"POST".to_string()));
        let body = json_body(&request);
        assert_eq!(
            body.pointer("/snippet/playlistId"),
            Some(&serde_json::json!("PL1"))
        );
        assert_eq!(
            body.pointer("/snippet/resourceId"),
            Some(&serde_json::json!({"kind": "youtube#video", "videoId": "vid1"}))
        );
        assert_argv_is_clean(&request, &["at-1"]);
    }

    #[tokio::test]
    async fn saving_adds_the_video_to_the_tuitube_playlist() {
        let paths = signed_in("save-existing");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            ok(&playlist_page(
                &[("PL0", "作業用BGM"), ("PL1", "tuitube")],
                None,
            )),
            ok(NO_ITEMS),
            ok(r#"{"id":"item1"}"#),
        ]);

        let notice = run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect("通る");

        assert_eq!(notice, SAVED_NOTICE);
        let calls = backend.calls();
        assert_eq!(calls.len(), 4, "トークン・一覧・入っているかの確認・追加");
        assert!(
            url_of(&calls, 2).contains("playlistId=PL1"),
            "{}",
            url_of(&calls, 2)
        );
        assert_eq!(
            json_body(&calls[3]).pointer("/snippet/playlistId"),
            Some(&serde_json::json!("PL1"))
        );
        assert_eq!(
            secret_of(&calls[3], "--header"),
            "Authorization: Bearer at-2"
        );
    }

    #[tokio::test]
    async fn saving_finds_the_playlist_on_a_later_page() {
        let paths = signed_in("save-later-page");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            ok(&playlist_page(&[("PL0", "作業用BGM")], Some("p2"))),
            ok(&playlist_page(&[("PL9", "tuitube")], None)),
            ok(NO_ITEMS),
            ok(r#"{"id":"item1"}"#),
        ]);

        run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect("通る");

        let calls = backend.calls();
        assert!(
            url_of(&calls, 2).ends_with("&pageToken=p2"),
            "{}",
            url_of(&calls, 2)
        );
        assert_eq!(
            json_body(&calls[4]).pointer("/snippet/playlistId"),
            Some(&serde_json::json!("PL9"))
        );
    }

    #[tokio::test]
    async fn saving_makes_the_playlist_when_there_is_none() {
        let paths = signed_in("save-create");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            // 名前が完全に同じものだけを使う。
            ok(&playlist_page(&[("PL0", "tuitube のメモ")], None)),
            ok(r#"{"id":"PLnew"}"#),
            ok(r#"{"id":"item1"}"#),
        ]);

        let notice = run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect("通る");

        assert_eq!(notice, SAVED_NOTICE);
        let calls = backend.calls();
        assert_eq!(
            url_of(&calls, 2),
            format!("{PLAYLISTS_ENDPOINT}?part=snippet,status")
        );
        // 作った直後は中身を聞くと見つからないと返る (実測)。空なので聞かずに足す。
        assert_eq!(calls.len(), 4, "トークン・一覧・作成・追加");
        assert_eq!(
            url_of(&calls, 3),
            format!("{PLAYLIST_ITEMS_ENDPOINT}?part=snippet")
        );
        assert_eq!(
            json_body(&calls[3]).pointer("/snippet/playlistId"),
            Some(&serde_json::json!("PLnew"))
        );
    }

    #[tokio::test]
    async fn a_video_already_in_the_playlist_is_not_added_twice() {
        let paths = signed_in("save-duplicate");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            ok(&playlist_page(&[("PL1", "tuitube")], None)),
            ok(r#"{"items":[{"id":"item1"}]}"#),
        ]);

        let notice = run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect("通る");

        assert_eq!(notice, ALREADY_SAVED_NOTICE);
        assert_eq!(backend.calls().len(), 3, "追加は送らない");
    }

    #[tokio::test]
    async fn a_refused_save_says_why() {
        let paths = signed_in("save-refused");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            ok(&playlist_page(&[("PL1", "tuitube")], None)),
            ok(NO_ITEMS),
            failed(
                403,
                r#"{"error":{"message":"quota","errors":[{"reason":"quotaExceeded"}]}}"#,
            ),
        ]);

        let error = run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect_err("通らない");

        assert!(error.contains("quota"), "{error}");
    }

    #[tokio::test]
    async fn a_playlist_list_that_cannot_be_read_stops_the_save() {
        let paths = signed_in("save-list-refused");
        let backend = FakeBackend::new().with_responses(vec![
            ok(REFRESHED_JSON),
            failed(401, r#"{"error":{"message":"invalid credentials"}}"#),
        ]);

        let error = run(&backend, &paths, &Action::Save("vid1".to_string()))
            .await
            .expect_err("通らない");

        assert!(error.contains("invalid credentials"), "{error}");
        assert_eq!(backend.calls().len(), 2, "作りも足しもしない");
    }
}
