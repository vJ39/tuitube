use crate::category::Tabs;
use crate::comments::{Comment, Comments};
use crate::cookies::{CookieState, Target};
use crate::display::{DisplayMode, FpsCap};
use crate::mpv::MpvCommand;
use crate::query::QueryEditor;
use crate::rgb::RgbImage;
use crate::search::{SearchReport, SearchResult};
use crate::seekbar::SeekBarState;
use crate::settings::{
    EnvOverridden, FPS_LIMIT_VAR, MAX_FPS_CAP, MAX_SEARCH_LIMIT, MAX_SEARCH_TIMEOUT_SECS,
    MAX_THUMB_TIMEOUT_SECS, MIN_SEARCH_LIMIT, MIN_SEARCH_TIMEOUT_SECS, Settings,
};
use crate::speed::{Polled, Speed};
use crate::subtitles::SubtitleState;
use crate::thumbs::Thumbs;
use crate::video::VideoSink;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use serde_json::Value;
use std::time::{Duration, Instant};

/// シークを送ってから確定値を待つ間、ポーリングの古い値を無視する時間。
pub const SEEK_HOLD: Duration = Duration::from_secs(2);
/// 相対シークは keyframes で着地がずれるので、この幅までは目標どおり着いたとみなす。
pub const SEEK_TOLERANCE_SECS: f64 = 3.0;
/// 速度を送ってから確定値を待つ間、ポーリングの古い値を無視する時間。
pub const SPEED_HOLD: Duration = Duration::from_secs(2);
/// 期限つきの知らせを出しておく時間。
pub const NOTICE_TTL: Duration = Duration::from_secs(3);

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize {
        width: u16,
        height: u16,
    },
    // nonce identifies the search, so results of a superseded query are ignored.
    SearchDone {
        nonce: u64,
        target: Target,
        report: SearchReport,
    },
    // nonce identifies the mpv instance, so events from an already replaced player are ignored.
    MpvProperty {
        nonce: u64,
        id: u64,
        data: Option<Value>,
    },
    /// 映像が1フレーム揃った合図。これが無いと再描画はティッカー任せになる。
    VideoFrame {
        nonce: u64,
    },
    /// mpv の映像出力を読めなくなった。放っておくと mpv だけが生き残る。
    VideoError {
        nonce: u64,
        error: String,
    },
    MpvExited {
        nonce: u64,
        error: Option<String>,
    },
    /// サムネイルのデコードが終わった。nonce は検索と共用で、
    /// 検索が入れ替わっていれば古い画像として捨てる。
    ThumbsReady {
        nonce: u64,
        target_px: (u32, u32),
        images: Vec<(String, Result<RgbImage, ()>)>,
        /// 一度だけ伝える事情 (保存先を作れない等)。
        notice: Option<String>,
        /// 以後サムネイル取得を止める理由 (curl が無い)。
        disable: Option<String>,
    },
    /// コメントの取得が終わった。nonce は再生ごとに進むので、
    /// 前の動画ぶんが遅れて届いても混ざらない。
    CommentsReady {
        nonce: u64,
        video_id: String,
        comments: Result<Vec<Comment>, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Results,
    Playing,
    Settings,
}

/// 設定画面で編集できる項目。画面の並びはこの順。
/// ここに無い値 (window.* / cookies.* / mpv.extra_args / subtitles.lang / categories) は
/// config.toml を直接編集する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    DisplayMode,
    DisplayQuality,
    FpsCap,
    SubtitlesEnabled,
    SearchLayout,
    SearchLimit,
    SearchTimeoutSecs,
    ThumbnailsEnabled,
    ThumbnailsMaxCached,
    ThumbnailsTimeoutSecs,
}

pub const SETTINGS_ITEMS: [SettingsItem; 10] = [
    SettingsItem::DisplayMode,
    SettingsItem::DisplayQuality,
    SettingsItem::FpsCap,
    SettingsItem::SubtitlesEnabled,
    SettingsItem::SearchLayout,
    SettingsItem::SearchLimit,
    SettingsItem::SearchTimeoutSecs,
    SettingsItem::ThumbnailsEnabled,
    SettingsItem::ThumbnailsMaxCached,
    SettingsItem::ThumbnailsTimeoutSecs,
];

/// 数値項目の 1 回ぶんの刻み。
const FPS_CAP_STEP: u64 = 5;
const SEARCH_LIMIT_STEP: u64 = 1;
const SEARCH_TIMEOUT_STEP: u64 = 5;
const MAX_CACHED_STEP: u64 = 50;
const THUMB_TIMEOUT_STEP: u64 = 5;
/// 0 秒では 1 枚も取れないので、秒数はここまでしか下げない。
const MIN_THUMB_TIMEOUT_SECS: u64 = 1;
/// max_cached に上限は無い。刻みの計算にだけ使う。
const NO_MAX: u64 = u64::MAX;

/// 刻みの次の目盛り。上限では止まる。
fn step_up(current: u64, step: u64, max: u64) -> u64 {
    (current / step * step).saturating_add(step).min(max)
}

/// 刻みの 1 つ前の目盛り。下限が刻みに乗っていなくても、目盛りの基準はずらさない。
/// (例: 刻み 5 / 下限 1 で 5 → 1 → 5 と戻る)
fn step_down(current: u64, step: u64, min: u64) -> u64 {
    (current.saturating_sub(1) / step * step).max(min)
}

impl SettingsItem {
    /// 設定ファイルのキーをそのまま出す。画面で見た項目を config.toml で探せるため。
    pub fn label(self) -> &'static str {
        match self {
            Self::DisplayMode => "display.mode",
            Self::DisplayQuality => "display.quality",
            Self::FpsCap => "fps_cap",
            Self::SubtitlesEnabled => "subtitles.enabled",
            Self::SearchLayout => "search.layout",
            Self::SearchLimit => "search.limit",
            Self::SearchTimeoutSecs => "search.timeout_secs",
            Self::ThumbnailsEnabled => "thumbnails.enabled",
            Self::ThumbnailsMaxCached => "thumbnails.max_cached",
            Self::ThumbnailsTimeoutSecs => "thumbnails.timeout_secs",
        }
    }

    /// 値も設定ファイルの表記に合わせる。
    pub fn value(self, settings: &Settings) -> String {
        match self {
            Self::DisplayMode => settings.display.mode.key().to_string(),
            Self::DisplayQuality => settings.display.quality.label().to_string(),
            Self::FpsCap => match settings.fps_cap {
                Some(cap) => cap.get().to_string(),
                None => "0 (無制限)".to_string(),
            },
            Self::SubtitlesEnabled => settings.subtitles.enabled.to_string(),
            Self::SearchLayout => settings.search.layout.key().to_string(),
            Self::SearchLimit => settings.search.limit.to_string(),
            Self::SearchTimeoutSecs => settings.search.timeout.as_secs().to_string(),
            Self::ThumbnailsEnabled => settings.thumbnails.enabled.to_string(),
            Self::ThumbnailsMaxCached => settings.thumbnails.max_cached.to_string(),
            Self::ThumbnailsTimeoutSecs => settings.thumbnails.timeout.as_secs().to_string(),
        }
    }

    pub fn row(self, settings: &Settings) -> String {
        format!("{}: {}", self.label(), self.value(settings))
    }

    /// 環境変数が上書きしている項目なら、その変数名。保存では書き換えないので画面にも出す。
    pub fn env_var(self, overridden: &EnvOverridden) -> Option<&'static str> {
        match self {
            Self::FpsCap => overridden.fps_cap.is_some().then_some(FPS_LIMIT_VAR),
            _ => None,
        }
    }

    /// 数字キーで値を直接打ち込める項目。選択肢と bool は ←→ だけで動かす。
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::FpsCap
                | Self::SearchLimit
                | Self::SearchTimeoutSecs
                | Self::ThumbnailsMaxCached
                | Self::ThumbnailsTimeoutSecs
        )
    }

    /// 上限の桁数。これを超えた入力はパースできず捨てられるので、打ち込みはここで止める。
    /// 数値でない項目は 0 桁 = 打ち込みを受けない。
    pub fn max_digits(self) -> usize {
        let max: u64 = match self {
            Self::FpsCap => u64::from(MAX_FPS_CAP),
            Self::SearchLimit => MAX_SEARCH_LIMIT as u64,
            Self::SearchTimeoutSecs => MAX_SEARCH_TIMEOUT_SECS,
            // max_cached に上限は無いので、apply_numeric が受け取れる最大値で数える。
            Self::ThumbnailsMaxCached => u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            Self::ThumbnailsTimeoutSecs => MAX_THUMB_TIMEOUT_SECS,
            Self::DisplayMode
            | Self::DisplayQuality
            | Self::SubtitlesEnabled
            | Self::SearchLayout
            | Self::ThumbnailsEnabled => return 0,
        };
        max.to_string().len()
    }

    /// 打ち込んだ値の確定。範囲は ←→ と同じで、外れていれば端へ寄せる。
    /// 数として読めない入力と数値でない項目は何もせず false を返す。
    pub fn apply_numeric(self, settings: &mut Settings, raw: &str) -> bool {
        let Ok(value) = raw.parse::<u64>() else {
            return false;
        };
        match self {
            Self::FpsCap => {
                // 0 は「制限なし」。FpsCap は 1 以上しか作れない。
                settings.fps_cap = FpsCap::new(value.min(u64::from(MAX_FPS_CAP)) as u32);
            }
            Self::SearchLimit => {
                let min = MIN_SEARCH_LIMIT as u64;
                let max = MAX_SEARCH_LIMIT as u64;
                settings.search.limit = value.clamp(min, max) as usize;
            }
            Self::SearchTimeoutSecs => {
                let secs = value.clamp(MIN_SEARCH_TIMEOUT_SECS, MAX_SEARCH_TIMEOUT_SECS);
                settings.search.timeout = Duration::from_secs(secs);
            }
            Self::ThumbnailsMaxCached => {
                settings.thumbnails.max_cached = usize::try_from(value).unwrap_or(usize::MAX);
            }
            Self::ThumbnailsTimeoutSecs => {
                let secs = value.clamp(MIN_THUMB_TIMEOUT_SECS, MAX_THUMB_TIMEOUT_SECS);
                settings.thumbnails.timeout = Duration::from_secs(secs);
            }
            // 選択肢と bool は打ち込みを受けない。is_numeric() と対で書き分ける。
            Self::DisplayMode
            | Self::DisplayQuality
            | Self::SubtitlesEnabled
            | Self::SearchLayout
            | Self::ThumbnailsEnabled => return false,
        }
        true
    }

    /// ←→ 1 回ぶんの変化。→ は次の値、← は前の値。bool はどちらでも反転する。
    /// 数値は刻みの目盛りを動き、範囲の端で止まる (ラップしない)。
    pub fn adjust(self, settings: &mut Settings, delta: i32) {
        let up = delta >= 0;
        match self {
            Self::DisplayMode => {
                let mode = settings.display.mode;
                settings.display.mode = if up { mode.next() } else { mode.prev() };
            }
            Self::DisplayQuality => {
                let quality = settings.display.quality;
                settings.display.quality = if up { quality.next() } else { quality.prev() };
            }
            Self::FpsCap => {
                let current = u64::from(settings.fps_cap.map(FpsCap::get).unwrap_or(0));
                // 0 は「制限なし」。FpsCap は 1 以上しか作れない。
                let next = step(current, FPS_CAP_STEP, 0, u64::from(MAX_FPS_CAP), up);
                settings.fps_cap = FpsCap::new(next as u32);
            }
            Self::SubtitlesEnabled => settings.subtitles.enabled = !settings.subtitles.enabled,
            Self::SearchLayout => {
                let layout = settings.search.layout;
                settings.search.layout = if up { layout.next() } else { layout.prev() };
            }
            Self::SearchLimit => {
                let current = settings.search.limit as u64;
                let min = MIN_SEARCH_LIMIT as u64;
                let max = MAX_SEARCH_LIMIT as u64;
                settings.search.limit = step(current, SEARCH_LIMIT_STEP, min, max, up) as usize;
            }
            Self::SearchTimeoutSecs => {
                let current = settings.search.timeout.as_secs();
                let min = MIN_SEARCH_TIMEOUT_SECS;
                let secs = step(
                    current,
                    SEARCH_TIMEOUT_STEP,
                    min,
                    MAX_SEARCH_TIMEOUT_SECS,
                    up,
                );
                settings.search.timeout = Duration::from_secs(secs);
            }
            Self::ThumbnailsEnabled => settings.thumbnails.enabled = !settings.thumbnails.enabled,
            Self::ThumbnailsMaxCached => {
                let current = settings.thumbnails.max_cached as u64;
                settings.thumbnails.max_cached =
                    step(current, MAX_CACHED_STEP, 0, NO_MAX, up) as usize;
            }
            Self::ThumbnailsTimeoutSecs => {
                let current = settings.thumbnails.timeout.as_secs();
                let min = MIN_THUMB_TIMEOUT_SECS;
                let secs = step(current, THUMB_TIMEOUT_STEP, min, MAX_THUMB_TIMEOUT_SECS, up);
                settings.thumbnails.timeout = Duration::from_secs(secs);
            }
        }
    }
}

/// 数値項目を 1 目盛りぶん動かす。
fn step(current: u64, width: u64, min: u64, max: u64, up: bool) -> u64 {
    if up {
        step_up(current, width, max).max(min)
    } else {
        step_down(current, width, min).min(max)
    }
}

/// シーク送信から確定までの先行表示。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingSeek {
    pub target: f64,
    pub sent_at: Instant,
}

#[derive(Debug, Default, Clone)]
pub struct Playback {
    pub title: String,
    /// 再生中の動画の URL。コピー用にここで持つ。
    pub url: String,
    pub paused: Option<bool>,
    pub time_pos: Option<f64>,
    pub duration: Option<f64>,
    pub volume: Option<f64>,
    pub pending_seek: Option<PendingSeek>,
    /// mpv が実際に使っている VO。要求した表示モードが通ったかはこれで見る。
    pub current_vo: Option<String>,
}

impl Playback {
    /// 送信と同時に表示だけ目標値へ進める。
    pub fn begin_seek(&mut self, target: f64, now: Instant) {
        self.time_pos = Some(target);
        self.pending_seek = Some(PendingSeek {
            target,
            sent_at: now,
        });
    }

    /// 相対シークの基準。押し続けたときに目標値が積み上がる。
    pub fn seek_base(&self) -> Option<f64> {
        self.pending_seek.map(|p| p.target).or(self.time_pos)
    }

    /// ポーリングの値を取り込む。保持時間中の古い値は捨てる。
    pub fn reconcile_time_pos(&mut self, polled: Option<f64>, now: Instant) {
        // シーク直後のポーリングは playback-restart より前に返って旧位置を寄越す。
        if let Some(pending) = self.pending_seek {
            let within_hold = now.saturating_duration_since(pending.sent_at) < SEEK_HOLD;
            let stale = polled.is_none_or(|p| (p - pending.target).abs() > SEEK_TOLERANCE_SECS);
            if within_hold && stale {
                return;
            }
        }
        self.pending_seek = None;
        self.time_pos = polled;
    }
}

pub struct App {
    pub mode: Mode,
    /// 検索欄の文字列・カーソル・選択範囲。
    pub query: QueryEditor,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub searching: bool,
    pub error: Option<String>,
    /// 操作が失敗した理由を消す時刻。ポーリングが上書きしてよいものは None。
    pub error_until: Option<Instant>,
    /// 設定を読み替えたときなど、エラーではないが一度伝えたいこと。
    pub notice: Option<String>,
    /// 知らせを消す時刻。出し続けるものは None。
    pub notice_until: Option<Instant>,
    /// ブラウザ cookie 連携の状態。検索・再生・表示がここを見る。
    pub cookies: CookieState,
    pub playback: Playback,
    /// 起動時に読んだ設定。
    pub settings: Settings,
    /// 要求中の表示モード。実際にどちらで出ているかは playback.current_vo。
    pub display: DisplayMode,
    /// 再生速度。動画をまたいで持ち越すので Playback でなく App が持つ。
    pub speed: Speed,
    /// 速度を送った時刻。ここから SPEED_HOLD の間は、食い違うポーリング値を捨てる。
    pub speed_sent_at: Option<Instant>,
    /// 字幕の表示状態。速度と同じく動画をまたいで持ち越す。
    pub subtitles: SubtitleState,
    /// 再生中だけ、mpv の kitty 出力を受け取るスロットが入る。
    pub video: Option<VideoSink>,
    /// 直近の terminal.draw() が描いた画面。マウスの当たり判定はこれで割り付ける。
    pub screen: Rect,
    /// results を入れ替えた回数。
    pub results_generation: u64,
    /// 直近の draw が描いた results_generation。
    pub drawn_generation: u64,
    pub seek_bar: SeekBarState,
    pub should_quit: bool,
    /// 擬似カテゴリタブ。results / selected / scroll はここの写し。
    pub tabs: Tabs,
    pub thumbs: Thumbs,
    /// 再生中の動画のコメント。取得状態と表示の on/off。
    pub comments: Comments,
    /// 格子の先頭表示位置 (項目インデックス)。描画のたびに列数で丸める。
    pub scroll: usize,
    /// 設定画面で選んでいる行。SETTINGS_ITEMS の範囲へ丸めて使う。
    pub settings_selected: usize,
    /// 設定画面を開いた元のモード。閉じたらここへ戻る。
    pub settings_return: Mode,
    /// 設定画面を開いた時点 (または最後に保存した時点) の設定。Esc はここへ戻す。
    pub settings_backup: Settings,
    /// 数値項目へ打ち込んでいる途中の文字列。None なら通常表示。
    /// settings へ書くのは確定したときだけなので、settings_backup とは別に持つ。
    pub settings_edit: Option<String>,
    /// 環境変数が上書きしている項目。画面の断りと、保存時の書き戻しに使う。
    pub env_overridden: EnvOverridden,
}

impl Default for App {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            mode: Mode::Input,
            query: QueryEditor::default(),
            results: Vec::new(),
            selected: 0,
            searching: false,
            error: None,
            error_until: None,
            notice: None,
            notice_until: None,
            cookies: CookieState::default(),
            playback: Playback::default(),
            display: DisplayMode::default(),
            speed: Speed::NORMAL,
            speed_sent_at: None,
            subtitles: SubtitleState::from_settings(&settings.subtitles),
            settings_backup: settings.clone(),
            settings,
            video: None,
            screen: Rect::default(),
            results_generation: 0,
            drawn_generation: 0,
            seek_bar: SeekBarState::default(),
            should_quit: false,
            tabs: Tabs::default(),
            thumbs: Thumbs::default(),
            comments: Comments::default(),
            scroll: 0,
            settings_selected: 0,
            settings_return: Mode::Input,
            settings_edit: None,
            env_overridden: EnvOverridden::default(),
        }
    }
}

impl App {
    /// 気づくまで出し続ける知らせ。
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
        self.notice_until = None;
    }

    /// 操作に対する短い返事。expire_notice が期限後に消す。
    pub fn set_temporary_notice(&mut self, notice: String, now: Instant) {
        self.notice = Some(notice);
        self.notice_until = Some(now + NOTICE_TTL);
    }

    pub fn expire_notice(&mut self, now: Instant) {
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice = None;
            self.notice_until = None;
        }
    }

    /// ポーリングが上書き・消去してよいエラー。
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
        self.error_until = None;
    }

    /// 操作が失敗した理由。読む間もなく消えないよう、知らせと同じだけ出しておく。
    pub fn set_temporary_error(&mut self, error: String, now: Instant) {
        self.error = Some(error);
        self.error_until = Some(now + NOTICE_TTL);
    }

    pub fn expire_error(&mut self, now: Instant) {
        if self.error_until.is_some_and(|until| now >= until) {
            self.set_error(None);
        }
    }

    /// ポーリングが成功したときの後片付け。期限内の操作エラーは残す。
    pub fn clear_polled_error(&mut self, now: Instant) {
        if self.error_until.is_some_and(|until| now < until) {
            return;
        }
        self.set_error(None);
    }

    pub fn select_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.results.len();
    }

    pub fn select_prev(&mut self) {
        if self.results.is_empty() {
            return;
        }
        self.selected = (self.selected + self.results.len() - 1) % self.results.len();
    }

    pub fn selected_result(&self) -> Option<&SearchResult> {
        self.results.get(self.selected)
    }

    pub fn set_results(&mut self, results: Vec<SearchResult>, target: &Target) {
        let state = self.tabs.state_mut();
        state.results = results;
        state.selected = 0;
        state.scroll = 0;
        // 0 件でも読み込み済みにする。戻るたびに同じ検索を投げ直さないため。
        state.loaded = true;
        self.sync_from_tab();
        if self.results.is_empty() {
            self.error = Some(target.empty_message(self.cookies.for_search()));
            self.enter_search_mode(Mode::Input);
        } else {
            self.enter_search_mode(Mode::Results);
        }
    }

    /// 検索や再生の終わりで戻る検索画面のモード。設定画面を開いている間は戻り先だけ
    /// 書き換える。裏で終わった検索が、編集中の設定画面を閉じてしまわないため。
    pub fn enter_search_mode(&mut self, mode: Mode) {
        if self.mode == Mode::Settings {
            self.settings_return = mode;
        } else {
            self.mode = mode;
        }
    }

    /// 描いたことを控える。ここから結果が入れ替わるまでのクリックは、見えている画面を指す。
    pub fn mark_drawn(&mut self) {
        self.drawn_generation = self.results_generation;
    }

    /// 今の results が、利用者の見ている画面に描かれたものか。
    /// 偽の間に届いたクリックは、入れ替わる前の画面を狙ったもの。
    pub fn results_are_drawn(&self) -> bool {
        self.drawn_generation == self.results_generation
    }

    /// 今のタブの内容を App 側の写しへ取り込む。サムネイルの状態表も入れ替える。
    pub fn sync_from_tab(&mut self) {
        let state = self.tabs.state();
        self.results_generation = self.results_generation.wrapping_add(1);
        self.results = state.results.clone();
        self.selected = state.selected;
        self.scroll = state.scroll;
        let ids = self.result_ids();
        self.thumbs.reset(&ids);
        self.thumbs.mark_dirty();
    }

    /// 画面から離れる前に、タブへ選択位置を書き戻す。
    pub fn store_to_tab(&mut self) {
        let (selected, scroll) = (self.selected, self.scroll);
        let state = self.tabs.state_mut();
        state.selected = selected;
        state.scroll = scroll;
    }

    pub fn result_ids(&self) -> Vec<String> {
        self.results.iter().map(|r| r.id.clone()).collect()
    }

    /// ポーリングの応答を取り込む。mpv へ送り返すものがあれば返す。
    pub fn apply_property(&mut self, id: u64, data: Option<Value>) -> Option<MpvCommand> {
        match id {
            crate::mpv::REQ_TIME_POS => self
                .playback
                .reconcile_time_pos(data.and_then(|v| v.as_f64()), Instant::now()),
            crate::mpv::REQ_DURATION => {
                self.playback.duration = data.and_then(|v| v.as_f64());
                // 字幕が「無い」と言い出すのは、長さが取れてから数秒後。
                self.subtitles
                    .observe_loaded(self.playback.duration.is_some(), Instant::now());
            }
            crate::mpv::REQ_PAUSE => self.playback.paused = data.and_then(|v| v.as_bool()),
            crate::mpv::REQ_VOLUME => self.playback.volume = data.and_then(|v| v.as_f64()),
            crate::mpv::REQ_SID => self.subtitles.observe_sid(data.as_ref()),
            crate::mpv::REQ_SUB_LANG => self.subtitles.observe_sub_lang(data.as_ref()),
            crate::mpv::REQ_CURRENT_VO => {
                self.playback.current_vo =
                    data.as_ref().and_then(|v| v.as_str()).map(str::to_string);
            }
            // 取れないときは触らない。mpv 側の値だけが正とは限らない。
            crate::mpv::REQ_SPEED => {
                if let Some(value) = data.and_then(|v| v.as_f64()) {
                    return self.reconcile_speed(Polled::from_f64(value), Instant::now());
                }
            }
            _ => {}
        }
        None
    }

    /// 自分で決めた速度を控える。mpv が確定するまでのポーリング値はこれで弾く。
    pub fn set_speed_sent(&mut self, speed: Speed, now: Instant) {
        self.speed = speed;
        self.speed_sent_at = Some(now);
    }

    /// ポーリングの速度を取り込む。保持時間中の古い値は捨てる。
    /// mpv 側が範囲外なら、丸めた値を送り返して表示と実際の再生を揃える。
    fn reconcile_speed(&mut self, polled: Polled, now: Instant) -> Option<MpvCommand> {
        // 送信前に発行された get_property の応答は、後から古い値を寄越す。
        if let Some(sent_at) = self.speed_sent_at {
            let within_hold = now.saturating_duration_since(sent_at) < SPEED_HOLD;
            if within_hold && polled.speed != self.speed {
                return None;
            }
        }
        if polled.clamped {
            self.set_speed_sent(polled.speed, now);
            return Some(polled.speed.command());
        }
        self.speed_sent_at = None;
        self.speed = polled.speed;
        None
    }

    /// 選択中の設定項目。壊れた添字でも必ず 1 つ返す。
    pub fn settings_item(&self) -> SettingsItem {
        SETTINGS_ITEMS[self.settings_selected.min(SETTINGS_ITEMS.len() - 1)]
    }

    /// 設定画面に並べる `ラベル: 現在値` の行。
    /// 環境変数が効いている行には、保存しても書き換わらない旨を添える。
    pub fn settings_rows(&self) -> Vec<String> {
        SETTINGS_ITEMS
            .iter()
            .map(|item| {
                let row = item.row(&self.settings);
                match item.env_var(&self.env_overridden) {
                    Some(var) => format!("{row}  ({var} が指定中。保存しません)"),
                    None => row,
                }
            })
            .collect()
    }

    /// 分岐は網羅する。モードを増やしたときの書き分け漏れをコンパイラに拾わせる。
    pub fn status_line(&self) -> String {
        let line = match self.mode {
            Mode::Playing => self.playing_status(),
            Mode::Input => self.search_status("検索したい語句を入力して Enter".to_string()),
            Mode::Results => self.search_status(self.results_status()),
            Mode::Settings => self.settings_status(),
        };
        // エラーが出ている行に足すと読みにくいので、そのときは譲る。
        match &self.notice {
            Some(notice) if self.error.is_none() => format!("{line}  |  {notice}"),
            _ => line,
        }
    }

    /// 再生中はエラーで再生状況を隠さず、併記する。
    fn playing_status(&self) -> String {
        let line = self.playback_line();
        match &self.error {
            Some(error) => format!("{line}  |  エラー: {error}"),
            None => line,
        }
    }

    /// 格子のタイトルは 18 桁ほどで切れるので、選択中の完全なタイトルはここに出す。
    fn results_status(&self) -> String {
        let count = format!("{} 件", self.results.len());
        if self.thumbs.is_fetching() {
            return format!("{count}  |  サムネイル取得中...");
        }
        match self.selected_result() {
            Some(result) => format!("{count}  |  {}", result.title),
            None => count,
        }
    }

    /// 設定画面は保存の成否をここで返す。ポーリングが上書きしない画面なので、
    /// エラーが出ていればそれだけを出す。
    fn settings_status(&self) -> String {
        if let Some(error) = &self.error {
            return format!("エラー: {error}");
        }
        format!(
            "設定 {}/{}",
            self.settings_selected.min(SETTINGS_ITEMS.len() - 1) + 1,
            SETTINGS_ITEMS.len()
        )
    }

    fn search_status(&self, idle: String) -> String {
        if let Some(error) = &self.error {
            return format!("エラー: {error}");
        }
        let line = if self.searching {
            "検索中...".to_string()
        } else {
            idle
        };
        // cookie 連携は GUI 側の事情で黙って効かなくなるので、常に現状を出しておく。
        match self.cookies.label() {
            Some(label) => format!("{line}  |  {label}"),
            None => line,
        }
    }

    /// 要求と mpv の実際が食い違う間は切替中と出す。切替は数秒かかることがある。
    pub fn display_label(&self) -> String {
        let fps = self
            .settings
            .fps_cap
            .map(|cap| format!(" {}fps", cap.get()))
            .unwrap_or_default();
        let detail = match self.display {
            DisplayMode::Embedded => format!("{fps} {}", self.settings.display.quality.label()),
            // 画質はピクセル予算なので、文字ブロックでは効かない。
            DisplayMode::Text => fps,
            DisplayMode::Window => String::new(),
        };
        let actual = DisplayMode::from_current_vo(self.playback.current_vo.as_deref());
        let transition = match actual {
            Some(actual) if actual != self.display => " (切替中)",
            _ => "",
        };
        format!("[{}{detail}{transition}]", self.display.label())
    }

    fn playback_line(&self) -> String {
        let state = match self.playback.paused {
            Some(true) => "PAUSED",
            Some(false) => "PLAYING",
            None => "状態不明",
        };
        let volume = self
            .playback
            .volume
            .map(|v| format!("  vol {v:.0}"))
            .unwrap_or_default();
        // 狭い端末では末尾から切れるので、字幕の印は行の前方に置く。
        let subtitles = self
            .subtitles
            .marker(&self.settings.subtitles, Instant::now())
            .map(|marker| format!("  {marker}"))
            .unwrap_or_default();
        format!(
            "{state}{subtitles}  {}  {} / {}{volume}  {}  {}",
            self.playback.title,
            format_time(self.playback.time_pos),
            format_time(self.playback.duration),
            self.speed.label(),
            self.display_label()
        )
    }
}

pub fn format_time(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return "--:--".to_string();
    };
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::{CookieSource, Feed};
    use crate::display::Quality;
    use crate::grid::LayoutMode;
    use crate::speed::Speed;
    use crate::subtitles::{SELECT_GRACE, SubtitleStatus};
    use serde_json::json;

    fn search_target() -> Target {
        Target::Search("q".to_string())
    }

    fn source() -> CookieSource {
        CookieSource::from_spec(Some("chrome")).expect("spec")
    }

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            title: format!("title {id}"),
            duration: None,
            uploader: None,
        }
    }

    /// 送り返しが要らない取り込み。返り値まで込みで確かめる。
    fn poll(app: &mut App, id: u64, data: Option<Value>) {
        assert_eq!(app.apply_property(id, data), None, "送り返しは出ない");
    }

    fn polled(speed: Speed) -> crate::speed::Polled {
        crate::speed::Polled {
            speed,
            clamped: false,
        }
    }

    fn playing_subtitle_app(title: &str) -> App {
        App {
            mode: Mode::Playing,
            playback: Playback {
                title: title.to_string(),
                paused: Some(false),
                ..Playback::default()
            },
            ..App::default()
        }
    }

    #[test]
    fn the_polled_sid_tells_whether_a_subtitle_track_is_selected() {
        let mut app = playing_subtitle_app("song");
        assert!(app.subtitles.wanted(), "[subtitles] enabled の既定は true");
        let now = Instant::now();
        app.subtitles.begin_playback(now);
        poll(&mut app, crate::mpv::REQ_DURATION, Some(json!(60.0)));

        poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
        assert_eq!(
            app.subtitles.status(&app.settings.subtitles, now),
            SubtitleStatus::Shown
        );

        // false は「トラックが選ばれていない」。猶予を過ぎたら字幕なしと判定する。
        // 長さを取り込んだ時刻は実時計なので、判定はその分だけ後ろで見る。
        poll(&mut app, crate::mpv::REQ_SID, Some(json!(false)));
        assert_eq!(
            app.subtitles
                .status(&app.settings.subtitles, now + SELECT_GRACE * 2),
            SubtitleStatus::Missing
        );
        // 応答が取れなかった回は「選ばれていない」と読まない。
        poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
        poll(&mut app, crate::mpv::REQ_SID, None);
        assert_eq!(
            app.subtitles.status(&app.settings.subtitles, now),
            SubtitleStatus::Shown
        );
    }

    #[test]
    fn a_playback_without_a_duration_still_reports_a_missing_subtitle() {
        // ライブ配信のように長さが取れない再生でも、いつまでも「字幕...」で止めない。
        let mut app = playing_subtitle_app("live");
        let now = Instant::now();
        app.subtitles.begin_playback(now);
        poll(&mut app, crate::mpv::REQ_DURATION, None);

        assert_eq!(
            app.subtitles
                .status(&app.settings.subtitles, now + crate::subtitles::LOAD_GRACE),
            SubtitleStatus::Missing
        );
    }

    #[test]
    fn the_status_line_names_the_track_that_mpv_chose() {
        // lang="ja-orig,ja" でも ja が選ばれることがある (実測)。印は選ばれた方を出す。
        let mut app = playing_subtitle_app("song");
        poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
        poll(&mut app, crate::mpv::REQ_SUB_LANG, Some(json!("ja")));
        assert!(app.status_line().starts_with("PLAYING  字幕ja  song"));
    }

    #[test]
    fn the_status_line_puts_the_subtitle_marker_right_after_the_state() {
        let mut app = playing_subtitle_app("song");
        poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
        let line = app.status_line();
        assert!(line.starts_with("PLAYING  字幕ja-orig  song"), "{line}");

        // 消しているときは桁を使わない。
        app.subtitles.set_wanted(false, Instant::now());
        let line = app.status_line();
        assert!(line.starts_with("PLAYING  song"), "{line}");
        assert!(!line.contains("字幕"), "{line}");
    }

    #[test]
    fn the_subtitle_marker_stays_ahead_of_a_long_title() {
        // 狭い端末では行の末尾から切れるので、印は必ずタイトルより前に出す。
        let mut app = playing_subtitle_app(&"長いタイトル".repeat(20));
        poll(&mut app, crate::mpv::REQ_SID, Some(json!(1)));
        let line = app.status_line();
        let marker_at = line.find("字幕").expect("印がある");
        let title_at = line.find("長いタイトル").expect("タイトルがある");
        assert!(marker_at < title_at, "{line}");
        assert!(marker_at < 10, "{line}");
    }

    #[test]
    fn formats_time() {
        assert_eq!(format_time(None), "--:--");
        assert_eq!(format_time(Some(0.0)), "00:00");
        assert_eq!(format_time(Some(75.4)), "01:15");
        assert_eq!(format_time(Some(3661.0)), "1:01:01");
        assert_eq!(format_time(Some(-3.0)), "00:00");
    }

    #[test]
    fn selection_wraps_around() {
        let mut app = App {
            results: vec![result("a"), result("b")],
            ..App::default()
        };
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn selection_is_noop_without_results() {
        let mut app = App::default();
        app.select_next();
        app.select_prev();
        assert_eq!(app.selected, 0);
        assert!(app.selected_result().is_none());
    }

    #[test]
    fn empty_results_set_error_and_stay_in_input() {
        let mut app = App::default();
        app.set_results(Vec::new(), &search_target());
        assert_eq!(app.mode, Mode::Input);
        assert!(app.error.is_some());
    }

    #[test]
    fn the_results_status_shows_the_full_title_of_the_selection() {
        // 格子ではタイトルが切り詰められるので、完全なタイトルはここでしか読めない。
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a"), result("b")],
            selected: 1,
            ..App::default()
        };
        assert_eq!(app.status_line(), "2 件  |  title b");

        app.thumbs.set_fetching(true);
        assert_eq!(app.status_line(), "2 件  |  サムネイル取得中...");

        // 0 件なら件数だけ。
        let app = App {
            mode: Mode::Results,
            ..App::default()
        };
        assert_eq!(app.status_line(), "0 件");
    }

    #[test]
    fn set_results_writes_through_to_the_current_tab() {
        let mut app = App {
            selected: 5,
            scroll: 8,
            ..App::default()
        };
        app.set_results(vec![result("a"), result("b")], &search_target());

        assert_eq!(app.tabs.state().results.len(), 2);
        assert!(app.tabs.state().loaded, "戻ったときに再検索しない");
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll, 0);
        // 0 件でも読み込み済みにする。
        app.set_results(Vec::new(), &search_target());
        assert!(app.tabs.state().loaded);
    }

    #[test]
    fn storing_and_syncing_moves_the_selection_between_tabs() {
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.selected = 1;
        app.scroll = 4;
        app.store_to_tab();

        app.tabs.next();
        app.sync_from_tab();
        assert!(app.results.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll, 0);

        app.tabs.prev();
        app.sync_from_tab();
        assert_eq!(app.results.len(), 2);
        assert_eq!(app.selected, 1);
        assert_eq!(app.scroll, 4);
        assert_eq!(app.result_ids(), ["a", "b"]);
    }

    #[test]
    fn results_switch_to_list_mode() {
        let mut app = App {
            selected: 5,
            ..App::default()
        };
        app.set_results(vec![result("a")], &search_target());
        assert_eq!(app.mode, Mode::Results);
        assert_eq!(app.selected, 0);
        assert!(app.error.is_none());
    }

    #[test]
    fn empty_search_results_keep_the_current_message() {
        let mut app = App::default();
        app.set_results(Vec::new(), &search_target());
        assert_eq!(app.error.as_deref(), Some("検索結果が0件でした"));
    }

    #[test]
    fn empty_feed_results_explain_the_login_requirement() {
        // フィードが空なのは件数の問題ではなく、ログインが効いていない疑いが濃い。
        let mut app = App::default();
        app.set_results(Vec::new(), &Target::Feed(Feed::Recommended));
        let error = app.error.expect("理由を出す");
        assert!(error.starts_with("おすすめ"), "{error}");
        assert!(error.contains("ログイン"), "{error}");
    }

    #[test]
    fn idle_and_searching_status_show_the_cookie_label() {
        let mut app = App {
            cookies: CookieState::Armed(source()),
            ..App::default()
        };
        assert!(
            app.status_line().ends_with("cookies: chrome"),
            "{}",
            app.status_line()
        );
        assert!(app.status_line().starts_with("検索したい語句"));

        app.searching = true;
        assert!(app.status_line().starts_with("検索中..."));
        assert!(app.status_line().ends_with("cookies: chrome"));

        // 停止中はその旨まで出す。
        app.searching = false;
        app.cookies = CookieState::Suspended {
            source: source(),
            reason: "読めませんでした".to_string(),
        };
        assert!(app.status_line().ends_with("cookies: chrome (停止)"));

        // 環境変数が無ければ従来どおり何も足さない。
        app.cookies = CookieState::Off;
        assert_eq!(app.status_line(), "検索したい語句を入力して Enter");
    }

    #[test]
    fn notice_shows_when_there_is_no_error_and_error_wins() {
        let mut app = App {
            cookies: CookieState::Armed(source()),
            notice: Some("cookie を読めませんでした".to_string()),
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.contains("cookies: chrome"), "{line}");
        assert!(line.ends_with("cookie を読めませんでした"), "{line}");

        app.error = Some("boom".to_string());
        assert_eq!(app.status_line(), "エラー: boom");
    }

    #[test]
    fn applies_polled_properties() {
        let mut app = App::default();
        poll(&mut app, crate::mpv::REQ_TIME_POS, Some(json!(12.0)));
        poll(&mut app, crate::mpv::REQ_DURATION, Some(json!(300.0)));
        poll(&mut app, crate::mpv::REQ_PAUSE, Some(json!(true)));
        poll(&mut app, crate::mpv::REQ_VOLUME, Some(json!(80.0)));
        assert_eq!(app.playback.time_pos, Some(12.0));
        assert_eq!(app.playback.duration, Some(300.0));
        assert_eq!(app.playback.paused, Some(true));
        assert_eq!(app.playback.volume, Some(80.0));

        poll(&mut app, crate::mpv::REQ_TIME_POS, None);
        assert_eq!(app.playback.time_pos, None);
    }

    #[test]
    fn speed_is_applied_from_the_poll_and_rounded() {
        let mut app = App::default();
        assert_eq!(app.speed, Speed::NORMAL);

        // mpv ウィンドウ側の × 1.1 は 0.1 刻みに乗らない値で返る。
        poll(&mut app, crate::mpv::REQ_SPEED, Some(json!(2.75)));
        assert_eq!(app.speed, Speed::from_tenths(28).expect("2.8x"));

        // 値が取れないときは触らない。
        poll(&mut app, crate::mpv::REQ_SPEED, None);
        assert_eq!(app.speed, Speed::from_tenths(28).expect("2.8x"));
    }

    #[test]
    fn a_speed_outside_the_range_is_sent_back_so_the_display_matches_the_playback() {
        // mpv ウィンドウ側で 8.0 にされた状態。表示を 4.0x にするだけでは実再生が 8 倍のまま。
        let mut app = App::default();
        assert_eq!(
            app.apply_property(crate::mpv::REQ_SPEED, Some(json!(8.0))),
            Some(Speed::MAX.command())
        );
        assert_eq!(app.speed, Speed::MAX);

        // 送り返した値が mpv から返ってくれば、それ以上は送らない。
        poll(&mut app, crate::mpv::REQ_SPEED, Some(json!(4.0)));
        assert_eq!(app.speed, Speed::MAX);
    }

    #[test]
    fn a_stale_speed_poll_within_the_hold_does_not_roll_back_the_local_value() {
        // 1.2x で問い合わせ → 応答が届く前に ] を処理 → 1.2 が後から届く、という順番。
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_speed_sent(Speed::from_tenths(13).expect("1.3x"), t0);

        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(12).expect("1.2x")),
                t0 + Duration::from_millis(500)
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(13).expect("1.3x"));

        // mpv が確定値を返したら保持は終わる。
        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(13).expect("1.3x")),
                t0 + Duration::from_millis(900),
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(13).expect("1.3x"));
        assert!(app.speed_sent_at.is_none());
    }

    #[test]
    fn a_speed_changed_in_the_mpv_window_wins_after_the_hold_expires() {
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_speed_sent(Speed::from_tenths(13).expect("1.3x"), t0);

        assert_eq!(
            app.reconcile_speed(
                polled(Speed::from_tenths(20).expect("2.0x")),
                t0 + SPEED_HOLD
            ),
            None
        );
        assert_eq!(app.speed, Speed::from_tenths(20).expect("2.0x"));
        assert!(app.speed_sent_at.is_none());
    }

    #[test]
    fn the_status_line_shows_the_speed_after_the_volume() {
        let mut app = App {
            mode: Mode::Playing,
            speed: Speed::from_tenths(15).expect("1.5x"),
            playback: Playback {
                title: "song".to_string(),
                paused: Some(false),
                volume: Some(70.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.contains("vol 70  1.5x  ["), "{line}");

        // 等速でも出す。戻ったことが分かるため。
        app.speed = Speed::NORMAL;
        assert!(
            app.status_line().contains("vol 70  1.0x  ["),
            "{}",
            app.status_line()
        );
    }

    #[test]
    fn pause_error_response_is_not_read_as_playing() {
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        poll(&mut app, crate::mpv::REQ_PAUSE, Some(json!(true)));
        assert!(app.status_line().starts_with("PAUSED"));
        poll(&mut app, crate::mpv::REQ_PAUSE, None);
        assert_eq!(app.playback.paused, None);
        assert!(!app.status_line().starts_with("PLAYING"));
    }

    #[test]
    fn the_notice_stays_visible_until_an_error_takes_the_line() {
        // 設定を読み替えた旨は、気づけるようステータス行に出し続ける。
        let mut app = App {
            mode: Mode::Results,
            results: vec![result("a")],
            notice: Some("TUITUBE_FPS_LIMIT=3O を数値として読めません".to_string()),
            ..App::default()
        };
        // 件数と選択中のタイトルの後ろに続く。
        assert_eq!(
            app.status_line(),
            "1 件  |  title a  |  TUITUBE_FPS_LIMIT=3O を数値として読めません"
        );

        // 検索中でも消えない。
        app.searching = true;
        assert!(app.status_line().starts_with("検索中..."));
        assert!(app.status_line().ends_with("読めません"));

        // 再生中も再生状況の後ろに続く。
        app.searching = false;
        app.mode = Mode::Playing;
        assert!(app.status_line().starts_with("状態不明"));
        assert!(app.status_line().ends_with("読めません"));

        // エラーが出ている行には足さない。
        app.error = Some("boom".to_string());
        assert!(app.status_line().ends_with("エラー: boom"));
    }

    #[test]
    fn playback_keeps_the_url_so_it_can_be_copied() {
        assert!(Playback::default().url.is_empty());
        let playback = Playback {
            url: "https://www.youtube.com/watch?v=abc".to_string(),
            ..Playback::default()
        };
        assert_eq!(playback.url, "https://www.youtube.com/watch?v=abc");
    }

    #[test]
    fn a_temporary_notice_disappears_once_its_time_is_up() {
        let t0 = Instant::now();
        let mut app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        app.set_temporary_notice("URL をコピーしました".to_string(), t0);
        assert!(app.status_line().ends_with("URL をコピーしました"));

        app.expire_notice(t0 + NOTICE_TTL - Duration::from_millis(1));
        assert!(app.notice.is_some(), "期限前は消さない");

        app.expire_notice(t0 + NOTICE_TTL);
        assert!(app.notice.is_none());
        assert!(app.notice_until.is_none());
    }

    #[test]
    fn a_notice_without_a_deadline_stays_until_it_is_replaced() {
        // 設定の読み替えや cookie の知らせは、気づくまで出し続ける。
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_notice(Some(
            "TUITUBE_FPS_LIMIT=3O を数値として読めません".to_string(),
        ));
        app.expire_notice(t0 + NOTICE_TTL * 10);
        assert!(app.notice.is_some());

        // 期限つきの知らせを上書きしたら、その期限も持ち越さない。
        app.set_temporary_notice("URL をコピーしました".to_string(), t0);
        app.set_notice(Some("cookie を読めませんでした".to_string()));
        assert!(app.notice_until.is_none());
        app.expire_notice(t0 + NOTICE_TTL * 10);
        assert_eq!(app.notice.as_deref(), Some("cookie を読めませんでした"));

        app.set_notice(None);
        assert!(app.notice.is_none());
    }

    #[test]
    fn an_operation_error_outlives_a_successful_poll_but_not_its_deadline() {
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_temporary_error("pbcopy が見つかりません".to_string(), t0);

        app.clear_polled_error(t0 + NOTICE_TTL - Duration::from_millis(1));
        assert!(app.error.is_some(), "ポーリングの成功では消さない");

        app.clear_polled_error(t0 + NOTICE_TTL);
        assert!(app.error.is_none());
        assert!(app.error_until.is_none());
    }

    #[test]
    fn a_polling_error_is_cleared_by_the_next_success() {
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_error(Some("パイプが閉じました".to_string()));
        assert!(app.error_until.is_none(), "期限つきの残りを持ち越さない");

        app.clear_polled_error(t0);
        assert!(app.error.is_none());
    }

    #[test]
    fn an_operation_error_is_dropped_by_the_tick_even_without_a_player() {
        let t0 = Instant::now();
        let mut app = App::default();
        app.set_temporary_error("URL をコピーできませんでした".to_string(), t0);

        app.expire_error(t0 + NOTICE_TTL - Duration::from_millis(1));
        assert!(app.error.is_some());

        app.expire_error(t0 + NOTICE_TTL);
        assert!(app.error.is_none());
    }

    #[test]
    fn current_vo_is_applied_from_the_poll() {
        let mut app = App::default();
        poll(
            &mut app,
            crate::mpv::REQ_CURRENT_VO,
            Some(json!("gpu-next")),
        );
        assert_eq!(app.playback.current_vo.as_deref(), Some("gpu-next"));
        // kitty VO では取れないプロパティもあるので、値が消えることも通常の経過。
        poll(&mut app, crate::mpv::REQ_CURRENT_VO, None);
        assert_eq!(app.playback.current_vo, None);
    }

    #[test]
    fn display_label_marks_the_transition_until_mpv_confirms() {
        let mut app = App {
            display: DisplayMode::Window,
            playback: Playback {
                current_vo: Some("kitty".to_string()),
                ..Playback::default()
            },
            ..App::default()
        };
        assert_eq!(app.display_label(), "[別ウィンドウ (切替中)]");

        app.playback.current_vo = Some("gpu-next".to_string());
        assert_eq!(app.display_label(), "[別ウィンドウ]");

        app.display = DisplayMode::Embedded;
        app.playback.current_vo = Some("kitty".to_string());
        assert_eq!(app.display_label(), "[埋め込み 15fps medium]");

        // 制限なしなら fps は出さない。
        app.settings.fps_cap = None;
        assert_eq!(app.display_label(), "[埋め込み medium]");

        // 値が来ていない間は切替中と決めつけない。
        app.playback.current_vo = None;
        assert_eq!(app.display_label(), "[埋め込み medium]");
    }

    #[test]
    fn the_status_line_ends_with_the_display_label_while_playing() {
        let app = App {
            mode: Mode::Playing,
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        assert!(
            app.status_line().ends_with(&app.display_label()),
            "{}",
            app.status_line()
        );
    }

    #[test]
    fn default_app_takes_the_display_mode_from_settings() {
        assert_eq!(App::default().display, Settings::default().display.mode);
        assert_eq!(App::default().settings, Settings::default());
    }

    #[test]
    fn status_line_prefers_error() {
        let app = App {
            searching: true,
            error: Some("boom".to_string()),
            ..App::default()
        };
        assert_eq!(app.status_line(), "エラー: boom");
    }

    #[test]
    fn playing_status_stays_visible_with_error() {
        let app = App {
            mode: Mode::Playing,
            error: Some("boom".to_string()),
            playback: Playback {
                title: "song".to_string(),
                paused: Some(true),
                time_pos: Some(30.0),
                duration: Some(60.0),
                volume: Some(70.0),
                ..Playback::default()
            },
            ..App::default()
        };
        let line = app.status_line();
        assert!(line.starts_with("PAUSED"), "{line}");
        assert!(line.contains("song  00:30 / 01:00  vol 70"), "{line}");
        assert!(line.ends_with("エラー: boom"));
    }

    #[test]
    fn optimistic_seek_survives_a_stale_poll_within_the_hold() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        playback.begin_seek(100.0, t0);
        assert_eq!(playback.time_pos, Some(100.0));
        assert!(playback.pending_seek.is_some());

        // シーク直後のポーリングは旧位置を返しうる。
        playback.reconcile_time_pos(Some(11.0), t0 + Duration::from_millis(500));
        assert_eq!(playback.time_pos, Some(100.0));
        playback.reconcile_time_pos(None, t0 + Duration::from_millis(800));
        assert_eq!(playback.time_pos, Some(100.0));

        playback.reconcile_time_pos(Some(101.5), t0 + Duration::from_millis(900));
        assert_eq!(playback.time_pos, Some(101.5));
        assert_eq!(playback.pending_seek, None);
    }

    #[test]
    fn a_stale_poll_wins_after_the_hold_expires() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        playback.begin_seek(100.0, t0);
        playback.reconcile_time_pos(Some(11.0), t0 + SEEK_HOLD);

        assert_eq!(playback.time_pos, Some(11.0));
        assert_eq!(playback.pending_seek, None);
    }

    #[test]
    fn relative_seeks_stack_on_the_pending_target() {
        let t0 = Instant::now();
        let mut playback = Playback {
            time_pos: Some(10.0),
            ..Playback::default()
        };
        assert_eq!(playback.seek_base(), Some(10.0));
        playback.begin_seek(15.0, t0);
        assert_eq!(playback.seek_base(), Some(15.0));

        let t1 = t0 + Duration::from_millis(100);
        playback.begin_seek(20.0, t1);
        assert_eq!(playback.time_pos, Some(20.0));
        let pending = playback.pending_seek.expect("先行更新が入っている");
        assert_eq!(pending.target, 20.0);
        assert_eq!(pending.sent_at, t1);
    }

    #[test]
    fn playing_status_without_pause_data_is_marked_unknown() {
        let app = App {
            mode: Mode::Playing,
            ..App::default()
        };
        assert!(app.status_line().starts_with("状態不明"));
    }

    /// 1 項目だけ動かした結果。他の行に触れていないことも見られるよう丸ごと返す。
    fn adjusted(item: SettingsItem, delta: i32, settings: &Settings) -> Settings {
        let mut next = settings.clone();
        item.adjust(&mut next, delta);
        next
    }

    #[test]
    fn the_settings_screen_lists_every_editable_item_in_order() {
        let labels: Vec<&str> = SETTINGS_ITEMS.iter().map(|item| item.label()).collect();
        assert_eq!(
            labels,
            [
                "display.mode",
                "display.quality",
                "fps_cap",
                "subtitles.enabled",
                "search.layout",
                "search.limit",
                "search.timeout_secs",
                "thumbnails.enabled",
                "thumbnails.max_cached",
                "thumbnails.timeout_secs",
            ]
        );
    }

    #[test]
    fn each_settings_row_shows_the_value_the_config_file_holds() {
        // 行の値は設定ファイルの表記と揃える。画面で見た値をそのまま探せるため。
        assert_eq!(
            App::default().settings_rows(),
            [
                "display.mode: embedded",
                "display.quality: medium",
                "fps_cap: 15",
                "subtitles.enabled: true",
                "search.layout: grid",
                "search.limit: 10",
                "search.timeout_secs: 30",
                "thumbnails.enabled: true",
                "thumbnails.max_cached: 500",
                "thumbnails.timeout_secs: 10",
            ]
        );
    }

    #[test]
    fn an_unlimited_fps_cap_is_shown_as_zero() {
        let mut app = App::default();
        app.settings.fps_cap = None;
        assert_eq!(app.settings_rows()[2], "fps_cap: 0 (無制限)");
    }

    #[test]
    fn a_row_the_environment_holds_says_that_it_is_not_saved() {
        // 環境変数が効いている間は保存でファイルへ書かないので、画面で断っておく。
        let app = App {
            env_overridden: EnvOverridden {
                fps_cap: Some(FpsCap::new(30)),
                ..EnvOverridden::default()
            },
            ..App::default()
        };
        let row = &app.settings_rows()[2];
        assert!(row.starts_with("fps_cap: "), "{row}");
        assert!(row.contains(FPS_LIMIT_VAR), "{row}");
        assert!(row.contains("保存しません"), "{row}");

        // 上書きされていない行には何も足さない。
        assert_eq!(app.settings_rows()[5], "search.limit: 10");
        assert_eq!(App::default().settings_rows()[2], "fps_cap: 15");
    }

    #[test]
    fn the_choice_rows_send_forward_on_the_right_and_back_on_the_left() {
        let settings = Settings::default();
        assert_eq!(
            adjusted(SettingsItem::DisplayMode, 1, &settings)
                .display
                .mode,
            DisplayMode::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayMode, -1, &settings)
                .display
                .mode,
            DisplayMode::default().prev()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayQuality, 1, &settings)
                .display
                .quality,
            Quality::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::DisplayQuality, -1, &settings)
                .display
                .quality,
            Quality::default().prev()
        );
        assert_eq!(
            adjusted(SettingsItem::SearchLayout, 1, &settings)
                .search
                .layout,
            LayoutMode::default().next()
        );
        assert_eq!(
            adjusted(SettingsItem::SearchLayout, -1, &settings)
                .search
                .layout,
            LayoutMode::default().prev()
        );
    }

    #[test]
    fn a_choice_row_comes_back_to_where_it_started() {
        // ← が「戻る」でないと、4 値ある quality は選び直しに 3 回かかる。
        let settings = Settings::default();
        let mut next = settings.clone();
        SettingsItem::DisplayQuality.adjust(&mut next, 1);
        SettingsItem::DisplayQuality.adjust(&mut next, -1);
        assert_eq!(next.display.quality, settings.display.quality);

        let mut back = settings.clone();
        SettingsItem::DisplayQuality.adjust(&mut back, -1);
        assert_eq!(back.display.quality, Quality::Low, "medium の 1 つ前");
    }

    #[test]
    fn the_flag_rows_toggle_whichever_way_they_are_pushed() {
        let mut settings = Settings::default();
        for delta in [-1, 1] {
            SettingsItem::SubtitlesEnabled.adjust(&mut settings, delta);
            assert!(!settings.subtitles.enabled, "{delta}");
            SettingsItem::SubtitlesEnabled.adjust(&mut settings, delta);
            assert!(settings.subtitles.enabled, "{delta}");

            SettingsItem::ThumbnailsEnabled.adjust(&mut settings, delta);
            assert!(!settings.thumbnails.enabled, "{delta}");
            SettingsItem::ThumbnailsEnabled.adjust(&mut settings, delta);
            assert!(settings.thumbnails.enabled, "{delta}");
        }
    }

    #[test]
    fn the_fps_cap_steps_by_five_and_stops_at_both_ends() {
        let mut settings = Settings::default();
        SettingsItem::FpsCap.adjust(&mut settings, 1);
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(20));
        SettingsItem::FpsCap.adjust(&mut settings, -1);
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(15));

        // 0 まで下げると制限なし。そこから下は動かない (ラップしない)。
        for _ in 0..10 {
            SettingsItem::FpsCap.adjust(&mut settings, -1);
        }
        assert_eq!(settings.fps_cap, None);

        for _ in 0..40 {
            SettingsItem::FpsCap.adjust(&mut settings, 1);
        }
        assert_eq!(settings.fps_cap.map(FpsCap::get), Some(MAX_FPS_CAP));
    }

    #[test]
    fn the_search_limit_steps_by_one_and_stays_inside_its_range() {
        let mut settings = Settings::default();
        SettingsItem::SearchLimit.adjust(&mut settings, 1);
        assert_eq!(settings.search.limit, 11);

        for _ in 0..20 {
            SettingsItem::SearchLimit.adjust(&mut settings, -1);
        }
        assert_eq!(settings.search.limit, MIN_SEARCH_LIMIT);

        for _ in 0..(MAX_SEARCH_LIMIT + 10) {
            SettingsItem::SearchLimit.adjust(&mut settings, 1);
        }
        assert_eq!(settings.search.limit, MAX_SEARCH_LIMIT);
    }

    #[test]
    fn the_search_timeout_steps_by_five_and_stays_inside_its_range() {
        let mut settings = Settings::default();
        let secs = |settings: &Settings| settings.search.timeout.as_secs();
        assert_eq!(secs(&settings), 30, "既定は 30 秒のまま");

        SettingsItem::SearchTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 35);
        SettingsItem::SearchTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 30);

        for _ in 0..20 {
            SettingsItem::SearchTimeoutSecs.adjust(&mut settings, -1);
        }
        assert_eq!(secs(&settings), MIN_SEARCH_TIMEOUT_SECS, "下限で止まる");

        for _ in 0..100 {
            SettingsItem::SearchTimeoutSecs.adjust(&mut settings, 1);
        }
        assert_eq!(secs(&settings), MAX_SEARCH_TIMEOUT_SECS, "上限で止まる");
    }

    #[test]
    fn the_search_timeout_row_shows_the_seconds() {
        let settings = Settings::default();
        assert_eq!(
            SettingsItem::SearchTimeoutSecs.row(&settings),
            "search.timeout_secs: 30"
        );
    }

    #[test]
    fn the_thumbnail_numbers_step_by_their_own_width() {
        let mut settings = Settings::default();
        SettingsItem::ThumbnailsMaxCached.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.max_cached, 550);
        for _ in 0..20 {
            SettingsItem::ThumbnailsMaxCached.adjust(&mut settings, -1);
        }
        assert_eq!(settings.thumbnails.max_cached, 0, "0 枚で止まる");

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(15));
        for _ in 0..10 {
            SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        }
        assert_eq!(
            settings.thumbnails.timeout,
            Duration::from_secs(1),
            "0 秒では 1 枚も取れない"
        );
        for _ in 0..40 {
            SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        }
        assert_eq!(
            settings.thumbnails.timeout,
            Duration::from_secs(MAX_THUMB_TIMEOUT_SECS)
        );
    }

    #[test]
    fn the_timeout_keeps_its_step_after_hitting_the_lower_bound() {
        // 下限 1 は刻みに乗っていない。ここで基準がずれると既定の 10 へ戻せなくなる。
        let mut settings = Settings::default();
        let secs = |settings: &Settings| settings.thumbnails.timeout.as_secs();

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 5);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(secs(&settings), 1, "0 秒では取れないので 1 で止まる");

        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 5, "刻みの目盛りへ戻る");
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(secs(&settings), 10, "既定値に戻せる");
    }

    #[test]
    fn a_value_off_the_step_is_pulled_back_onto_it() {
        // 設定ファイルに手で書いた端数から始めても、目盛りの上を動く。
        let mut settings = Settings::default();
        settings.thumbnails.timeout = Duration::from_secs(12);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, -1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(10));

        settings.thumbnails.timeout = Duration::from_secs(12);
        SettingsItem::ThumbnailsTimeoutSecs.adjust(&mut settings, 1);
        assert_eq!(settings.thumbnails.timeout, Duration::from_secs(15));
    }

    #[test]
    fn the_search_results_do_not_close_the_settings_screen() {
        // 検索中に設定を開いても、結果が届いたところで画面が消えない。
        let mut app = App {
            mode: Mode::Settings,
            settings_return: Mode::Input,
            ..App::default()
        };
        app.enter_search_mode(Mode::Results);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(app.settings_return, Mode::Results, "閉じたら結果へ戻す");

        // 設定画面を開いていなければ、そのままモードを動かす。
        let mut app = App::default();
        app.enter_search_mode(Mode::Results);
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn an_out_of_range_selection_still_points_at_a_row() {
        let mut app = App::default();
        assert_eq!(app.settings_selected, 0);
        assert_eq!(app.settings_item(), SETTINGS_ITEMS[0]);

        app.settings_selected = 99;
        assert_eq!(
            app.settings_item(),
            SETTINGS_ITEMS[SETTINGS_ITEMS.len() - 1]
        );
    }

    #[test]
    fn only_the_number_rows_take_a_typed_value() {
        let numeric: Vec<&str> = SETTINGS_ITEMS
            .iter()
            .filter(|item| item.is_numeric())
            .map(|item| item.label())
            .collect();
        assert_eq!(
            numeric,
            [
                "fps_cap",
                "search.limit",
                "search.timeout_secs",
                "thumbnails.max_cached",
                "thumbnails.timeout_secs",
            ]
        );
    }

    #[test]
    fn every_number_row_actually_takes_the_value_it_advertises() {
        // is_numeric() に項目を足して apply_numeric() の腕を足し忘れると、
        // 数字は打てるのに Enter が効かない行ができる。
        let settings = Settings::default();
        for item in SETTINGS_ITEMS {
            let mut next = settings.clone();
            assert_eq!(
                item.apply_numeric(&mut next, "1"),
                item.is_numeric(),
                "{}",
                item.label()
            );
            assert_eq!(item.max_digits() > 0, item.is_numeric(), "{}", item.label());
        }
    }

    #[test]
    fn the_digit_limit_still_lets_an_out_of_range_number_be_typed() {
        // 端へ寄せる入力 (search.limit の 9999 等) は打てる長さに収める。
        assert_eq!(SettingsItem::FpsCap.max_digits(), 3);
        assert_eq!(SettingsItem::SearchLimit.max_digits(), 4);
        assert_eq!(SettingsItem::SearchTimeoutSecs.max_digits(), 3);
        assert_eq!(SettingsItem::ThumbnailsTimeoutSecs.max_digits(), 3);
        assert_eq!(SettingsItem::ThumbnailsMaxCached.max_digits(), 20);
    }

    /// 打ち込んだ値を 1 項目だけ確定した結果。
    fn typed(item: SettingsItem, raw: &str, settings: &Settings) -> Settings {
        let mut next = settings.clone();
        item.apply_numeric(&mut next, raw);
        next
    }

    #[test]
    fn a_typed_number_lands_inside_the_range_the_arrows_use() {
        let settings = Settings::default();
        let limit = |raw| {
            typed(SettingsItem::SearchLimit, raw, &settings)
                .search
                .limit
        };
        assert_eq!(limit("250"), 250);
        // 範囲の外は ←→ と同じ端で止める。
        assert_eq!(limit("0"), MIN_SEARCH_LIMIT);
        assert_eq!(limit("5000"), MAX_SEARCH_LIMIT);

        let fps = |raw| typed(SettingsItem::FpsCap, raw, &settings).fps_cap;
        assert_eq!(fps("24"), FpsCap::new(24));
        assert_eq!(fps("999"), FpsCap::new(MAX_FPS_CAP));
        // 0 は「制限なし」。
        assert_eq!(fps("0"), None);

        let search_timeout = |raw| {
            typed(SettingsItem::SearchTimeoutSecs, raw, &settings)
                .search
                .timeout
        };
        assert_eq!(search_timeout("120"), Duration::from_secs(120));
        assert_eq!(
            search_timeout("0"),
            Duration::from_secs(MIN_SEARCH_TIMEOUT_SECS)
        );
        assert_eq!(
            search_timeout("9999"),
            Duration::from_secs(MAX_SEARCH_TIMEOUT_SECS)
        );

        let cached = |raw| {
            typed(SettingsItem::ThumbnailsMaxCached, raw, &settings)
                .thumbnails
                .max_cached
        };
        assert_eq!(cached("1200"), 1200);
        assert_eq!(cached("0"), 0);

        let timeout = |raw| {
            typed(SettingsItem::ThumbnailsTimeoutSecs, raw, &settings)
                .thumbnails
                .timeout
        };
        assert_eq!(timeout("30"), Duration::from_secs(30));
        assert_eq!(timeout("0"), Duration::from_secs(MIN_THUMB_TIMEOUT_SECS));
        assert_eq!(timeout("999"), Duration::from_secs(MAX_THUMB_TIMEOUT_SECS));
    }

    #[test]
    fn a_typed_value_that_is_not_a_number_leaves_the_setting_alone() {
        let settings = Settings::default();
        // 空入力・符号・小数・桁あふれは読めないので捨てる。
        for raw in ["", " ", "abc", "-5", "1.5", "12 ", "99999999999999999999"] {
            let mut next = settings.clone();
            assert!(
                !SettingsItem::SearchLimit.apply_numeric(&mut next, raw),
                "{raw:?} は受け付けない"
            );
            assert_eq!(next, settings, "{raw:?}");
        }
    }

    #[test]
    fn typing_a_number_into_a_choice_row_changes_nothing() {
        let settings = Settings::default();
        for item in SETTINGS_ITEMS.iter().filter(|item| !item.is_numeric()) {
            let mut next = settings.clone();
            assert!(!item.apply_numeric(&mut next, "1"), "{}", item.label());
            assert_eq!(next, settings, "{}", item.label());
        }
    }

    #[test]
    fn the_settings_screen_starts_without_a_typed_value() {
        assert_eq!(App::default().settings_edit, None);
    }

    #[test]
    fn the_settings_status_line_shows_the_position_and_keeps_the_notice() {
        let t0 = Instant::now();
        let mut app = App {
            mode: Mode::Settings,
            ..App::default()
        };
        assert_eq!(
            app.status_line(),
            format!("設定 1/{}", SETTINGS_ITEMS.len())
        );

        app.settings_selected = 2;
        assert!(
            app.status_line().starts_with("設定 3/"),
            "{}",
            app.status_line()
        );

        app.set_temporary_notice("保存しました".to_string(), t0);
        assert!(app.status_line().ends_with("保存しました"));

        // 保存に失敗した理由はエラーとして出す。
        app.set_temporary_error("保存できません".to_string(), t0);
        assert_eq!(app.status_line(), "エラー: 保存できません");
    }
}
