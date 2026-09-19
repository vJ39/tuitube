# チャンネルID遅延解決

フィード系タブ(:ytrec等)の結果には`channel_id`が付かない(flat-playlist出力に含まれないことを実測確認済み)。`c`キー押下時に`channel_id`が無ければ、選択中の動画1本だけ非flat取得して解決してからチャンネルへ移動する。

## 取得

- 引数: `[url, "--dump-json"]`(flat-playlistを付けない単一動画取得)
- パースは既存の`search::parse_line`を再利用する(同じキー名`channel_id`/`title`/`id`を読むため新規パーサーは書かない)
- タイムアウトは既存の`comments.rs`の単一動画取得と同じ考え方で15秒程度
- 戻り値は`ChannelRef { id, uploader }`。履歴タブ(`:ythis`)の行は`uploader`も`channel`も持たないため、チャンネル名は取得した行から拾う

## 状態管理

`comments_task`/`comments_nonce`と同型のパターンを`Session`に追加する。

- `Session.channel_lookup_task: Option<JoinHandle<()>>`
- `Session.channel_lookup_nonce: u64`
- `AppEvent::ChannelLookupDone { nonce, video_id, result: Result<Option<ChannelRef>, String> }`(`Ok(None)`=取得はできたがchannel_idが無い動画、`Err`=取得失敗)

`video_id`で「今選択している動画への結果か」を照合する(`CommentsReady`と同じ発想)。一致しなければ結果を捨てる。

## 処理の流れ

`open_channel_with`の「`channel_id`が無ければ何もしない」分岐を、「`channel_lookup_task`をspawnしてreturn」に差し替える。`channel_id`があった場合の既存処理(`cancel_search`〜`start_channel_search_with`)は`enter_channel_with`として切り出し、直接呼ぶ経路とルックアップ完了後に呼ぶ経路の両方から使う。

`main.rs`の`handle_event`に`ChannelLookupDone`の分岐を追加する(`CommentsReady`と同型: nonce不一致なら破棄、`video_id`不一致なら破棄、`Ok(Some(_))`なら`enter_channel_with`、`Ok(None)`なら「このチャンネルへは移動できません」の一時通知、`Err`ならエラー表示)。中身は`apply_channel_lookup_with`に切り出し、テストから`YtDlp`を差し替えられるようにする。

引いている間にモードが変わることがあるため、`enter_channel_with`を呼ぶのは`app.mode`が`Mode::Results`か`Mode::Channel`のときだけ。再生中・設定画面・検索入力へ移っていたら結果を捨てる(再生中に移ると映像を止めるキーが無くなり、設定画面から移ると`close_settings`を通らず編集中の状態が残るため)。

チャンネル名は「取得した行の`uploader`→一覧の行の`uploader`→`channel_id`」の順で決める。

## 打ち切り

既存の`cancel_search`呼び出し箇所(タブ切替・タブ選択・チャンネルへ入る・チャンネルを出る・チャンネル内タブ切替・チャンネル内タブ選択、計8箇所)全てで`channel_lookup_task`も合わせて打ち切る。アプリ終了時も`search_task`/`thumbs_task`/`comments_task`と同じく`abort`する。

## 表示

`app.searching`(検索専用フラグ、既存のsearch_nonce管理と競合するため流用しない)とは別に、`set_notice`で「チャンネル情報を取得中…」を出す。引きは実測3〜4秒・上限15秒で`NOTICE_TTL`(3秒)より長いため、期限では消さずTTL無しで出す。nonceが一致する結果が届いた時点で、移るときも捨てるときも消す。失敗時は`set_error`。

## 対象外(v1)

- 選択している動画自体が変わった場合の明示的な打ち切り(既存の`move_selection`はcancel_searchを呼ばないため)。ただし`video_id`照合により、古いルックアップ結果が誤って新しい選択に適用されることは無い
