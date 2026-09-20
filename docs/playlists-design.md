# プレイリスト一覧・動画一覧・戻る(#69)

自分のYouTubeプレイリストを一覧表示し、選ぶと中の動画一覧を表示、戻るナビゲーションにも対応する。既存の `Mode::Channel`(#49、チャンネル→タブ→動画)と同じ考え方だが、階層が1段深い(プレイリスト一覧→プレイリストの中の動画)。

## 前提: yt-dlp での取得可否(実機で確認済み)

- 一覧: `https://www.youtube.com/feed/playlists` を `--flat-playlist --dump-json` で検索すると、自分の全プレイリスト(`Liked videos`/`Watch later`等の特殊プレイリストも含む)が `{id, title, uploader: "View full playlist", duration: null, channel_id: null}` の形で返る。`uploader` は実際のアップロード者名でなく固定文字列なので、既存の `SearchResult`(動画用、uploaderフォールバックの仕組みを持つ)には乗せず、専用の軽量構造体で受ける。
- 個々のプレイリストの中身: `https://www.youtube.com/playlist?list=<id>` を検索すると、通常の動画検索と同じ形(`id`/`title`/`uploader`/`duration`/`channel_id`)で動画が返る。既存の `SearchResult`・検索経路をそのまま使える。

## データモデル

`src/search.rs` に追加:

```rust
/// プレイリスト一覧の1件。動画一覧とはフィールドが違うので SearchResult と分ける。
pub struct PlaylistEntry {
    pub id: String,
    pub title: String,
}
```

一覧の行は `parse_playlist_lines()` で読む。`id` が無い行・壊れた行・空行は落とし、`title` が無い行は `SearchResult` と同じく `(title unknown)` にする。`Liked videos`(`LL`)・`Watch later`(`WL`)も他と同じ1件として扱う。

`cookies.rs` の `Target` に追加:

```rust
pub enum Target {
    Search(String),
    Feed(Feed),
    Channel { id: String, tab: ChannelTab },
    Playlist(String), // 追加。id は list= の値。
}
```

URL と件数:

- 一覧: `PLAYLISTS_URL`(`https://www.youtube.com/feed/playlists`)。id を取らないので `Target` には乗せず、`playlists_args()` が `--flat-playlist --dump-json` と cookie 引数を組み立てる。cookie が無いと空で返る。返るのが動画行でないため `run_search()`(`SearchReport` を返す)には乗らず、`fetch_channel()` と同じ形の `fetch_playlists()` を通って `AppEvent::PlaylistsReady` で戻る。0 件は失敗でないので、エラーでなく「プレイリストがありません (cookie が YouTube にログイン済みか確認してください)」を知らせとして出す。
- 個々のプレイリスト: `Target::Playlist(id)` が `https://www.youtube.com/playlist?list=<id>` になる。中身を全部返すので、チャンネルのタブと同じ `PLAYLIST_LIMIT`(= `CHANNEL_LIMIT`)を `--playlist-end` で渡す。`[search] limit` は使わない。
- 0 件のときの文言は「このプレイリストには動画がありません」。公開プレイリストは cookie 無しでも引けるので、フィードのように先回りで断らない(`requires_login()` は false)。

## 状態

`App` に追加(`channel: Option<ChannelView>` と並列):

```rust
pub struct PlaylistsView {
    pub entries: Vec<PlaylistEntry>,
    pub selected: usize,
    pub loaded: bool,
}

pub struct PlaylistView {
    pub playlist_id: String,
    pub playlist_title: String,
    pub state: TabState, // ChannelView が使っているものを再利用(1タブ分)
}
```

- `app.playlists: Option<PlaylistsView>` — 一覧を開いている間だけ `Some`。
- `app.playlist: Option<PlaylistView>` — 個々のプレイリストを開いている間だけ `Some`。

両方 `None` なら通常の検索/結果一覧、`playlists` だけ `Some` なら一覧画面、`playlist` も `Some` なら動画一覧画面、という3状態。`ChannelView` は現状 `app.channel` 1本で足りているが、プレイリストは「一覧」と「個々の中身」の2段があるため2本に分ける。

## モード

新規 `Mode::Playlists`(一覧)・`Mode::Playlist`(個々の動画一覧)。`Mode::Playlist` の描画・操作(結果グリッド/リスト、サムネイル、選択移動、再生開始)は既存の `Mode::Channel` のコードをそのまま流用できる箇所が多い(結果一覧を持つ画面という点で同型)。

## 画面遷移

- `Mode::Results` で `p` キー、`Mode::Input` で `Ctrl+P`: プレイリスト一覧を検索して `Mode::Playlists` へ(`app.playlists` を `Some` にする)。入力欄では `p` も検索語なので、`Ctrl+S`(設定)・`Ctrl+B`(前面へ)と同じく Ctrl 付きで取る。
- `Mode::Playlists`: `↑↓` で選択移動(末尾と先頭で巻き戻る)。`Enter` で選択したプレイリストの動画一覧を検索して `Mode::Playlist` へ(`app.playlist` を `Some` にする)。`Esc`/`/` で元の `Mode::Results`/`Mode::Input` へ戻り、`app.playlists` を `None` に。
- `Mode::Playlist`: 既存の `Mode::Channel` と同じ操作(結果一覧の移動・再生・サムネイル・`c` でチャンネルへ・`d` でダウンロード・`r` で取り直し・`h` で動画を隠す)。タブ送りとチャンネル登録は持たない。`h` はチャンネルと違い動画 1 件だけを隠す。`Esc`/`/` で `Mode::Playlists` へ戻り、`app.playlist` を `None` に(`app.playlists` の一覧は残っているので再検索しない)。
- `Mode::Playlist` で動画を選ぶと通常の `Mode::Playing` へ(既存の再生フローと同じ)。再生を終える・裏へ回すと、開いていた `Mode::Playlist`/`Mode::Playlists` へ戻る。
- 一覧は検索結果とは別の入れ物なので、`p` で開いて `Esc` で戻るだけなら検索結果のサムネイルは貼ったまま残る。

## 描画

- `Mode::Playlists`: サムネイルを持たないので `draw_list` と同じ形(タイトルのみのリスト)で描く。専用の `draw_playlists` を新設。画像が行に重ならないよう `grid_layout_in` はこのモードでは割り付けを組まない(`None` を返し、メインループには消す指示だけが残る)。
- `Mode::Playlist`: 既存の結果一覧描画(`draw_grid`/`draw_list`、`app.settings.search.layout` に従う)をそのまま使う。`view_results()`/`view_selected()` 等の既存の「今の画面が見ている一覧」抽象を `app.playlist` にも対応させる(`app.channel` と同じ扱いに追加する)。

## 実装箇所

- `src/search.rs`: `PlaylistEntry`、一覧のパース関数 `parse_playlist_lines()`(`--dump-json` の行から `id`/`title` だけ取る。`uploader`/`duration`/`channel_id` は読まない)、一覧を取る `playlists_args()`・`fetch_playlists()`。個々のプレイリストは `yt_dlp_args()`・`parse_lines()` の既存経路をそのまま使う
- `src/cookies.rs`: `Target::Playlist(String)`、`PLAYLISTS_URL`、`PLAYLIST_LIMIT`
- `src/app.rs`: `Mode::Playlists`/`Mode::Playlist`、`PlaylistsView`/`PlaylistView`、`view_results()` 等の既存抽象に `app.playlist` を追加、`AppEvent::PlaylistsReady`、`ViewKey` に開いているプレイリストを足す(画面が入れ替わったことを `view_key()` の変化で知らせ、サムネイルを貼り直させる)
- `src/actions.rs`: `open_playlists`(一覧を開く)、`apply_playlists_ready`(届いた一覧を取り込む)、`open_playlist`(個々を開く)、`reload_playlist`(開いているプレイリストを取り直す)、`leave_playlist`(`Mode::Playlists` へ戻る)、`leave_playlists`(元のモードへ戻る)。戻り先は再生終了・バックグラウンド化と共通の `search_return_mode()` で決める
- `src/main.rs`: `AppEvent::PlaylistsReady` を `apply_playlists_ready` へ渡す配線
- `src/input.rs`: `p`/`Ctrl+P` キー配線、`handle_key_playlists`・`handle_key_playlist`
- `src/ui.rs`: `draw_playlists`、既存の結果一覧描画を `Mode::Playlist` でも呼ぶ配線、`results_hints`/`input_hints` への案内追加

## 対象外(v1)

- プレイリスト一覧のサムネイル表示(タイトルのみ)
- プレイリストの並び替え・削除・新規作成
- プレイリスト一覧のキャッシュ(#34のキャッシュ機構への統合。毎回開き直すたびに検索する)
- プレイリスト内動画のいいね/登録済みバッジ(#66と同じ仕組みを流用できるはずだが、今回のスコープ外)
- `Mode::Playlists`/`Mode::Playlist` のマウス操作(キーだけで操作する。`results_click` はカテゴリタブ行を先に見るため、そのまま流用すると誤って反応する)
- プレイリスト内の追加読み込み(`m` キー・末尾の自動読み込み)。`load_more` の行き先はカテゴリタブなので、プレイリストには使えない
