# 再生画面のいいね/チャンネル登録アイコン(#65)

`Mode::Playing` の画面下に、いいね・チャンネル登録をクリック可能なアイコン(短いラベル)として表示する。状態(済み/未)の反映は `docs/engagement-status-design.md` の `EngagementCache` を使う。

## 現状

- いいね: `l` キーで実行可能(既存、#28)。視覚的な表示は無い。
- チャンネル登録: `s` キーは Playing 中は字幕トグルに割当済み。`subscribe_channel` は `app.channel`(`Option<ChannelView>`、チャンネル一覧表示中しか入らない)に依存しており、Playing 中は `app.channel` が無関係な値(前回チャンネル閲覧の残り、または `None`)なので実質呼べない。

## 変更方針

### 1. 再生中チャンネルIDの引き継ぎ

`Playback` に `channel_id: Option<String>` を追加する。`start_playback` で再生を始める時点の `SearchResult.channel_id` をコピーする。チャンネルタブ経由の行はもともと `channel_id` が無いため、その場合は `None` のままになる。

### 2. `subscribe_channel` の一般化

既存の `subscribe_channel(app, tx, session, deps)` は `app.channel` からしか channel_id を取れない。呼び出し元でチャンネルIDを渡す形に変える:

```rust
pub fn subscribe_to<B>(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session, channel_id: String, deps: Oauth<B>)
where B: oauth::Backend + 'static,
{
    start_oauth(app, tx, session, oauth::Action::Subscribe(channel_id), deps);
}
```

既存の `subscribe_channel`(Channelモード用、`app.channel` から取る)はこの関数を呼ぶ薄いラッパーに変える。Playing用は `subscribe_playing_channel` で、`app.playback.channel_id` があるときだけ `subscribe_to` を呼ぶ。

### 3. 画面レイアウト

`playing_areas` を4段([映像, シークバー, ステータス, ヘルプ])から5段([映像, シークバー, **アクション**, ステータス, ヘルプ])に変える。アクション行の高さは1(`Constraint::Length(1)`)。映像領域が1行減る。

アクション行の描画(`draw_actions` 新設):

- いいね: `♥いいね`(済み=`Color::Red` + 太字、未=`Color::Gray`)
- チャンネル登録: `channel_id` が `Some` のときだけ `＋登録`(済み=`Color::Green` + 太字、未=`Color::Gray`)を空白2つ挟んで続けて表示。`None` のときはいいねラベルのみ。登録状態が未確認(`is_subscribed` が `None`)のときは未登録と同じ見た目にする。

描画は `tab_spans`(既存、タブラベルの描画パターン)と同型: `Vec<Span<'static>>` を作って `Line::from(spans)` → `Paragraph`。

### 4. マウス

`ui.rs` に `action_at_point(app, column, row) -> Option<ActionKind>`(`ActionKind::Like` / `ActionKind::Subscribe`)を新設。描画側と同じ「ラベルの並び→矩形」計算を共有する(`tab_at_point` と同型)。

`handle_mouse_playing` の既存のシーク処理の前段に、アクション行のクリック判定を追加する(先に判定し、当たらなければ既存のシーク処理へフォールスルー)。判定するのは左ボタンの押し込みだけにする。移動まで奪うと、シークバーのhover表示が動かなくなる。

### 5. キー

チャンネル登録用に新規キーを割り当てる(`l`=いいねは変更しない)。空きキーから `u`(登録='Uploader'/'subscribe'に近い語感)を使う。既存のヘルプ文言(`playing_hints`)に `♥l:いいね` `＋u:登録` を追加する(`channel_id` が無いときは登録ヒント自体を出さない)。

## 実装箇所

- `src/app.rs`: `Playback.channel_id: Option<String>` 追加
- `src/actions.rs`: `start_playback` で `channel_id` を引き継ぐ、`subscribe_to`(一般化) の新設、既存 `subscribe_channel` はラッパー化
- `src/input.rs`: Playing の `u` キー配線、`handle_mouse_playing` にアクション行クリック追加
- `src/ui.rs`: `playing_areas` を5段に、`draw_actions`、`action_at_point`、`playing_hints` に2項目追加
- `src/engagement.rs`(`docs/engagement-status-design.md` 側): 済み/未の判定に使う

## 対象外(v1)

- アクション行のマウスホバー時の見た目変化
- いいね/登録アイコン以外(例: 低評価)の追加
- チャンネル登録アイコンを `channel_id` 取得不可時にAPIへ問い合わせて解決する(v1では `SearchResult.channel_id` が無ければ諦める)
