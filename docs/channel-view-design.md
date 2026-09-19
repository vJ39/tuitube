# チャンネル閲覧

検索結果で選択中の動画のチャンネルへ移動し、そのチャンネルの動画/ショート/ライブ配信タブを閲覧できるようにする。

## 実現可否(確認済み)

yt-dlpは`https://www.youtube.com/channel/<channel_id>/videos`(`/shorts`・`/streams`も同様)を`--flat-playlist --dump-json`でcookie連携なし(公開情報)で取得できる。実測30件0.7秒程度。配信タブを持たないチャンネルの`/streams`はexit=1+明確なエラーで1秒未満に落ちる。

`--flat-playlist`の各行はチャンネルタブ経由の場合`channel_id`/`uploader`が空で、代わりに`playlist_channel_id`/`playlist_uploader`に入る(通常検索と構造が異なる)。表示に使うのは`id`(動画ID、`SearchResult::url()`でそのまま再生できる)と`title`のみで足りる。

## SearchResultの拡張

`channel_id: Option<String>`を追加する(yt-dlpの`channel_id`フィールド、UC...形式)。`parse_line`に読み取りを1行追加する。

## Target::Channel

`cookies.rs`の`Target` enumに`Channel { id: String, tab: ChannelTab }`を追加する。

```rust
pub enum ChannelTab {
    Videos,
    Shorts,
    Streams,
}
```

- `yt_dlp_url`: `format!("https://www.youtube.com/channel/{id}/{tab_path}")` (`tab_path`は"videos"/"shorts"/"streams")
- `requires_login`: false(公開情報のため)
- `empty_message`: タブに応じた文言(例: Streamsが空なら「配信中のライブはありません」、エラー表示にしない)
- 無制限取得を避けるため、既存の`FEED_LIMIT`と同じパターンで`CHANNEL_LIMIT`(例: 50)を新設し`--playlist-end`を付ける

## 状態

`category::TabState`(results/selected/scroll/loaded、既存の汎用データ構造)をそのまま流用する。`Tabs`本体(カテゴリタブ専用、「すべて」を強制挿入する等の仕様がある)は流用しない。

`App`に新設:

```rust
pub struct ChannelView {
    pub channel_id: String,
    pub channel_title: String,
    pub tab: ChannelTab,
    pub states: [TabState; 3],  // Videos/Shorts/Streamsの順
}
```

`App.channel: Option<ChannelView>`、`Mode::Channel`を追加する。

## 操作

- Resultsモードで選択中の結果に`channel_id`があれば`c`キーでチャンネルへ移動する(`channel_id`が無ければ何もしない)
- Mode::Channel: `Tab`/`BackTab`でVideos/Shorts/Streamsを切り替え(未読み込みタブは切替時に検索開始、既存の`switch_tab`と同じ骨格)。`↑↓←→`でのグリッド選択・`Enter`での再生は既存のResultsモードのロジック(`move_selection`/`start_playback`/クリック再生)を、対象データを`app.results`ではなく`app.channel`の現在タブの`TabState`に向ける形で共用する
- `r`で今のタブを取り直す(`loaded`を落としてから再検索する。Resultsの`r`と同じ)
- `Esc`または`/`でチャンネル一覧を抜け、元のResults(検索結果)へ戻る(`app.channel = None`)
- `q`でアプリ終了(既存のResultsと同じ)
- `S`または`Ctrl+S`で設定画面を開く(既存のResultsと同じ)
- Ctrl付きの文字キーは設定を開く`Ctrl+S`以外受け付けない

## 取得に失敗したとき

yt-dlpが`This channel does not have a ... tab`を出して落ちた場合だけ0件として扱い、`empty_message`を出す。yt-dlp未インストール・通信失敗・タイムアウトはエラーとして表示し、そのタブは`loaded = false`のまま`Mode::Channel`に留める(`r`で取り直せる)。

## 対象外(v1)

- チャンネル一覧からさらに別チャンネルへ移動する多段ナビゲーション(履歴スタックは持たない。戻ると必ず元のResultsに戻る)
- ライブ配信中の動画の特別な再生UI(シーク不可等はmpv/yt-dlpの挙動に任せる)
- Feed(:ytrec等)経由の結果からのチャンネル移動時、channel_idが取得できるかは未検証(cookie環境が必要なため)。取得できない場合はcキーが無反応になるだけで、既存の動作には影響しない
