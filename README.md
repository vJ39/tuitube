# tuitube

端末の中で YouTube の動画を探して見るための TUI アプリ。検索と一覧の取得に [yt-dlp](https://github.com/yt-dlp/yt-dlp)、再生に [mpv](https://mpv.io/) を使い、映像は Kitty graphics protocol で端末の中に描く。

## できること

- キーワード検索と、カテゴリのタブ(音楽・ゲーム・ニュース・アニメ・スポーツ。設定で差し替えられる)
- ログインした YouTube のおすすめ・履歴・登録チャンネル・後で見るの一覧(ブラウザの cookie を使う)
- サムネイル付きの格子表示と 1 行ずつのリスト表示
- 端末の中での再生(Kitty graphics protocol の画像か文字ブロック)と、mpv の別ウィンドウでの再生
- シーク(キー・マウス)、倍速、字幕、コメント、続きからの再生、再生しながらの一覧の操作
- チャンネル(動画・ショート・ライブ配信)とプレイリストの閲覧、動画とプレイリストの URL からの再生
- いいね、チャンネル登録、「tuitube」プレイリストへの保存(YouTube Data API を使う)
- 動画・音声のダウンロード
- 選んだ動画・チャンネルを一覧に出さない設定

## 必要なもの

- macOS。クリップボードへのコピーに `pbcopy`、ブラウザを開くのに `open` を使う
- Rust(ビルド用。1.94.1 で確認。依存クレートが 1.88 以降を要求する)
- mpv(0.41.0 で確認)
- yt-dlp(2026.08.19 で確認)と、yt-dlp が YouTube の取得に使う JavaScript の実行環境(deno など)
- curl と openssl(macOS に入っているもので動く)
- ダウンロードを使うなら ffmpeg(yt-dlp が映像と音声の結合と mp3 への変換に使う)
- Kitty graphics protocol を使える端末(iTerm2 3.6.11 で確認)。使えない端末では、設定ファイルで再生の表示を `text`、一覧を `list` にする

## ビルドと起動

```sh
cargo build --release
./target/release/tuitube
```

起動すると検索欄が開き、cookie 連携があれば「おすすめ」、無ければ先頭のカテゴリを自動で表示する。語句を入れて Enter で検索し、一覧で Enter を押すと再生が始まる。

## 画面と操作

検索欄と結果一覧が基本の画面で、結果一覧からチャンネル・プレイリスト・ダウンロード・設定の各画面へ移る。画面の最下行に、その画面で使えるキーの案内が出る(幅が足りないと後ろの方が省かれる)。

どの画面でも `Ctrl+C` ですぐ終了する。一覧で `q`、または結果が無いときに検索欄で `Esc` を押すと終了の確認が出て、`y` か `Enter` で終了、`n` か `Esc` で戻る。

### 検索欄

| キー | 動作 |
| --- | --- |
| 文字 | 入力。選択範囲があれば置き換える |
| `Enter` | 検索する。動画の URL ならその動画を開いてすぐ再生し、プレイリストの URL ならその中身を開く |
| `Tab` / `Shift+Tab` | カテゴリのタブを切り替える |
| `←` `→` `Home` `End` | カーソル移動。`Shift` を付けると選択範囲を広げる |
| `Backspace` | 選択範囲か、カーソルの前の 1 文字を消す |
| `Ctrl+A` | 全部選ぶ |
| `Esc` | 結果一覧へ戻る。結果が無ければ終了の確認 |
| `Ctrl+S` | 設定画面 |
| `Ctrl+P` | 自分のプレイリストの一覧 |
| `Ctrl+B` | 裏で再生中なら再生画面へ戻る |
| クリック | 検索欄の内側はカーソル移動、枠(上下の線)を押すと `Enter` と同じく検索を実行する。タブの行ではそのタブを選ぶ |

検索欄の枠は、検索欄にいる間は明るい色(シアン)、ほかの画面にいる間は暗い色になる。

`:ytrec`(おすすめ)・`:ythis`(履歴)・`:ytsubs`(登録チャンネル)・`:ytwatchlater`(後で見る)と入れて Enter を押すと、ログインした YouTube の一覧を開く。同じものが既定のタブにも入っている。どれも cookie 連携が要る。

受け付ける URL は `youtube.com`(`www.`・`m.`・`music.` 付きも)と `youtu.be` の、動画(`watch?v=`・`youtu.be/`・`shorts/`・`live/`・`embed/`)とプレイリスト(`playlist?list=`)。それ以外の文字列は検索語として扱う。

### 結果一覧

| キー | 動作 |
| --- | --- |
| `↑` `↓` `←` `→` | 選ぶ。格子表示では端で止まり、リスト表示では上下の端で反対側へ回る |
| `Enter` | 再生する |
| `Tab` / `Shift+Tab` | カテゴリのタブを切り替える。一度読んだタブは取り直さない |
| `r` | 今のタブを取り直す |
| `m` | もっと見る。末尾で `↓` を押しても同じ(検索のタブで、まだ続きがあるときだけ) |
| `c` | 選んでいる動画のチャンネルを開く |
| `h` | 選んでいる動画を隠す |
| `a` | 選んでいる動画を「tuitube」プレイリストに保存する |
| `l` | 選んでいる動画にいいねする |
| `u` | 選んでいる動画のチャンネルを登録する(行が channel_id を持つときだけ) |
| `d` | ダウンロード画面 |
| `p` | 自分のプレイリストの一覧 |
| `v` | 格子表示とリスト表示を切り替える(設定ファイルに保存する) |
| `b` | 裏で再生中なら再生画面へ戻る |
| `/` / `Esc` | 検索欄へ |
| `S` / `Ctrl+S` | 設定画面 |
| `q` | 終了の確認 |
| クリック | 格子表示の動画を押すと再生する。タブの行ではそのタブを選ぶ |

一覧は公開日の新しい順に並べる。チャンネルとプレイリストの中身も同じで、ログイン連動の一覧だけは YouTube の並びのまま。「もっと見る」は、今の件数に `[search] limit` の件数を足して取り直す(上限 1000 件)。

### チャンネル

結果一覧・プレイリストの中身で `c` を押すと開く。操作は結果一覧とほぼ同じで、`c` と `p` は使えない。違うのは次のキー。

| キー | 動作 |
| --- | --- |
| `Tab` / `Shift+Tab` | 動画・ショート・ライブ配信のタブを切り替える |
| `s` | このチャンネルを登録する |
| `l` | 選んでいる動画にいいねする |
| `h` | このチャンネルごと隠して、開く前の画面へ戻る |
| `r` | 今のタブを取り直す |
| `/` | 開く前の画面へ戻る |
| `Esc` | 検索欄へ直接戻る(間の画面は経由しない) |

各タブは 50 件まで。「もっと見る」は無い。

### プレイリスト

検索欄の `Ctrl+P` か結果一覧の `p` で、自分のプレイリストの一覧を開く(cookie 連携が要る)。`↑` `↓` で選び、`Enter` で中身を開く。`/` で戻り、`Esc` で検索欄へ直接戻る。

中身の画面の操作は結果一覧とほぼ同じで、タブの切り替え・「もっと見る」・`p`・クリックでの再生が無い(`l`・`u` でのいいね・登録はできる)。`h` はその動画だけを隠す。`/` で一覧へ戻り、`Esc` は(一覧を経由せず)検索欄へ直接戻る。URL で開いたプレイリストからは `/` でも検索側へ戻る。中身は 50 件まで。

### 再生画面

| キー | 動作 |
| --- | --- |
| `Space` | 一時停止と再開 |
| `←` `→` | 5 秒戻す・進める |
| `↑` `↓` | 音量を 5 ずつ上げ下げする。コメントを出している間はコメントを 1 行送る(`PageUp` `PageDown` で 1 画面) |
| `[` `]` | 速度を 0.1 倍ずつ下げ上げする(0.1〜4.0 倍) |
| `Backspace` | 速度を 1 倍に戻す |
| `w` | 表示を切り替える(端末内の画像 → 文字ブロック → 別ウィンドウ → 端末内の画像)。設定ファイルの `display.mode` に保存する |
| `s` | 字幕を出す・消す |
| `o` | コメントを出す・消す |
| `c` | 動画の URL をクリップボードへコピーする |
| `l` | いいね |
| `u` | チャンネル登録(チャンネルが分かっている動画だけ) |
| `a` | 「tuitube」プレイリストに保存する |
| `d` | ダウンロード画面 |
| `b` | 再生を続けたまま一覧へ戻る |
| `q` / `Esc` | 再生を止めて一覧へ戻る |

マウスでは、シークバーを押すかドラッグするとその位置へ飛ぶ(離したときに 1 回)。シークバーの上にポインタを置くと、その位置の時刻が出る。ライブ配信など長さの分からない動画では飛ばない。アクション行の「♥いいね」「＋登録」「★保存」は押しても使える。

速度は次の動画にも引き継ぐ。字幕を出すかどうかも次の動画に引き継ぐ。

途中でやめた動画は、次に再生したときに続きから始まる。最後の 5% より手前でやめたときに位置を覚え、60 秒未満の動画と、10 秒より手前でやめたときは覚えない。

コメントはいいねの多い順に 50 件まで出す。返信は出ない。

別ウィンドウで再生している間、mpv のウィンドウが前面にあるときのキーは mpv が受ける。

### 再生しながら一覧を見る

再生画面で `b` を押すと、再生を続けたまま一覧へ戻る。端末内の表示では、一覧の右上に小さな映像が出る。一覧の `b`(検索欄では `Ctrl+B`)で再生画面へ戻る。別ウィンドウで再生しているときは、一覧に映像は出ず、mpv のウィンドウで再生が続く。

### ダウンロード画面

結果一覧・チャンネル・プレイリストの中身・再生画面で `d` を押すと開く。

| キー | 動作 |
| --- | --- |
| `↑` `↓` | 保存先・ファイル名・形式の行を選ぶ |
| 文字・`←` `→`・`Backspace` | 保存先とファイル名の編集 |
| `←` `→` `Enter` `Space`(形式の行) | 動画と音声(mp3)を切り替える |
| `Enter` | ダウンロードを始めて元の画面へ戻る |
| `Esc` | 何もせず元の画面へ戻る |

保存先は `[download] dir`、無ければ `~/Downloads` から始まる。ファイル名は動画のタイトルから始まり、拡張子は yt-dlp が付ける。同時に進めるのは 1 本までで、新しく始めると前のものは打ち切る。

### 設定画面

一覧で `S` か `Ctrl+S`、検索欄で `Ctrl+S` を押すと開く。設定ファイルの一部の項目をこの画面で変えられる。

| キー | 動作 |
| --- | --- |
| `↑` `↓` | 項目を選ぶ |
| `←` `→` | 値を変える |
| `Enter` / `Space` | オンとオフを切り替える。選択肢と数値では `→` と同じ |
| `0`〜`9` | 数値の項目に直接打ち込む(`Enter` で確定、`Esc` で取り消し) |
| `s` | 設定ファイルに保存する(画面は開いたまま) |
| `Esc` / `q` | 保存していない変更を捨てて戻る |

変えられるのは `display.mode`・`display.quality`・`fps_cap`・`subtitles.enabled`・`window.ontop`・`search.layout`・`search.limit`・`search.timeout_secs`・`search.cache_enabled`・`search.cache_ttl_secs`・`thumbnails.enabled`・`thumbnails.max_cached`・`thumbnails.timeout_secs`・`download.debug`。ほかの項目は設定ファイルを直接書き換える。`display.mode` の変更は次の起動から効く(再生中は `w` で切り替える)。

## ログイン連携

### ブラウザの cookie

設定ファイルの `[cookies] browser` にブラウザを書くと、yt-dlp がそのブラウザの cookie を読んでログインした状態で検索・再生する。書けるのは `chrome`・`safari`・`firefox`・`chrome:Profile 1` のような yt-dlp の `--cookies-from-browser` の指定。

- Safari を使うときは、端末アプリにフルディスクアクセスを許可する(許可が無いと読めない)
- 環境変数 `TUITUBE_COOKIES_FROM_BROWSER` を付けて起動すると、設定ファイルの `browser` と `file` より優先する。`none` でその回だけ連携を切る
- cookie を読めなかったときは、連携を切って検索し直す。以後その起動中は連携を戻さない

### cookies.txt

ブラウザから読めない環境では、書き出した cookies.txt(Netscape 形式)を `[cookies] file` で渡す。`browser` と両方書くと `file` を使う。yt-dlp が終了時にこのファイルへ cookie を書き戻すので、書き込みも許可しておく。

Chrome の cookie から作る手順:

```sh
TMPPROFILE="$(mktemp -d)/Default"
mkdir -p "$TMPPROFILE"
cp "$HOME/Library/Application Support/Google/Chrome/Default/Cookies" "$TMPPROFILE/Cookies"

yt-dlp --cookies-from-browser "chrome:${TMPPROFILE}" \
  --cookies "$HOME/.config/tuitube/cookies.txt" \
  --flat-playlist --dump-json --playlist-end 1 ":ytwatchlater"
```

- Cookies ファイルだけを別の場所へ写してから読ませるのは、Chrome の拡張機能が持つ別の Cookies ファイルを yt-dlp が読んでしまうことがあるため(プロファイルの下で一番新しい `Cookies` を選ぶ)
- `-v` を付けると `Extracted N cookies from chrome` が出る。500 件前後なら正しいファイルを読めている。30 件ほどしか無ければ別のファイルを読んでいる
- プロファイルが `Default` でなければ、パスの `Default` をそのプロファイル名にする
- ログインの cookie には期限がある。ログインが要る一覧が開けなくなったら、もう一度作る

### いいね・チャンネル登録・保存

いいね・チャンネル登録・「tuitube」プレイリストへの保存は YouTube Data API を使う。使う前に次を用意する。

1. Google Cloud のプロジェクトで YouTube Data API v3 を有効にし、OAuth クライアント(種類はデスクトップ アプリ)を作る
2. `~/.config/tuitube/oauth_client.toml` にクライアントの ID とシークレットを書く

   ```toml
   client_id = "…"
   client_secret = "…"
   ```

3. tuitube で初めて `l`・`u`・`a` などを押すとブラウザが開くので、許可する。tuitube は `http://127.0.0.1:<空いているポート>/callback` で応答を受け取り、refresh token を `~/.config/tuitube/oauth_token.toml`(権限 600)に保存する。次からはブラウザを開かない

許可が切れて操作が失敗するようになったら、`oauth_token.toml` を消すと次の操作で許可からやり直す。

YouTube の「後で見る」には API から足せないので、保存先は自分のアカウントに作る「tuitube」という非公開のプレイリストにする(無ければ最初の保存のときに作る)。保存した動画はプレイリストの一覧から開ける。

一覧のサムネイルと再生画面には、いいね済み・登録済みの印が出る(格子表示は色付きの小さな画像、リスト表示は赤い♥・緑の＋)。状態は OAuth を用意しているときに YouTube へ問い合わせて控え、`[engagement] ttl_secs`(既定 1 週間)ごとに取り直す。

## 設定ファイル

`~/.config/tuitube/config.toml`(`$XDG_CONFIG_HOME` があればその下の `tuitube/config.toml`)。無ければ起動時に次の内容で作る。書き換えたら tuitube を起動し直す。

```toml
# tuitube の設定。編集後は tuitube を再起動する。
# このファイルは tuitube が書き直すことがあり、自分で書いたコメントは残らない。

[display]
# 再生開始時の表示。"embedded" = TUI 内に埋め込み (Kitty graphics protocol)、"text" = 文字ブロック(Kitty 非対応端末向け)、
# "window" = mpv の別ウィンドウ。再生中は w で順に切り替え。
mode = "embedded"
# 埋め込み表示の細かさ。"low" / "medium" / "high" / "native"。
# 変わるのは映像の細かさと端末へ送るデータ量。mpv のデコード負荷は変わらない。
quality = "medium"
# 細かさをピクセル数で直接指定したいとき (quality より優先)。例: 640*360 = 230400
# max_frame_pixels = 230400

[playback]
# 埋め込み・テキスト表示の fps 上限。端末へ送るフレーム数を抑える。0 で制限なし。別ウィンドウには適用しない。
fps_cap = 15

[subtitles]
# YouTube の自動生成字幕を要求するか。false のときは再生中に s を押しても出せない。
enabled = true
# 取得する字幕の言語。カンマ区切りで複数書ける。
# 例: "ja-orig" (原語の文字起こし) / "ja" (自動翻訳) / "ja-orig,ja"
# 複数書いたときにどれを出すかは mpv が決める。先頭が選ばれるとは限らない。
# yt-dlp の sub-langs と mpv の --slang に同じ値を渡すので、言語コード以外 ("all" や正規表現) は書けない。
lang = "ja-orig,ja"

[window]
# 別ウィンドウ時の mpv オプション。値はそのまま mpv に渡る。
# vo = "gpu-next"
# autofit = "640x360"
# geometry = "50%+0+0"
# ontop = false
# fullscreen = false
# focus_on = "never"
# title = "tuitube"

[cookies]
# YouTube のログイン連携。yt-dlp の --cookies-from-browser に渡すブラウザ指定 (BROWSER[+KEYRING][:PROFILE][::CONTAINER])。
# 例: "chrome" / "safari" / "firefox" / "chrome:Profile 1"。ここに入るのはブラウザ名だけで、cookie の値は保存しない。
# 環境変数 TUITUBE_COOKIES_FROM_BROWSER があればそちらが優先 ("none" で一時的に連携を切る)。
# browser = "chrome"
# ブラウザもキーチェーンも無い環境向けに、エクスポートした cookies.txt (Netscape 形式) を渡す指定。yt-dlp の --cookies に渡す。
# browser と両方書いたときは file を使う。使えないファイルを指した場合は cookie 連携なしで起動する。
# yt-dlp が終了時にこのファイルへ cookie を書き戻すので、読み取り専用にせず書き込みも許可しておく。
# file = "~/.config/tuitube/cookies.txt"

[mpv]
# mpv にそのまま渡す追加引数。
# extra_args = ["--hwdec=videotoolbox-copy"]

[search]
# 検索結果の見せ方。"grid" = サムネイル付きの格子、"list" = 1 行ずつのリスト。
# Kitty graphics protocol 非対応の端末では "list" にする。
layout = "grid"
# 1 回の検索で取る件数。1..=1000。
limit = 10
# yt-dlp 1 回ぶんを待つ上限秒数。5..=300。
# cookie を読めずに出し直すときは、最大 2 回ぶん待つ。
# limit を大きくすると検索に時間がかかる。「検索がタイムアウトしました」が出るなら延ばす。
timeout_secs = 30
# true にすると、同じ検索を cache_ttl_secs の間は取り直さない。
# 控えはメモリ上だけなので、アプリを終了すると消える。r を押せば必ず取り直す。
cache_enabled = false
# 控えを使い回す秒数。10..=3600。
cache_ttl_secs = 300

[thumbnails]
# false にすると取得しない。格子のまま枠だけが出る。
enabled = true
# 取得したサムネイルの置き場。既定は $XDG_CACHE_HOME/tuitube/thumbs。
# cache_dir = "~/.cache/tuitube/thumbs"
# 起動時にここまで間引く枚数。
max_cached = 500
# 1 枚あたりのダウンロード上限秒数。1..=120。
timeout_secs = 10

[engagement]
# いいね済み/登録済みの印。false にすると印を出さず、状態の問い合わせもしない。
enabled = true
# 印を取り直すまでの秒数。60..=2592000 (既定は 1 週間)。
ttl_secs = 604800
# 状態確認を同時に投げる本数。1..=10。
max_concurrent_requests = 3

[download]
# ダウンロード画面 (d) の保存先。空欄のまま使うと $HOME/Downloads (無ければ空欄) から始まる。
# dir = "~/Movies"
# true にすると、yt-dlp への実引数・終了コード・標準出力/エラーを
# $XDG_CONFIG_HOME/tuitube/download-debug.log (無ければ $HOME/.config/tuitube/...) へ追記する。
# ダウンロードがうまく動かないときの調査用。使い終わったら false に戻す (ログは増え続ける)。
debug = false

# カテゴリタブ。書いた場合は既定の一覧を丸ごと置き換える。
# 先頭の「すべて」タブは常に自動で付くので書かない。
# query が次のキーワードならログイン連動の一覧のタブになる: :ytrec = おすすめ / :ythis = 履歴 / :ytsubs = 登録チャンネル / :ytwatchlater = 後で見る
# 既定の一覧にはこの 4 つも入っているので、書き換えるときは残す分も並べる。
# [[categories]]
# label = "音楽"
# query = "音楽"
# [[categories]]
# label = "おすすめ"
# query = ":ytrec"
```

`w`(表示)と `v`(一覧の形)で切り替えたときは、該当する 1 行だけを書き換える。設定画面で保存したときはファイル全体を書き直すので、自分で書いたコメントは消える。

環境変数:

- `TUITUBE_FPS_LIMIT`: `fps_cap` をその回だけ変える。`0` か `unlimited` で制限なし
- `TUITUBE_COOKIES_FROM_BROWSER`: `[cookies]` をその回だけ変える。`none` で連携を切る

環境変数で変えた値は、設定画面で保存しても設定ファイルには書かない。

## 保存するファイル

| 場所 | 中身 |
| --- | --- |
| `~/.config/tuitube/config.toml` | 設定 |
| `~/.config/tuitube/hidden.toml` | 隠した動画とチャンネル |
| `~/.config/tuitube/resume.toml` | 途中でやめた動画の再生位置(500 件まで) |
| `~/.config/tuitube/oauth_client.toml` | OAuth クライアント(自分で置く) |
| `~/.config/tuitube/oauth_token.toml` | refresh token |
| `~/.config/tuitube/download-debug.log` | `[download] debug = true` のときのダウンロードの記録 |
| `~/.cache/tuitube/thumbs/` | サムネイル(起動時に `max_cached` 枚まで減らす) |
| `~/.cache/tuitube/engagement.json` | いいね済み・登録済みの控え |

`~/.config` は `$XDG_CONFIG_HOME`、`~/.cache` は `$XDG_CACHE_HOME` があればそちらになる。

隠した動画を戻す画面は無い。`hidden.toml` から該当する項目を消す。

```toml
[[videos]]
id = "動画 ID"
title = "表示用のタイトル"

[[channels]]
id = "チャンネル ID"
title = "表示用のチャンネル名"
```

## できないこと

- 文字ブロックの表示では字幕が出ない(mpv がこの表示では字幕を描かない)
- 字幕の言語を再生中に選び直す
- いいねと登録の取り消し
- 起動をまたいだ「保存済み」の印(起動し直すと保存の印は消える)
- 通常の検索結果・フィード・プレイリストでのショートの印(チャンネルのショートのタブでだけ出る)
- リスト表示でのクリック再生
- プレイリストの作成・並べ替え・削除(「tuitube」を自動で作るのを除く)
- 検索欄の文字のコピーと貼り付け(端末の貼り付けは使える)
- 再生位置と隠した項目の一覧を見る画面

## うまく動かないとき

- cookie 連携中に再生や一覧の取得だけ失敗する: yt-dlp の設定ファイル(`~/.config/yt-dlp/config`)に `--remote-components ejs:github` を足す。yt-dlp 2026.08.19 では、YouTube の確認を解くためのスクリプトを取りに行くのにこの指定が要った
- 「検索がタイムアウトしました」と出る: `[search] timeout_secs` を延ばす。`limit` を大きくすると検索に時間がかかる
- ダウンロードが失敗する: `[download] debug = true` にすると、yt-dlp への引数と出力を `download-debug.log` に残す。調べ終わったら `false` に戻す
- 画像が出ない: 端末が Kitty graphics protocol に対応していない。`[display] mode = "text"` と `[search] layout = "list"` にする

設計の説明は [docs/design.md](docs/design.md) にある。
