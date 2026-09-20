# コメント表示

再生中の動画のコメントを、Playing画面内で表示する。

## 取得

- コマンド: `yt-dlp -J --write-comments --extractor-args "youtube:max_comments=50,all,0,0;comment_sort=top" <URL>`
- 上位50件(いいね数順)のみ取得する。全件取得は動画によって数百万件に達し非現実的。
- 取得は`SearchResult.id`(既に手元にある)を使い、再生開始時にバックグラウンドタスクで行う。再生をブロックしない。
- タイムアウトは15秒(既存の検索用`YT_DLP_TIMEOUT`=30秒より短く、実測50件で5秒程度のため十分な余裕を持たせつつ長すぎない値)。
- コメント無効の動画(`comments`キーが無い/空配列)は「コメントはありません」扱いにする(エラーではない)。

## パーサー

既存の`search.rs`の`parse_lines`(改行区切りの複数JSON行を前提)は再利用できない。`-J`は単一動画の単一JSONオブジェクトを返すため、新規に単一JSONから`comments`配列を取り出す関数を書く。

## 状態管理

`thumbs.rs`の`ThumbState`(Pending/Ready/Failed)と同型のパターンを転用する。

- `CommentState`: Pending / Ready(Vec\<Comment\>) / Failed(String)
- `Comment`: author, text, like_count
- `Session.comments_task: Option<JoinHandle<()>>`
- `AppEvent::CommentsReady { nonce, video_id, comments: Result<Vec<Comment>, String> }`
- nonceは再生ごとに変わる値(既存の`player_nonce`とは別世代管理)で、古い再生のコメントが新しい再生に混ざらないようにする。

差し込み位置: `actions::start_playback`内、`enter_playback`成功直後にタスクをspawnする。

## 表示

`playing_areas`のレイアウト(映像/シークバー/アクション/ステータス/ヘルプの5段)は変えない。

- `o`キー(未使用と確認済み)でコメント表示on/offをトグルする。
- 表示on時は、既存の別ウィンドウモード時のplaceholder表示と同じ方式で`video_area`をコメント一覧(List)で上書きする。埋め込み映像(kitty)の上に重ね描画はしない。コメント表示中はkitty映像フレームの送出を止める(表示専念の一時状態)。
- 表示off時は通常の映像表示に戻る。表示中に届いたフレームは保留しておき、閉じた時点で貼り直す(再生を再開しなくても映像が戻る)。
- 上限50件は1件2行で100行あり、80x24の映像領域(枠の内側19行)には入らない。表示中の`↑↓`で1行、`PageUp`/`PageDown`で1画面ぶん送る。`↑↓`の音量は表示offのときだけ効く。
- ヘルプ行に`o:コメント`を追加(既存の`fit_hints`で80桁に収まらない場合は自動的に落ちる)。

## 対象外(v1)

- コメントの返信(スレッド)表示
- コメント件数の設定画面での変更(上限50固定)
- 非公開/年齢制限動画へのcookie引き渡し(必要になった時点で追加検討)
