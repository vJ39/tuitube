# YouTube OAuth連携(チャンネル登録・いいね)

`subscriptions.insert`(チャンネル登録)と`videos.rate`(いいね)は書き込み系のためcookie方式では使えず、OAuth 2.0が必要。既存の`docs/google-account-cookies-design.md`が「OAuthは使わない」としているのは読み取り系(視聴履歴・おすすめ)の話で、書き込み系の本機能とは矛盾しない。

外部プロセスを呼ぶ既存の統一パターン(`Fetcher`/`YtDlp`トレイト+本番実装+テスト用フェイク)をそのまま踏襲する。新規の重い依存(reqwest、rand、sha2等)は追加せず、curl/openssl/openという既に環境にある外部コマンドで完結させる。

## 前提

- Client ID/Secretは`$XDG_CONFIG_HOME/tuitube/oauth_client.toml`(未設定時`~/.config/tuitube/oauth_client.toml`)に保存済み(リポジトリ外、パーミッション600)。置き場の決め方は`src/settings.rs`の`config_path`と揃える
- スコープ: `https://www.googleapis.com/auth/youtube.force-ssl`

## OAuthフロー(PKCE、installed app)

1. `code_verifier`: `/dev/urandom`から32バイト読み、`rgb::base64_into`で符号化した後、`+`→`-`・`/`→`_`・`=`除去でbase64url化する(新規のbase64実装は書かない)
2. `code_challenge`: `code_verifier`を`openssl dgst -sha256 -binary`に渡しSHA256を計算(外部プロセス、既存のcurl/yt-dlp呼び出しと同じ流儀)、結果を同じbase64url変換に通す。`code_challenge_method=S256`
3. 認可URL(`https://accounts.google.com/o/oauth2/v2/auth`)を組み立て、`open`コマンド(macOS)で既定のブラウザを開く
4. ローカルに`tokio::net::TcpListener`を`127.0.0.1:0`(空きポート自動割当)で立て、リダイレクト(`http://127.0.0.1:<port>/callback?code=...&state=...`)の最初の1行(`GET /callback?...`)だけをパースして`code`/`state`/`error`を取り出す。取得後は「このタブは閉じて構いません」という簡易HTMLを1回返してリスナーを閉じる。待ち受けは5分で打ち切る(ブラウザで認可をやめてもポートを掴んだままにしないため)
5. `state`はCSRF対策として`code_verifier`と同様に乱数生成し、コールバックの値と一致するか確認する
6. 認可コードを`https://oauth2.googleapis.com/token`へform-urlencoded POST(`grant_type=authorization_code`, `code`, `client_id`, `client_secret`, `redirect_uri`, `code_verifier`)し、`access_token`/`refresh_token`を取得

## トークン保存

`oauth_client.toml`と同じディレクトリの`oauth_token.toml`(リポジトリ外)に`refresh_token`を保存する。ファイルはパーミッション600で作成する。作ってから`chmod`すると、その間はumask(既定022)のぶん他の利用者から読めてしまうため、`OpenOptions::mode(0o600)`で作成時にモードを指定する。`access_token`は短命(約1時間)なのでメモリ内キャッシュのみとし、ファイルには保存しない。

`refresh_token`が保存済みなら、API呼び出し前に`grant_type=refresh_token`で新しい`access_token`を都度取得する(有効期限管理は行わず、呼び出しのたびに取り直す。頻度が低い機能のため簡潔さを優先する)。

## API呼び出し

curlでJSONレスポンスを変数として読む新規ヘルパーを追加する(既存`fetch.rs`の`Fetcher`はファイル書き込み専用のため流用しない、同じ「外部プロセス+トレイト+フェイク」構成で新設する)。`-w "\n%{http_code}"`で末尾にステータスコードを付与し、`--fail`は使わない(エラー時のJSON本文を読むため)。

`client_secret`・`refresh_token`・認可code・`code_verifier`・`access_token`はコマンドライン引数に置かない(同じ利用者の他プロセスから`ps`で読める)。`--config -`で設定ファイルとしてstdinから渡し、argvにはURLと秘密でないヘッダだけを並べる。

curlは接続できないときも`-w`の書式をstdoutへ出し、`%{http_code}`が`000`になる。ステータス`0`と、curlの異常終了(接続拒否=7、`--max-time`超過=28)は、stdoutの有無によらず通信失敗として扱う。ここを通すとYouTubeへ1バイトも届かないまま「チャンネル登録しました」が出る。

- チャンネル登録: `POST https://www.googleapis.com/youtube/v3/subscriptions?part=snippet` + `Authorization: Bearer <token>` + JSON body `{"snippet":{"resourceId":{"kind":"youtube#channel","channelId":"<id>"}}}`
- いいね: `POST https://www.googleapis.com/youtube/v3/videos/rate?id=<video_id>&rating=like` + `Authorization: Bearer <token>` + 空body

既に登録済み/いいね済みの場合のエラー(重複)は失敗として扱わず、成功と同じ通知にする。

## 操作

- `Mode::Channel`で`s`キー: 表示中のチャンネルを登録する(トークンが無ければ上記OAuthフローを開始してから実行)
- `Mode::Playing`で`l`キー: 再生中の動画にいいねする(同上)
- 処理中は`set_notice`で「チャンネル登録の認証中…」等を表示。既存の`channel_lookup_task`と同型のnonce+`JoinHandle`管理を`Session`に追加する(`oauth_task`)
- 検索の打ち切り(`cancel_search`)は`oauth_task`も畳む。知らせを消しながら認証待ちだけ残すと、待っていることが画面から見えないまま中断もできなくなる

## 対象外(v1)

- 登録解除・いいね取り消し(トグル)。一方向の操作のみ
- 事前の登録済み/いいね済み状態の確認(`subscriptions.list`/`videos.getRating`の追加呼び出しは行わない)
- Linux(`xdg-open`)対応。macOSの`open`コマンドのみ
- アクセストークンの有効期限管理・キャッシュ(呼び出しのたびに`refresh_token`から取り直す)

## 検証について

実装・テストは全てフェイク(curl/open/TCPリスナーの差し替え)で行い、実際のGoogleアカウント・実ブラウザ・実APIへは実装時にアクセスしない。認可フローを実際に通して本物のチャンネル登録/いいねを行う検証は、ユーザー自身が実施する(実際にアカウントへ変更が入るため)。
