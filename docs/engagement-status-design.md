# いいね済み/登録済み状態の管理(#65/#66共通基盤)

いいね(#65で操作)・チャンネル登録(#65で操作)について、YouTube本家上の実際の状態を確認し、サムネイル(#66)・再生画面のアイコン(#65)へ反映する。

## 前提: YouTube Data API v3 の制約(公式ドキュメントで確認済み)

- `videos.list` の `myRating` は `id` と**同時指定できない**(排他フィルタ)。個別動画ごとに「いいね済みか」を確認する経路は無い。代わりに `myRating=like&maxResults=50` でページネーションし、**自分がいいねした動画IDの一覧を丸ごと取得**する(最大1000件までしか取れない制約あり)。
- `subscriptions.list` の `forChannelId` はカンマ区切りで複数チャンネルIDを渡せる(バッチ対応)。`mine=true` との組み合わせ挙動はドキュメントに明記がないため、実装時に実機で確認する。

この非対称性(動画側は一覧取得、チャンネル側は個別/バッチ確認)がそのまま設計に反映される。

## 状態モデル

新規モジュール `src/engagement.rs`。

```rust
pub struct EngagementCache {
    liked_videos: HashSet<String>,
    liked_last_confirmed: Option<SystemTime>,
    subscribed_channels: HashMap<String, bool>,
    channel_last_confirmed: HashMap<String, SystemTime>,
}
```

- `liked_videos`: 自分がいいねした動画IDの全体集合(API から丸ごと取得、またはこのアプリで操作した分をローカルで追加)。
- `subscribed_channels`: 問い合わせ済みチャンネルの登録有無(未確認のチャンネルはキーが存在しない=判定不能=バッジを出さない)。件数は `MAX_CHANNELS`(500件、resume.rs の RESUME_CAPACITY と同じ考え方)で上限を切り、超えたら `channel_last_confirmed` が最も古いものから捨てる(`remember_subscription`/`remember_channels` の末尾で毎回実行)。`liked_videos` は API 取得のたびに丸ごと入れ替える(`replace_liked`)ので上限は設けない。
- `*_last_confirmed`: API 取得・ローカル操作いずれかで確定した最終時刻。TTL 判定の基準。

フィールドは非公開で、次のメソッド経由で読み書きする。

| 用途 | メソッド |
| --- | --- |
| 印の判定 | `is_liked(video_id) -> bool` / `is_subscribed(channel_id) -> Option<bool>`(未確認は `None`) |
| 取得の要否 | `liked_needs_refresh(now, ttl) -> bool` / `channels_needing_refresh(&[String], now, ttl) -> Vec<String>`(渡された順・重複は1度だけ) |
| API 結果の反映 | `replace_liked(video_ids, now)` / `remember_channels(asked, subscribed, now)`(問い合わせたのに返らなかったIDは未登録として確定) |
| ローカル操作の反映 | `remember_like(video_id, liked, now)` / `remember_subscription(channel_id, subscribed, now)` |

## TTL(最終更新からの経過で再確認)

- 設定 `[engagement] ttl_secs`(既定 604800 = 1週間)。値は `engagement.rs` に持たず、`Duration` を引数で受け取る。
- 動画: `liked_last_confirmed` が無い、または `now - liked_last_confirmed > ttl` なら `liked_videos` を丸ごと再取得。
- チャンネル: 個別チャンネルごとに `channel_last_confirmed` を見て、TTL切れのIDだけ再確認対象にする。
- **ローカル操作による確定**(このアプリで実際にいいね/登録した瞬間)も `*_last_confirmed` を更新する。これにより「操作した直後にすぐAPIへ問い合わせ直す」無駄を防ぐ。
- ただし一覧を一度も取っていない状態でのいいね操作では `liked_last_confirmed` を更新しない。更新すると初回の一覧取得がTTLのあいだ止まり、他のいいね済み動画に印が出なくなる。
- 最終確認時刻が未来(時計の巻き戻し)の場合はTTL切れとして扱う。そうしないと再確認が止まったままになる。

## API呼び出し

`oauth.rs` に追加(既存の `rate_request`/`subscribe_request` と同じ `Request` を組むだけの関数):

```rust
pub fn list_liked_videos_request(access_token: &str, page_token: Option<&str>) -> Request; // GET videos?part=id&myRating=like&maxResults=50[&pageToken=...]
pub fn list_subscriptions_request(access_token: &str, channel_ids: &[String]) -> Request;  // GET subscriptions?part=snippet&mine=true&forChannelId=<csv>
```

既存の `Action`/`run`/`call_api` は「1アクションを実行して文言を返す」形(#65のいいね/登録操作用)。状態確認は「複数ページ・複数チャンネルを辿ってキャッシュを更新する」別種の非同期処理なので、別系列の関数として新設する(`oauth::refresh_liked_videos`, `oauth::refresh_subscriptions`)。既存の `Action` enum・`call_api` は変更しない。

## レート制限・並列処理

設定 `[engagement] max_concurrent_requests`(既定 3)。

- 動画いいね一覧: ページネーションは直列(1000件上限・ページ50件なので最大20リクエスト)。前のページのトークンが無いと次を呼べないため並列化できない。
- チャンネル登録確認: TTL切れの channel_id 一覧を50件(`forChannelId` の上限が公表されていないため安全側)ずつのチャンクに分け、`max_concurrent_requests` を上限に `tokio::spawn` で同時実行する。1チャンクでも失敗したらチャンネル側は丸ごと捨てる。一部だけ反映すると、返らなかったIDを未登録として確定させてしまうため。

## トリガー

- 検索結果が表示された(`SearchDone`)ときに `actions::start_engagement` をキックする(サムネイル取得の既存パターン=`start_thumbnails_with` と同型)。取り直しが要るIDが無ければ何もしない。
- 印は飾りなので、置き場が無い・保存済みトークンが無い・問い合わせが失敗したときは通知もエラーも出さずに黙って終える。トークンが無い場合もブラウザは開かない。
- 結果は `AppEvent::EngagementReady` で返し、`nonce` が今の検索と一致するときだけ控えへ写す。いいね一覧が取れなかったときは `liked_videos = None`、チャンネル側が取れなかったときは `asked_channels` を空で返し、その分の控えは触らない。
- `apply_oauth_done` の `Ok` 分岐で、操作した動画/チャンネルIDを即時 `liked_videos`/`subscribed_channels` へ反映し `last_confirmed` を更新、`app.thumbs.mark_dirty()` を呼ぶ。本家で確定しているので、次の問い合わせを待たずに印を出す。

## サムネイルへの重ね描き(#66)

`present_thumbs` 内、`rgb::encode_image` 呼び出しの直後に、いいね済み/登録済みのセルへ小さな別Kitty配置を追加送信する。RgbImage自体は変更しない(既存のデコード/キャッシュ経路を汚さない)。印の組み立ては `badge.rs` に置く。

- 出す印は `badge::badges_for` で決める。いいね済み(`is_liked`)なら赤、`channel_id` が登録済み(`is_subscribed` が `Some(true)`)なら緑。未確認(`None`)は出さない。両方なら いいね → 登録 の順に並べる。
- 位置はサムネイルの左上から右へ1セルずつ。サムネイルの幅(`Placement.cols`)に収まらない分は隣のセルを汚すので出さない。
- 印の画像はセルの縦横比に寄せて作り、1辺24pxで打ち止める(端末が大きなセル寸法を報告しても送出量が伸びないため)。暗い1px枠を付け、枠だけで埋まる寸法(3px未満)では塗りだけにする。
- 印はサムネイルより後に送る(画像は後から貼った方が上に出る)。`[engagement] enabled = false` のときは送らない。

list表示(`grid::LayoutMode::List`)はサムネイルを描かず Kitty 画像を使わないため、上記の重ね描きが乗らない。代わりに `draw_list` の行テキストに `badge::badges_for` の結果を記号(`Badge::symbol()`、♥/＋)として接頭辞で足す(印が無ければ何も足さない)。

## 再生画面のアイコン(#65)

`playing_areas` に新規の1段を足し、いいね/登録をクリック可能なラベルとして表示する。`EngagementCache` を見て、済んでいれば見た目を変える(例: 塗り色を変える)。既存の `l` キー(いいね)はそのまま残し、クリックでも同じ効果を出す。チャンネル登録は `Playback` に `channel_id: Option<String>` を追加し、`start_playback` 時に選択した `SearchResult.channel_id` を引き継ぐ(チャンネルタブ経由の行で無ければアイコン自体を出さない)。詳細は別途 `docs/playing-action-icons-design.md` に書く。

## 永続化

`$XDG_CACHE_HOME/tuitube/engagement.json`(無ければ `$HOME/.cache/tuitube/`)に `liked_videos`/`subscribed_channels`/`*_last_confirmed` を保存する。プロセス再起動のたびに1週間分のTTLが失われないようにする(サムネイルのディスクキャッシュと同じ考え方)。

- 入口は `engagement::load()`/`engagement::save(&cache)`(パスを渡す `load_from`/`save_to` もテスト用に公開)。起動時に `load` して `App` へ渡し、終了時に `save` する。書けなくても終了は止めない(次の起動で取り直すだけ)。
- 時刻はUNIX秒で持つ。`liked_videos` は書く前に並べ替える(差分が読めるようにするため)。
- 読めない・壊れているファイルは空として扱い、起動を止めない。控えが空のときはファイルを作らない。
- 書き込みは `<名前>.json.tmp.<pid>` へ書いてから `rename` で置き換える。窓を2つ開けても、片方の書きかけをもう片方が本番の位置へ移さない。

## 設定ファイル拡張

```toml
[engagement]
enabled = true
ttl_secs = 604800
max_concurrent_requests = 3
```

- `enabled`: false なら印を出さず、状態の問い合わせもしない。
- `ttl_secs`: 60..=2592000(30日)。範囲外は丸めて通知を出す。
- `max_concurrent_requests`: 1..=10。範囲外は丸めて通知を出す。

## 対象外(v1)

- ディスライク状態の表示
- 検索結果一覧以外(コメント欄の投稿者チャンネル等)への状態表示
- 手動での即時再確認キー
- `forChannelId` のバッチ上限を超える件数の一括処理の最適化(超えたら単純にチャンクを増やすだけ)
