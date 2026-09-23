use crate::category::{TabState, Tabs};
use crate::comments::{Comment, Comments};
use crate::cookies::{ChannelTab, CookieState, Target};
use crate::display::DisplayMode;
use crate::engagement::EngagementCache;
use crate::hidden::Hidden;
use crate::mpv::MpvCommand;
use crate::query::QueryEditor;
use crate::resume::Resume;
use crate::rgb::RgbImage;
use crate::screen::download::{self as download_screen, DownloadForm};
use crate::screen::playlists::{self as playlists_screen, PlaylistsView};
use crate::screen::settings::{self as settings_screen, SettingsScreen};
use crate::search::{ChannelRef, PlaylistEntry, SearchReport, SearchResult};
use crate::seekbar::SeekBarState;
use crate::settings::{EnvOverridden, MAX_SEARCH_LIMIT, Settings};
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
        /// yt-dlp へ実際に要求した件数。TabState::requested_limit へそのまま渡す。
        requested_limit: usize,
    },
    /// プレイリスト一覧が返った。行が動画でないので SearchDone とは別枠。
    /// nonce は検索と同じものを使う(打ち切りの仕組みを共用している)。
    PlaylistsReady {
        nonce: u64,
        entries: Result<Vec<PlaylistEntry>, String>,
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
    /// 選択中の 1 本からチャンネルを引き終わった。Ok(None) は channel_id を
    /// 持たない動画。video_id で今の行のものかを見分ける。
    ChannelLookupDone {
        nonce: u64,
        video_id: String,
        result: Result<Option<ChannelRef>, String>,
    },
    /// チャンネル登録・いいねが終わった。Ok は画面に出す文言。
    OauthDone {
        nonce: u64,
        result: Result<String, String>,
    },
    /// いいね済み/登録済みの問い合わせが返った。取れなかった側は
    /// liked_videos = None / asked_channels 空で、その分の控えを触らない。
    EngagementReady {
        nonce: u64,
        liked_videos: Option<Vec<String>>,
        asked_channels: Vec<String>,
        subscribed_channels: Vec<String>,
    },
    /// ダウンロードが終わった。Ok は set_temporary_notice、Err は set_error へ渡す文言。
    DownloadDone {
        nonce: u64,
        notice: Result<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Results,
    Playing,
    Settings,
    Channel,
    Download,
    /// プレイリストの一覧。
    Playlists,
    /// 1 つのプレイリストの中の動画一覧。
    Playlist,
}

/// 一覧の出どころ (チャンネルのタブ位置, カテゴリタブの位置, 開いているプレイリスト)。
pub type ViewKey = (Option<usize>, usize, Option<String>);

/// チャンネル閲覧中の状態。タブごとの一覧はカテゴリタブと同じ TabState に持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelView {
    pub channel_id: String,
    pub channel_title: String,
    pub tab: ChannelTab,
    /// ChannelTab::ALL と同じ並び。
    pub states: [TabState; 3],
}

impl ChannelView {
    pub fn new(channel_id: String, channel_title: String) -> Self {
        Self {
            channel_id,
            channel_title,
            tab: ChannelTab::Videos,
            states: Default::default(),
        }
    }

    pub fn state(&self) -> &TabState {
        &self.states[self.tab.index()]
    }

    pub fn state_mut(&mut self) -> &mut TabState {
        &mut self.states[self.tab.index()]
    }

    pub fn next_tab(&mut self) {
        let next = (self.tab.index() + 1) % ChannelTab::ALL.len();
        self.tab = ChannelTab::ALL[next];
    }

    pub fn prev_tab(&mut self) {
        let prev = (self.tab.index() + ChannelTab::ALL.len() - 1) % ChannelTab::ALL.len();
        self.tab = ChannelTab::ALL[prev];
    }

    /// 位置を指して移る。範囲外は黙って無視し、選べたかどうかを返す。
    pub fn select_tab(&mut self, index: usize) -> bool {
        match ChannelTab::from_index(index) {
            Some(tab) => {
                self.tab = tab;
                true
            }
            None => false,
        }
    }

    pub fn labels() -> [&'static str; 3] {
        ChannelTab::ALL.map(ChannelTab::label)
    }

    pub fn target(&self) -> Target {
        Target::Channel {
            id: self.channel_id.clone(),
            tab: self.tab,
        }
    }
}

/// 1 つのプレイリストを開いている間の状態。中身は普通の動画一覧なので、
/// チャンネルのタブと同じ TabState で持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistView {
    pub playlist_id: String,
    pub playlist_title: String,
    pub state: TabState,
}

impl PlaylistView {
    pub fn new(playlist_id: String, playlist_title: String) -> Self {
        Self {
            playlist_id,
            playlist_title,
            state: TabState::default(),
        }
    }

    pub fn target(&self) -> Target {
        Target::Playlist(self.playlist_id.clone())
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
    /// 再生中の動画 ID。再開位置を記憶するときの検索キー。
    pub id: String,
    /// 投稿チャンネルの ID。チャンネルタブ経由の行には無いので None もある。
    pub channel_id: Option<String>,
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
    /// Mode::Playing を抜けて検索側にいる間 true。session.player/video/playback は
    /// 生かしたまま、embedded/text 表示中だけ隅にミニプレイヤーを出す。
    pub background: bool,
    /// 直近の terminal.draw() が描いた画面。マウスの当たり判定はこれで割り付ける。
    pub screen: Rect,
    /// results を入れ替えた回数。
    pub results_generation: u64,
    /// 直近の draw が描いた results_generation。
    pub drawn_generation: u64,
    pub seek_bar: SeekBarState,
    pub should_quit: bool,
    /// 終了確認 (y/N) を出している間 true。立っている間は他のキー/マウスを無視する。
    pub confirm_quit: bool,
    /// 擬似カテゴリタブ。results / selected / scroll はここの写し。
    pub tabs: Tabs,
    /// チャンネル閲覧中だけ入る。入っている間は一覧の参照先がこちらへ移る。
    pub channel: Option<ChannelView>,
    /// プレイリストの一覧を開いている間だけ入る。
    pub playlists: Option<PlaylistsView>,
    /// 1 つのプレイリストを開いている間だけ入る。入っている間は一覧の参照先がこちらへ移る。
    pub playlist: Option<PlaylistView>,
    pub thumbs: Thumbs,
    /// 再生中の動画のコメント。取得状態と表示の on/off。
    pub comments: Comments,
    /// 格子の先頭表示位置 (項目インデックス)。描画のたびに列数で丸める。
    pub scroll: usize,
    /// 設定画面の状態。
    pub settings_screen: SettingsScreen,
    /// 環境変数が上書きしている項目。画面の断りと、保存時の書き戻しに使う。
    pub env_overridden: EnvOverridden,
    /// ローカル非表示リスト。一覧へ入れる前にここで外す。
    pub hidden: Hidden,
    /// 動画ごとの再生位置の記憶。次に選んだときの再開位置をここから引く。
    pub resume: Resume,
    /// いいね済み/チャンネル登録済みの控え。印の判定と再確認の要否をここから引く。
    pub engagement: EngagementCache,
    /// ダウンロード画面の状態。
    pub download: DownloadForm,
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
            settings_screen: SettingsScreen {
                backup: settings.clone(),
                ..SettingsScreen::default()
            },
            settings,
            video: None,
            background: false,
            screen: Rect::default(),
            results_generation: 0,
            drawn_generation: 0,
            seek_bar: SeekBarState::default(),
            should_quit: false,
            confirm_quit: false,
            tabs: Tabs::default(),
            channel: None,
            playlists: None,
            playlist: None,
            thumbs: Thumbs::default(),
            comments: Comments::default(),
            scroll: 0,
            env_overridden: EnvOverridden::default(),
            hidden: Hidden::default(),
            resume: Resume::default(),
            engagement: EngagementCache::default(),
            download: DownloadForm::default(),
        }
    }
}

/// 非表示になった行を落とし、選択と表示位置を残りへ合わせる。
fn retain_visible(state: &mut TabState, hidden: &Hidden) {
    let before = state.results.len();
    let selected_id = state
        .results
        .get(state.selected)
        .map(|result| result.id.clone());
    state.results.retain(|result| !hidden.hides(result));
    if state.results.len() == before {
        return;
    }
    // 選んでいた動画が残っていればそこへ。消えていれば同じ位置の次の行へ送る。
    // 添字だけで丸めると、手前の行が消えたときに別の動画へ移る。
    let last = state.results.len().saturating_sub(1);
    state.selected = selected_id
        .and_then(|id| state.results.iter().position(|result| result.id == id))
        .unwrap_or_else(|| state.selected.min(last));
    // 選択より後ろから描き始めると、残った行を飛び越したまま画面に出る。
    state.scroll = state.scroll.min(state.selected);
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

    /// 検索側のタブではなく、チャンネル/プレイリストの一覧を見ているか。
    pub fn view_is_nested(&self) -> bool {
        self.channel.is_some() || self.playlist.is_some()
    }

    /// 結果の行き先になるタブの状態。チャンネル閲覧中はその現在タブ、
    /// プレイリストを開いている間はその一覧。
    /// プレイリストの中からもチャンネルへ移れるので、両方入っているときは後から開いた
    /// チャンネル側を見る。チャンネルを閉じればプレイリストへ戻る。
    pub fn view_state(&self) -> &TabState {
        match (&self.channel, &self.playlist) {
            (Some(channel), _) => channel.state(),
            (None, Some(playlist)) => &playlist.state,
            (None, None) => self.tabs.state(),
        }
    }

    pub fn view_state_mut(&mut self) -> &mut TabState {
        match (&mut self.channel, &mut self.playlist) {
            (Some(channel), _) => channel.state_mut(),
            (None, Some(playlist)) => &mut playlist.state,
            (None, None) => self.tabs.state_mut(),
        }
    }

    /// 今の画面が見ている一覧。チャンネル/プレイリストを開いている間はそちら、
    /// それ以外は検索結果。
    pub fn view_results(&self) -> &[SearchResult] {
        match (&self.channel, &self.playlist) {
            (Some(channel), _) => &channel.state().results,
            (None, Some(playlist)) => &playlist.state.results,
            (None, None) => &self.results,
        }
    }

    pub fn view_selected(&self) -> usize {
        match (&self.channel, &self.playlist) {
            (Some(channel), _) => channel.state().selected,
            (None, Some(playlist)) => playlist.state.selected,
            (None, None) => self.selected,
        }
    }

    pub fn view_scroll(&self) -> usize {
        match (&self.channel, &self.playlist) {
            (Some(channel), _) => channel.state().scroll,
            (None, Some(playlist)) => playlist.state.scroll,
            (None, None) => self.scroll,
        }
    }

    pub fn set_view_selected(&mut self, index: usize) {
        match (&mut self.channel, &mut self.playlist) {
            (Some(channel), _) => channel.state_mut().selected = index,
            (None, Some(playlist)) => playlist.state.selected = index,
            (None, None) => self.selected = index,
        }
    }

    pub fn set_view_scroll(&mut self, scroll: usize) {
        match (&mut self.channel, &mut self.playlist) {
            (Some(channel), _) => channel.state_mut().scroll = scroll,
            (None, Some(playlist)) => playlist.state.scroll = scroll,
            (None, None) => self.scroll = scroll,
        }
    }

    pub fn view_selected_result(&self) -> Option<&SearchResult> {
        self.view_results().get(self.view_selected())
    }

    /// 今のタブで「もっと見る」ができる状態か。Target::Feed / Target::Channel のタブは
    /// requested_limit を立てないままにしているので、個別の場合分けなしで弾かれる。
    pub fn can_load_more(&self) -> bool {
        let state = self.view_state();
        state.requested_limit > 0
            && state.results.len() >= state.requested_limit
            && state.requested_limit < MAX_SEARCH_LIMIT
    }

    pub fn view_result_ids(&self) -> Vec<String> {
        self.view_results().iter().map(|r| r.id.clone()).collect()
    }

    /// 今の一覧に出ているチャンネル ID。登録済みかを確認する対象に使う。
    /// チャンネルを開いている間は行が channel_id を持たないので、開いた 1 つを返す。
    /// プレイリストの行は投稿者がばらばらなので、検索結果と同じく行から集める。
    pub fn view_channel_ids(&self) -> Vec<String> {
        if let Some(channel) = &self.channel {
            return vec![channel.channel_id.clone()];
        }
        self.view_results()
            .iter()
            .filter_map(|r| r.channel_id.clone())
            .collect()
    }

    /// 選択中の行が一覧の最後の行か。0件のときは false。
    pub fn view_is_at_last_result(&self) -> bool {
        let len = self.view_results().len();
        len > 0 && self.view_selected() + 1 == len
    }

    pub fn select_next(&mut self) {
        let len = self.view_results().len();
        if len == 0 {
            return;
        }
        self.set_view_selected((self.view_selected() + 1) % len);
    }

    pub fn select_prev(&mut self) {
        let len = self.view_results().len();
        if len == 0 {
            return;
        }
        self.set_view_selected((self.view_selected() + len - 1) % len);
    }

    pub fn set_results(&mut self, results: Vec<SearchResult>, target: &Target) {
        let results: Vec<SearchResult> = results
            .into_iter()
            .filter(|result| !self.hidden.hides(result))
            .collect();
        let state = self.view_state_mut();
        // 選んでいた動画がまだ新しい一覧にあればそこへ。通常の新規検索では
        // まったく違う一覧になるため ID がほぼ一致せず、これまでどおり先頭へ。
        let selected_id = state
            .results
            .get(state.selected)
            .map(|result| result.id.clone());
        state.results = results;
        state.selected = selected_id
            .and_then(|id| state.results.iter().position(|result| result.id == id))
            .unwrap_or(0);
        state.scroll = 0;
        // 0 件でも読み込み済みにする。戻るたびに同じ検索を投げ直さないため。
        state.loaded = true;
        // 取り直しの頼みはここで果たされる。
        state.reload = false;
        self.sync_from_view();
        if self.view_is_nested() {
            // 配信を持たないチャンネルも空のプレイリストも 0 件で来る。
            // 失敗ではないのでエラーにしない。
            if self.view_results().is_empty() {
                self.set_notice(Some(target.empty_message(self.cookies.for_search())));
            }
            // チャンネルはプレイリストの中からも開ける。開いている方の画面へ戻す。
            let mode = if self.channel.is_some() {
                Mode::Channel
            } else {
                Mode::Playlist
            };
            self.enter_search_mode(mode);
            return;
        }
        if self.results.is_empty() {
            self.error = Some(target.empty_message(self.cookies.for_search()));
            self.enter_search_mode(Mode::Input);
        } else {
            self.enter_search_mode(Mode::Results);
        }
    }

    /// 検索や再生の終わりで戻る検索画面のモード。設定画面/ダウンロード画面を開いている
    /// 間は戻り先だけ書き換える。裏で終わった検索が、編集中の画面を閉じてしまわないため。
    pub fn enter_search_mode(&mut self, mode: Mode) {
        if self.mode == Mode::Settings {
            self.settings_screen.return_mode = mode;
        } else if self.mode == Mode::Download {
            self.download.return_mode = mode;
        } else {
            self.mode = mode;
        }
    }

    /// 今の一覧の出どころ。変わっていれば結果集合ごと入れ替わっている。
    pub fn view_key(&self) -> ViewKey {
        (
            self.channel.as_ref().map(|channel| channel.tab.index()),
            self.tabs.selected(),
            self.playlist
                .as_ref()
                .map(|playlist| playlist.playlist_id.clone()),
        )
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

    /// 一覧を入れ替えた後の取り込み。チャンネル/プレイリストを開いている間は検索結果の
    /// 写しを保ったまま、世代とサムネイルの状態表だけを開いている一覧へ向ける。
    pub fn sync_from_view(&mut self) {
        if !self.view_is_nested() {
            self.sync_from_tab();
            return;
        }
        self.results_generation = self.results_generation.wrapping_add(1);
        let ids = self.view_result_ids();
        self.thumbs.reset(&ids);
        self.thumbs.mark_dirty();
    }

    /// 非表示にした行を、開いている一覧からも取り除く。次の検索を待たずに消すため。
    pub fn drop_hidden(&mut self) {
        // 選択位置は App 側が持っている。先に書き戻さないと取り込みで巻き戻る。
        if !self.view_is_nested() {
            self.store_to_tab();
        }
        for state in self.tabs.states_mut() {
            retain_visible(state, &self.hidden);
        }
        if let Some(channel) = self.channel.as_mut() {
            for state in &mut channel.states {
                retain_visible(state, &self.hidden);
            }
        }
        if let Some(playlist) = self.playlist.as_mut() {
            retain_visible(&mut playlist.state, &self.hidden);
        }
        self.sync_from_view();
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

    /// 分岐は網羅する。モードを増やしたときの書き分け漏れをコンパイラに拾わせる。
    pub fn status_line(&self) -> String {
        let line = match self.mode {
            Mode::Playing => self.playing_status(),
            Mode::Input => self.search_status("検索したい語句を入力して Enter".to_string()),
            Mode::Results => self.search_status(self.results_status()),
            Mode::Channel => self.search_status(self.channel_status()),
            Mode::Playlists => self.search_status(playlists_screen::playlists_status(self)),
            Mode::Playlist => self.search_status(self.playlist_status()),
            Mode::Settings => settings_screen::settings_status(self),
            Mode::Download => download_screen::download_status(self),
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

    /// バックグラウンド中に状態行の先頭へ足す、再生中のタイトルの目印。
    pub(crate) fn background_marker(&self) -> String {
        if self.background {
            format!("▶ {}  |  ", self.playback.title)
        } else {
            String::new()
        }
    }

    /// 件数と選択中のタイトルの本体。results_status/channel_status で共有する。
    fn results_body(&self) -> String {
        let count = format!("{} 件", self.view_results().len());
        if self.thumbs.is_fetching() {
            return format!("{count}  |  サムネイル取得中...");
        }
        match self.view_selected_result() {
            Some(result) => format!("{count}  |  {}", result.title),
            None => count,
        }
    }

    /// 格子のタイトルは 18 桁ほどで切れるので、選択中の完全なタイトルはここに出す。
    pub(crate) fn results_status(&self) -> String {
        format!("{}{}", self.background_marker(), self.results_body())
    }

    /// チャンネル名とタブは検索欄に出ないので、状態行の先頭に出す。
    fn channel_status(&self) -> String {
        let Some(channel) = &self.channel else {
            return self.results_status();
        };
        format!(
            "{}{} [{}]  |  {}",
            self.background_marker(),
            channel.channel_title,
            channel.tab.label(),
            self.results_body()
        )
    }

    /// プレイリスト名は検索欄に出ないので、チャンネルと同じく状態行の先頭に出す。
    fn playlist_status(&self) -> String {
        let Some(playlist) = &self.playlist else {
            return self.results_status();
        };
        format!(
            "{}{}  |  {}",
            self.background_marker(),
            playlist.playlist_title,
            self.results_body()
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
    use crate::cookies::{ChannelTab, CookieSource, Feed};
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
            channel_id: None,
            is_live: false,
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
        assert!(app.view_selected_result().is_none());
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
    fn background_marker_leads_the_results_and_channel_status() {
        let app = App {
            mode: Mode::Results,
            background: true,
            results: vec![result("a")],
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        assert_eq!(app.status_line(), "▶ song  |  1 件  |  title a");

        // バックグラウンドでなければ足さない。
        let app = App {
            background: false,
            ..app
        };
        assert_eq!(app.status_line(), "1 件  |  title a");

        let mut channel = ChannelView::new("UCabc".to_string(), "channel".to_string());
        channel.state_mut().results = vec![result("a")];
        let app = App {
            mode: Mode::Channel,
            background: true,
            channel: Some(channel),
            playback: Playback {
                title: "song".to_string(),
                ..Playback::default()
            },
            ..App::default()
        };
        assert_eq!(
            app.status_line(),
            "▶ song  |  channel [動画]  |  1 件  |  title a"
        );
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
    fn set_results_keeps_the_selected_video_when_it_is_still_in_the_new_list() {
        // 「もっと見る」で件数が増えて並びが変わっても、選んでいた動画を追い続ける。
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.tabs.state_mut().selected = 1; // "b" を選んでいた

        app.set_results(
            vec![result("c"), result("b"), result("a")],
            &search_target(),
        );
        assert_eq!(app.tabs.state().selected, 1, "b の新しい位置へ移る");
    }

    #[test]
    fn set_results_falls_back_to_the_top_when_the_selected_video_is_gone() {
        // 通常の新規検索では ID がほぼ一致しないため、これまでどおり先頭へ戻る。
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.tabs.state_mut().selected = 1;

        app.set_results(vec![result("x"), result("y")], &search_target());
        assert_eq!(app.tabs.state().selected, 0);
    }

    #[test]
    fn can_load_more_is_false_until_a_tab_has_actually_requested_a_count() {
        let app = App::default();
        assert!(!app.can_load_more(), "取得前は requested_limit が 0");
    }

    #[test]
    fn can_load_more_is_true_when_the_full_requested_count_came_back() {
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.tabs.state_mut().requested_limit = 2;
        assert!(app.can_load_more());
    }

    #[test]
    fn can_load_more_is_false_once_the_results_run_short_of_the_request() {
        // 要求件数より実際の結果が少なければ、それ以上無いと分かる。
        let mut app = App::default();
        app.set_results(vec![result("a")], &search_target());
        app.tabs.state_mut().requested_limit = 2;
        assert!(!app.can_load_more());
    }

    #[test]
    fn can_load_more_is_false_once_the_cap_is_reached() {
        let max = crate::settings::MAX_SEARCH_LIMIT;
        let mut app = App::default();
        let results: Vec<SearchResult> = (0..max).map(|i| result(&i.to_string())).collect();
        app.set_results(results, &search_target());
        app.tabs.state_mut().requested_limit = max;
        // 要求件数どおり満額で返っていても、上限まで要求済みならこれ以上は無い。
        assert!(!app.can_load_more(), "上限まで要求済み");
    }

    #[test]
    fn view_is_at_last_result_is_false_when_nothing_is_selected_yet() {
        let app = App::default();
        assert!(!app.view_is_at_last_result(), "0件のときは false");
    }

    #[test]
    fn view_is_at_last_result_is_false_in_the_middle_of_the_list() {
        let mut app = App::default();
        app.set_results(
            vec![result("a"), result("b"), result("c")],
            &search_target(),
        );
        app.selected = 1;
        assert!(!app.view_is_at_last_result());
    }

    #[test]
    fn view_is_at_last_result_is_true_on_the_last_row() {
        let mut app = App::default();
        app.set_results(
            vec![result("a"), result("b"), result("c")],
            &search_target(),
        );
        app.selected = 2;
        assert!(app.view_is_at_last_result());
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

    /// 検索結果を 2 件持ち、そこからチャンネルを開いた App。
    fn channel_app() -> App {
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.selected = 1;
        app.scroll = 4;
        app.store_to_tab();
        app.channel = Some(ChannelView::new(
            "UCabc".to_string(),
            "Some Channel".to_string(),
        ));
        app.mode = Mode::Channel;
        app.sync_from_view();
        app
    }

    #[test]
    fn a_channel_view_keeps_one_state_per_tab() {
        let mut view = ChannelView::new("UCabc".to_string(), "Some Channel".to_string());
        assert_eq!(view.tab, ChannelTab::Videos);
        assert_eq!(
            view.target(),
            Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Videos
            }
        );

        view.state_mut().results = vec![result("v")];
        view.state_mut().selected = 1;
        view.state_mut().loaded = true;

        view.next_tab();
        assert_eq!(view.tab, ChannelTab::Shorts);
        assert!(view.state().results.is_empty(), "タブごとに別の一覧");
        assert!(!view.state().loaded);

        view.prev_tab();
        assert_eq!(view.state().results.len(), 1, "戻ると持ち越した一覧");
        assert_eq!(view.state().selected, 1);

        // 端では巻き戻る。
        view.prev_tab();
        assert_eq!(view.tab, ChannelTab::Streams);
        view.next_tab();
        assert_eq!(view.tab, ChannelTab::Videos);
    }

    #[test]
    fn selecting_a_channel_tab_by_position_ignores_the_ones_that_do_not_exist() {
        let mut view = ChannelView::new("UCabc".to_string(), "c".to_string());
        assert!(view.select_tab(2));
        assert_eq!(view.tab, ChannelTab::Streams);
        assert!(!view.select_tab(3));
        assert!(!view.select_tab(usize::MAX));
        assert_eq!(view.tab, ChannelTab::Streams, "範囲外では動かさない");
    }

    #[test]
    fn the_view_follows_the_channel_and_leaves_the_search_results_alone() {
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").state_mut().results =
            vec![result("v0"), result("v1"), result("v2")];

        assert_eq!(app.view_result_ids(), ["v0", "v1", "v2"]);
        assert_eq!(app.result_ids(), ["a", "b"], "検索結果はそのまま");

        app.set_view_selected(2);
        app.set_view_scroll(3);
        assert_eq!(app.view_selected(), 2);
        assert_eq!(app.view_scroll(), 3);
        assert_eq!(
            app.view_selected_result().map(|r| r.id.as_str()),
            Some("v2")
        );
        assert_eq!(app.selected, 1, "検索結果側の選択は動かさない");
        assert_eq!(app.scroll, 4);

        // チャンネルを閉じれば検索結果へ戻る。
        app.channel = None;
        assert_eq!(app.view_result_ids(), ["a", "b"]);
        assert_eq!(app.view_selected(), 1);
        assert_eq!(app.view_scroll(), 4);
        assert_eq!(app.view_selected_result().map(|r| r.id.as_str()), Some("b"));
    }

    #[test]
    fn the_channel_ids_of_the_open_list_come_out_for_checking() {
        let mut app = App::default();
        app.set_results(
            vec![
                SearchResult {
                    channel_id: Some("UC1".to_string()),
                    ..result("v0")
                },
                SearchResult {
                    channel_id: Some("UC2".to_string()),
                    ..result("v1")
                },
                result("v2"),
            ],
            &search_target(),
        );

        assert_eq!(app.view_channel_ids(), ["UC1", "UC2"], "持たない行は飛ばす");

        // チャンネルを開いている間は、行に channel_id が入らないので開いた 1 つを見る。
        let app = channel_app();
        assert_eq!(app.view_channel_ids(), ["UCabc"]);
    }

    #[test]
    fn the_view_selection_wraps_inside_the_channel_tab() {
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").state_mut().results =
            vec![result("v0"), result("v1")];

        app.select_next();
        assert_eq!(app.view_selected(), 1);
        app.select_next();
        assert_eq!(app.view_selected(), 0, "巻き戻る");
        app.select_prev();
        assert_eq!(app.view_selected(), 1);
        assert_eq!(app.selected, 1, "検索結果側は触らない");
    }

    #[test]
    fn set_results_writes_through_to_the_current_channel_tab() {
        let mut app = channel_app();
        app.set_results(
            vec![result("v0")],
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Videos,
            },
        );

        let channel = app.channel.as_ref().expect("channel");
        assert_eq!(channel.states[0].results.len(), 1);
        assert!(channel.states[0].loaded);
        assert!(channel.states[1].results.is_empty(), "他のタブは空のまま");
        assert_eq!(app.mode, Mode::Channel);
        assert_eq!(app.result_ids(), ["a", "b"], "検索結果は残す");
        assert!(app.error.is_none());
    }

    #[test]
    fn an_empty_channel_tab_is_told_as_a_notice_not_an_error() {
        // 配信を持たないチャンネルでも 0 件になるだけ。エラー表示にはしない。
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").tab = ChannelTab::Streams;
        app.set_results(
            Vec::new(),
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Streams,
            },
        );

        assert!(app.error.is_none(), "{:?}", app.error);
        let notice = app.notice.as_deref().expect("文言を出す");
        assert!(notice.contains("ライブ配信"), "{notice}");
        assert_eq!(app.mode, Mode::Channel, "検索欄へ落とさない");
        let channel = app.channel.as_ref().expect("channel");
        assert!(channel.states[2].loaded, "0 件でも読み込み済みにする");
    }

    #[test]
    fn the_status_line_names_the_channel_and_the_tab() {
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").state_mut().results = vec![result("v0")];
        let line = app.status_line();
        assert!(line.contains("Some Channel"), "{line}");
        assert!(line.contains("動画"), "{line}");
        assert!(line.contains("1 件"), "{line}");
    }

    fn entry(id: &str, title: &str) -> PlaylistEntry {
        PlaylistEntry {
            id: id.to_string(),
            title: title.to_string(),
        }
    }

    /// 検索結果を 2 件持ち、そこからプレイリストの動画一覧まで開いた App。
    /// 選択位置はまだタブへ書き戻していない。取り込みで巻き戻さないことも見たいため。
    fn playlist_app() -> App {
        let mut app = App::default();
        app.set_results(vec![result("a"), result("b")], &search_target());
        app.selected = 1;
        app.scroll = 4;
        app.playlists = Some(PlaylistsView {
            entries: vec![entry("PL1", "作業用BGM"), entry("PL2", "あとで見る")],
            selected: 0,
            loaded: true,
        });
        app.playlist = Some(PlaylistView::new(
            "PL1".to_string(),
            "作業用BGM".to_string(),
        ));
        app.mode = Mode::Playlist;
        app.sync_from_view();
        app
    }

    #[test]
    fn a_playlist_view_starts_empty_and_knows_its_target() {
        let app = App::default();
        assert!(app.playlists.is_none(), "通常の検索画面では持たない");
        assert!(app.playlist.is_none());

        let list = PlaylistsView::default();
        assert!(list.entries.is_empty());
        assert_eq!(list.selected, 0);
        assert!(!list.loaded);

        let view = PlaylistView::new("PL1".to_string(), "作業用BGM".to_string());
        assert!(view.state.results.is_empty());
        assert!(!view.state.loaded);
        assert_eq!(view.target(), Target::Playlist("PL1".to_string()));
    }

    #[test]
    fn the_view_follows_the_open_playlist_and_leaves_the_search_results_alone() {
        let mut app = playlist_app();
        app.playlist.as_mut().expect("playlist").state.results =
            vec![result("v0"), result("v1"), result("v2")];

        assert_eq!(app.view_result_ids(), ["v0", "v1", "v2"]);
        assert_eq!(app.result_ids(), ["a", "b"], "検索結果はそのまま");

        app.set_view_selected(2);
        app.set_view_scroll(3);
        assert_eq!(app.view_selected(), 2);
        assert_eq!(app.view_scroll(), 3);
        assert_eq!(
            app.view_selected_result().map(|r| r.id.as_str()),
            Some("v2")
        );
        assert_eq!(app.selected, 1, "検索結果側の選択は動かさない");
        assert_eq!(app.scroll, 4);

        // プレイリストを閉じれば検索結果へ戻る。一覧画面はまだ開いたまま。
        app.playlist = None;
        assert_eq!(app.view_result_ids(), ["a", "b"]);
        assert_eq!(app.view_selected(), 1);
        assert_eq!(app.view_scroll(), 4);
    }

    #[test]
    fn a_channel_opened_from_a_playlist_takes_over_the_view() {
        // プレイリストの中からもチャンネルへ移れる。後から開いた方が一覧の主になり、
        // 閉じるとプレイリストへ戻る。
        let mut app = playlist_app();
        app.playlist.as_mut().expect("playlist").state.results = vec![result("v0")];
        app.channel = Some(ChannelView::new(
            "UCabc".to_string(),
            "Some Channel".to_string(),
        ));
        app.channel.as_mut().expect("channel").state_mut().results =
            vec![result("c0"), result("c1")];

        assert_eq!(app.view_result_ids(), ["c0", "c1"]);
        assert_eq!(app.view_channel_ids(), ["UCabc"]);

        app.channel = None;
        assert_eq!(app.view_result_ids(), ["v0"], "プレイリストへ戻る");
    }

    #[test]
    fn the_channel_ids_of_an_open_playlist_come_from_its_rows() {
        // プレイリストの行は投稿者がばらばらなので、チャンネルと違って行から集める。
        let mut app = playlist_app();
        app.playlist.as_mut().expect("playlist").state.results = vec![
            from_channel("v0", "UC1"),
            from_channel("v1", "UC2"),
            result("v2"),
        ];

        assert_eq!(app.view_channel_ids(), ["UC1", "UC2"], "持たない行は飛ばす");
    }

    #[test]
    fn set_results_writes_through_to_the_open_playlist() {
        let mut app = playlist_app();
        app.set_results(vec![result("v0")], &Target::Playlist("PL1".to_string()));

        let playlist = app.playlist.as_ref().expect("playlist");
        assert_eq!(playlist.state.results.len(), 1);
        assert!(playlist.state.loaded);
        assert_eq!(app.mode, Mode::Playlist);
        assert_eq!(app.result_ids(), ["a", "b"], "検索結果は残す");
        assert_eq!(app.selected, 1, "検索側の写しは触らない");
        assert_eq!(app.scroll, 4);
        assert!(app.error.is_none());
        assert!(
            !app.can_load_more(),
            "中身は全部返っているので追加読み込みはしない"
        );
    }

    #[test]
    fn an_empty_playlist_is_told_as_a_notice_not_an_error() {
        let mut app = playlist_app();
        app.set_results(Vec::new(), &Target::Playlist("PL1".to_string()));

        assert!(app.error.is_none(), "{:?}", app.error);
        let notice = app.notice.as_deref().expect("文言を出す");
        assert!(
            notice.contains("プレイリストには動画がありません"),
            "{notice}"
        );
        assert_eq!(app.mode, Mode::Playlist, "検索欄へ落とさない");
        assert!(
            app.playlist.as_ref().expect("playlist").state.loaded,
            "0 件でも読み込み済みにする"
        );
    }

    #[test]
    fn the_view_key_tells_an_open_playlist_from_the_search_results() {
        // 鍵が変わらないと、出入りのときサムネイルを取り直す合図が出ない。
        let mut app = playlist_app();
        let inside = app.view_key();

        app.playlist = None;
        assert_ne!(app.view_key(), inside, "中身を閉じたら別の一覧");

        app.playlist = Some(PlaylistView::new(
            "PL2".to_string(),
            "あとで見る".to_string(),
        ));
        assert_ne!(app.view_key(), inside, "別のプレイリストでも別の一覧");
    }

    #[test]
    fn the_status_line_names_the_playlist_and_its_count() {
        let mut app = playlist_app();
        app.playlist.as_mut().expect("playlist").state.results = vec![result("v0")];
        let line = app.status_line();
        assert!(line.contains("作業用BGM"), "{line}");
        assert!(line.contains("1 件"), "{line}");
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

    #[test]
    fn the_search_results_do_not_close_the_settings_screen() {
        // 検索中に設定を開いても、結果が届いたところで画面が消えない。
        let mut app = App {
            mode: Mode::Settings,
            settings_screen: SettingsScreen {
                return_mode: Mode::Input,
                ..SettingsScreen::default()
            },
            ..App::default()
        };
        app.enter_search_mode(Mode::Results);
        assert_eq!(app.mode, Mode::Settings);
        assert_eq!(
            app.settings_screen.return_mode,
            Mode::Results,
            "閉じたら結果へ戻す"
        );

        // 設定画面を開いていなければ、そのままモードを動かす。
        let mut app = App::default();
        app.enter_search_mode(Mode::Results);
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn the_search_results_do_not_close_the_download_screen() {
        // ダウンロード画面を開いている間に裏の検索/再生が終わっても、画面が消えない。
        let mut app = App {
            mode: Mode::Download,
            download: DownloadForm {
                return_mode: Mode::Playing,
                ..DownloadForm::default()
            },
            ..App::default()
        };
        app.enter_search_mode(Mode::Results);
        assert_eq!(app.mode, Mode::Download);
        assert_eq!(
            app.download.return_mode,
            Mode::Results,
            "閉じたら結果へ戻す"
        );
    }

    fn from_channel(id: &str, channel_id: &str) -> SearchResult {
        SearchResult {
            channel_id: Some(channel_id.to_string()),
            ..result(id)
        }
    }

    fn hiding(videos: &[&str], channels: &[&str]) -> Hidden {
        Hidden {
            videos: videos.iter().map(|id| id.to_string()).collect(),
            channels: channels.iter().map(|id| id.to_string()).collect(),
            ..Hidden::default()
        }
    }

    fn by_uploader(id: &str, uploader: &str) -> SearchResult {
        SearchResult {
            uploader: Some(uploader.to_string()),
            ..result(id)
        }
    }

    #[test]
    fn set_results_drops_the_hidden_videos_and_channels() {
        let mut app = App {
            hidden: hiding(&["v1"], &["UC1"]),
            ..App::default()
        };
        app.set_results(
            vec![
                result("v1"),
                from_channel("v2", "UC1"),
                from_channel("v3", "UC9"),
                result("v4"),
            ],
            &search_target(),
        );

        assert_eq!(app.result_ids(), ["v3", "v4"]);
        assert_eq!(
            app.tabs.state().results.len(),
            2,
            "タブ側も絞った結果を持つ"
        );
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn set_results_where_everything_is_hidden_ends_up_empty() {
        let mut app = App {
            hidden: hiding(&["v1"], &[]),
            ..App::default()
        };
        app.set_results(vec![result("v1")], &search_target());

        assert!(app.results.is_empty());
        assert!(app.error.is_some(), "0 件として扱う");
        assert_eq!(app.mode, Mode::Input);
    }

    #[test]
    fn set_results_drops_hidden_rows_from_a_channel_tab_too() {
        let mut app = channel_app();
        app.hidden = hiding(&["v1"], &[]);
        app.set_results(
            vec![result("v1"), result("v2")],
            &Target::Channel {
                id: "UCabc".to_string(),
                tab: ChannelTab::Videos,
            },
        );

        assert_eq!(app.view_result_ids(), ["v2"]);
    }

    #[test]
    fn dropping_hidden_takes_them_out_of_the_open_list() {
        let mut app = App::default();
        app.set_results(
            vec![result("v1"), result("v2"), from_channel("v3", "UC1")],
            &search_target(),
        );
        app.selected = 2;
        app.scroll = 2;

        app.hidden = hiding(&["v1"], &["UC1"]);
        app.drop_hidden();

        assert_eq!(app.result_ids(), ["v2"], "次の検索を待たずに消える");
        assert_eq!(app.tabs.state().results.len(), 1);
        assert_eq!(app.selected, 0, "残りの件数へ丸める");
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn dropping_hidden_keeps_the_selection_the_user_moved_to() {
        let mut app = App::default();
        app.set_results(
            vec![result("v1"), result("v2"), result("v3")],
            &search_target(),
        );
        // タブへ書き戻していない選択位置。取り込みで巻き戻してはいけない。
        app.selected = 2;

        app.hidden = hiding(&["v1"], &[]);
        app.drop_hidden();

        assert_eq!(app.result_ids(), ["v2", "v3"]);
        assert_eq!(app.selected, 1, "見ていた v3 を選んだまま");
    }

    #[test]
    fn dropping_hidden_reaches_the_channel_tabs_and_the_search_side() {
        let mut app = channel_app();
        app.channel.as_mut().expect("channel").state_mut().results =
            vec![from_channel("v1", "UCabc"), result("v2")];

        app.hidden = hiding(&["a"], &["UCabc"]);
        app.drop_hidden();

        assert_eq!(app.view_result_ids(), ["v2"], "チャンネルの一覧から外す");
        // 検索側のタブは a / b を持っていた。戻ったときには a が消えている。
        assert_eq!(app.tabs.state().results.len(), 1);
    }

    #[test]
    fn dropping_hidden_reaches_the_open_playlist() {
        let mut app = playlist_app();
        app.playlist.as_mut().expect("playlist").state.results =
            vec![from_channel("v1", "UCabc"), result("v2")];

        app.hidden = hiding(&["a"], &["UCabc"]);
        app.drop_hidden();

        assert_eq!(app.view_result_ids(), ["v2"], "プレイリストの一覧から外す");
        assert_eq!(app.tabs.state().results.len(), 1, "検索側のタブも絞る");
    }

    #[test]
    fn set_results_drops_rows_that_only_carry_the_channel_name() {
        // フィード系の行は channel_id を持たない。名前で外さないと隠したチャンネルが残る。
        let mut hidden = hiding(&[], &["UC1"]);
        hidden.channel_names.insert("One Channel".to_string());
        let mut app = App {
            hidden,
            ..App::default()
        };
        app.set_results(
            vec![
                by_uploader("v1", "One Channel"),
                by_uploader("v2", "Another Channel"),
            ],
            &search_target(),
        );

        assert_eq!(app.result_ids(), ["v2"]);
    }

    #[test]
    fn dropping_hidden_keeps_the_selection_on_the_same_video() {
        let mut app = App::default();
        app.set_results(
            vec![from_channel("v1", "UC1"), result("v2"), result("v3")],
            &search_target(),
        );
        // 選んでいるのは v2。手前の v1 が消えても v2 のままにする。
        app.selected = 1;

        app.hidden = hiding(&[], &["UC1"]);
        app.drop_hidden();

        assert_eq!(app.result_ids(), ["v2", "v3"]);
        assert_eq!(app.selected, 0, "選んでいた v2 を選んだまま");
    }

    #[test]
    fn dropping_hidden_pulls_the_scroll_back_to_the_selection() {
        let mut app = App::default();
        let mut results = vec![from_channel("v0", "UC1")];
        results.extend((1..20).map(|n| result(&format!("v{n}"))));
        app.set_results(results, &search_target());
        app.selected = 12;
        app.scroll = 12;

        // 先頭の 1 件だけ消えても、表示位置が選択より後ろに残ると残りを飛び越す。
        app.hidden = hiding(&[], &["UC1"]);
        app.drop_hidden();

        assert_eq!(app.selected, 11, "v12 を選んだまま");
        assert!(app.scroll <= app.selected, "選択より前から描き始める");
    }

    #[test]
    fn dropping_hidden_from_the_middle_does_not_skip_the_rest() {
        let mut app = App::default();
        let mut results: Vec<SearchResult> = (0..5).map(|n| result(&format!("v{n}"))).collect();
        results.extend((5..20).map(|n| from_channel(&format!("v{n}"), "UC1")));
        app.set_results(results, &search_target());
        app.selected = 2;
        app.scroll = 12;

        app.hidden = hiding(&[], &["UC1"]);
        app.drop_hidden();

        assert_eq!(app.results.len(), 5);
        assert_eq!(app.selected, 2);
        assert_eq!(app.scroll, 2, "残り 5 件の後ろから描き始めない");
    }

    #[test]
    fn dropping_hidden_with_nothing_hidden_changes_nothing() {
        let mut app = App::default();
        app.set_results(vec![result("v1"), result("v2")], &search_target());
        app.selected = 1;
        app.drop_hidden();

        assert_eq!(app.result_ids(), ["v1", "v2"]);
        assert_eq!(app.selected, 1);
    }
}
