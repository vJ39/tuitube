自分の YouTube/Google アカウントのログイン状態を、検索結果とおすすめ等のフィード、そして再生に反映するための設計。方式は yt-dlp の `--cookies-from-browser`(ブラウザが保持している YouTube のログイン cookie を yt-dlp が読む)。YouTube Data API v3 には視聴履歴・おすすめの公式エンドポイントが無いため OAuth は使わない。実装は TDD(Red→Green)で進める前提で、外部プロセス(yt-dlp・mpv・ブラウザ・キーチェーン)を伴わない純粋関数の単位を先に切り出す。

対象バージョン: yt-dlp 2026.08.19 / mpv 0.41.0 / macOS 14.8.2 / ratatui 0.29。行番号の参照はコミット `7976a89` 時点。

## 0. 合意済み仕様

1. yt-dlp の `--cookies-from-browser <spec>` 方式を使う(非公式・スクレイピング相当だが現実的)
2. 検索結果にログイン状態を反映する(パーソナライズ)
3. おすすめフィード(`:ytrec`)を出す。実測で動いたので採用。同じ仕組みで動く履歴(`:ythis`)・登録チャンネル(`:ytsubs`)・後で見る(`:ytwatchlater`)も対象に含める
4. どのブラウザを使うかは環境変数 `TUITUBE_COOKIES_FROM_BROWSER` で指定する(値は yt-dlp の spec をそのまま渡す。例 `chrome` / `chrome:Profile 1` / `safari`)
5. cookie を読めなかった場合は従来どおりの非ログイン検索へフォールバックし、理由を画面に出す
6. 検索と再生(mpv → ytdl_hook → yt-dlp)の両方に同じ cookie 指定を適用する

## 1. 実装が依存する事実

2026/09/18 に実機で確認した内容。検証環境: macOS 14.8.2、yt-dlp 2026.08.19(`~/bin/yt-dlp`、37MB のスタンドアロン版・adhoc 署名)、mpv 0.41.0、端末 iTerm2(フルディスクアクセス付与済み)。Chrome 153 は起動中で `Default` と `Profile 1` の両プロファイルが YouTube ログイン済み。Safari はインストール済みだが YouTube 未ログイン(cookie 8 件)。Firefox・Chromium は未インストール。Brave/Edge/Vivaldi はアプリはあるがプロファイル無し。Arc はインストール済み。yt-dlp の設定ファイル(`~/.config/yt-dlp/config` 等)は無し。

### 1-1. `--cookies-from-browser` の対応ブラウザと失敗形

対応ブラウザ(`yt-dlp --help` と cookies.py L49-50): `brave, chrome, chromium, edge, firefox, opera, safari, vivaldi, whale`。spec の書式は `BROWSER[+KEYRING][:PROFILE][::CONTAINER]`(KEYRING は Linux 用、CONTAINER は Firefox 用、PROFILE は名前またはパス。Safari の PROFILE は binarycookies ファイルのパス)。

`yt-dlp <spec 指定> "ytsearch1:test" --flat-playlist --dump-json` の結果:

| 指定 | 結果 | exit | stdout | stderr(要点) |
|---|---|---|---|---|
| `chrome`(ログイン済) | 成功。402 cookie。`-v` で `[youtube:search] Found YouTube account cookies` | 0 | 結果 JSON | 何も出ない(`--dump-json` は quiet) |
| `chrome:Profile 1` | 成功。295 cookie、ログイン検出 | 0 | 結果 JSON | 無し |
| `chrome:NoSuchProfile` | 失敗。検索は実行されない | 1 | 空 | `ERROR: could not find chrome cookies database in "/Users/x/Library/Application Support/Google/Chrome/NoSuchProfile"` |
| `firefox`(未インストール) | 失敗。検索は実行されない | 1 | 空 | `ERROR: could not find firefox cookies database in '/Users/x/Library/Application Support/Firefox/Profiles'` |
| `chromium`(未インストール) | 同上 | 1 | 空 | `ERROR: could not find chromium cookies database in "/Users/x/Library/Application Support/Chromium"` |
| `arc` | 非対応 | 2 | 空 | `yt-dlp: error: unsupported browser specified for cookies: "arc". Supported browsers are: brave, chrome, chromium, edge, firefox, opera, safari, vivaldi, whale` |
| `""`(空文字) | 失敗 | 1 | 空 | `ERROR: _parse_browser_specification() missing 1 required positional argument: 'browser_name'` |
| `safari`(FDA あり端末) | 成功。8 cookie(未ログイン) | 0 | 結果 JSON | 無し |
| `safari`(FDA 無し文脈。launchd 起動で確認) | 失敗 | 1 | 空 | `ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies'` |
| `safari:/読めないファイル`(mode 000) | 失敗 | 1 | 空 | `ERROR: [Errno 13] Permission denied: '<path>'` |
| `safari:/存在しないパス` | 失敗 | 1 | 空 | `ERROR: custom safari cookies database not found` |
| `chrome` + キーチェーン拒否(`security` コマンドが失敗する状態を PATH 上のシムで再現) | **成功扱い**。結果は非ログイン相当 | 0 | 結果 JSON | `WARNING: find-generic-password failed` / `WARNING: cannot decrypt v10 cookies: no key found`(`-v` 時は stdout に `Extracted 0 cookies from chrome (444 could not be decrypted)`) |

設計への影響:

- yt-dlp 側にフォールバックは無い。cookie を読めない種類の失敗は検索そのものが中止される(結果 0 行・exit 1 か 2)。フォールバックは tuitube が行う
- キーチェーン拒否だけは exit 0 で成功したように見える。stderr の `WARNING:` 行を読まないと劣化に気づけない。`--dump-json`(quiet)でも WARNING/ERROR は stderr に出る(`-v` は不要)
- 失敗の判定材料は exit code と stderr の文言だけ。stdout は結果 JSON のみ

### 1-2. OS 権限と読み取り方式

| ブラウザ | cookie の所在 | 必要な権限 | ブラウザ起動中 | 根拠 |
|---|---|---|---|---|
| Chrome/Chromium 系 | `~/Library/Application Support/Google/Chrome/<Profile>/Cookies`(SQLite・値は暗号化) | 復号鍵をキーチェーン項目「Chrome Safe Storage」から取る。yt-dlp は `security find-generic-password -w -a Chrome -s "Chrome Safe Storage"` を実行(cookies.py L995-1010)。**初回はキーチェーンの許可ダイアログが出る(推定・後述)**。TCC(フルディスクアクセス)は不要 | 問題なし。DB を一時ディレクトリにコピーしてから開く(cookies.py `_open_database_copy` L1112-1117、chrome 側呼び出し L324)。Chrome 起動中に実測成功 | 実測 + ソース |
| Safari | `~/Library/Cookies/Cookies.binarycookies` → 無ければ `~/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies`(L578-582) | 後者は TCC 保護領域。端末アプリに**フルディスクアクセス**が必要。無いと `[Errno 1] Operation not permitted`(ダイアログは出ず黙って拒否) | 問題なし。ファイルを直接読むだけ | 実測(FDA あり iTerm2 配下で成功、FDA 無しの launchd 起動で失敗)。システム TCC.db の実測値: `com.googlecode.iterm2`=2(許可)、`com.apple.Terminal`=0 |
| Firefox | `~/Library/Application Support/Firefox/Profiles/*/cookies.sqlite` | 暗号化無し。キーチェーン不要 | DB コピー方式(L164) | ソースのみ(未インストールで未検証) |

Chrome のキーチェーンについて分かっていること:

- 確定: 現在この Mac の「Chrome Safe Storage」項目の ACL(`security dump-keychain -a`)には `/usr/bin/security` と `/Applications/Google Chrome.app` の 2 つが decrypt 権限で入っている。`/usr/bin/security` が入っている状態は「常に許可」を選んだ後の状態で、`security find-generic-password -w` は 16ms で返る
- 推定: `/usr/bin/security` が ACL に無い環境(初めて使う Mac)では、`security` の実行ごとにキーチェーンの許可ダイアログが GUI に出て、押されるまで yt-dlp が止まる。「許可」だと次回また出る。「常に許可」で ACL に追加されて以後は出ない
- 要確認: 検証中(08:43〜08:50)の Chrome 付き実行は毎回 12〜17 秒の追加待ちがあり、以後は 2 秒以下に落ちた。この間に「常に許可」が押されて ACL が更新された時系列と一致するが、ダイアログの表示自体は観測していない
- ダイアログが出ている間に tuitube 側が yt-dlp を kill しても(検索タイムアウト)、`security` プロセスとダイアログは残る。押されれば静かに終わる

### 1-3. ログイン連動の特殊 URL(フィード)

`yt-dlp --extractor-descriptions` に載る keyword と、Chrome cookie(ログイン済)/ cookie 無しでの実測:

| keyword | 抽出器 | ログイン済 cookie あり | cookie 無し・未ログイン cookie(Safari) | 備考 |
|---|---|---|---|---|
| `:ytrec` | youtube:recommended | 30 件(`--playlist-end 30`)。制限無しだと 167 件・14 秒 | **0 件・exit 0**(エラーにならない) | 説明文に「requires cookies」は無いが実質ログイン必須 |
| `:ythis` / `:ythistory` | youtube:history | 30 件 | exit 1 `ERROR: [youtube:history] Login details are needed to download this content. Use --cookies-from-browser or --cookies for the authentication. …` | 両表記が動く |
| `:ytsubs` | youtube:subscriptions | 30 件 | exit 1、同じ文言(`[youtube:subscriptions]`) | |
| `:ytwatchlater` | youtube:watchlater | 30 件 | 未検証 | |
| `:ytfav` / `:ytnotif` | youtube:favorites / youtube:notif | 未検証 | 未検証 | v1 の対象外 |

出力は `ytsearch` と同じ `--flat-playlist --dump-json` の 1 行 1 JSON(`_type: "url"`, `ie_key: "Youtube"`)。フィールドの有無(30 件中の件数):

| 出力 | id | title | duration | uploader | channel | uploader_url / channel_url |
|---|---|---|---|---|---|---|
| `ytsearch10:`(cookie あり) | 10/10 | 10/10 | 7/10 | 10/10 | 10/10 | 5/10, 10/10 |
| `:ytrec` | 30 | 30 | 29 | 30 | 30 | 30 |
| `:ytsubs` | 30 | 30 | 22 | 30 | 30 | 30 |
| `:ytwatchlater` | 30 | 30 | 29 | 29 | 29 | 28 |
| `:ythis` | 30 | 30 | 30 | **0** | **0** | **0** |

現行の `parse_line`(search.rs L23-46)は `uploader` だけを見るので、履歴の行は投稿者が `-` になる。`channel` へのフォールバックを入れても履歴は埋まらない。

### 1-4. 検索のパーソナライズ

`ytsearch10:music` の結果 id 集合を比較: cookie 無し vs Chrome(ログイン済)の一致は 2/10、Chrome 2 回の一致は 10/10、cookie 無し vs Safari(未ログイン)は 7/10。ログイン cookie で結果が変わり、かつ再現性がある。

### 1-5. mpv 経路(ytdl_hook)

| 項目 | 事実 | 設計への影響 |
|---|---|---|
| 渡し方 | `--ytdl-raw-options-append=cookies-from-browser=<spec>`。ytdl_hook が組む argv(ログ実測): `yt-dlp --no-warnings -J --flat-playlist --sub-format ass/srt/best --cookies-from-browser <spec> --sub-langs all --write-srt --no-playlist -- <URL>` | 検索側と同じ spec 文字列を 1 引数で渡せる |
| `-append` と `=` の違い | `--ytdl-raw-options=` はリスト全体を置き換える。`-append` は 1 項目を追加し、値を `,` で分割しない(`safari,verbose=` がそのまま browser 名として渡り「unsupported browser」になった)。空白・コロン入りの値(`chrome:NoSuch Profile`)もそのまま届く | `-append` を使う。利用者の mpv.conf にある `ytdl-raw-options` を壊さない。値のエスケープは不要 |
| cookie を読めない失敗 | ログ(`--log-file`): `[e][ytdl_hook] ERROR: could not find firefox cookies database in '…'` → `[e][ytdl_hook] youtube-dl failed: unexpected error occurred` → `[e][cplayer] Failed to recognize file format.`。mpv は exit 2 | 現行 `log_detail()`(mpv.rs L227-245)は最後の `[e]` 行を返すため、利用者には「Failed to recognize file format.」しか見えない。ytdl_hook の `ERROR:` 行を優先する |
| キーチェーン拒否(劣化) | `--no-warnings` が付くため yt-dlp の WARNING はログに出ない(ytdl_hook 行は [d]/[v] のみ。`WARNING`/`decrypt` の文字列 0 件)。exit 0 で再生は進む | **再生側では劣化を検知できない**。判定は検索側の結果で行い、再生は「検索で cookie が効いた」時だけ渡す |
| 起動〜1 フレーム(`--frames=1`) | cookie 無し 11.8 秒 / safari 11.6 秒 / chrome 13.8 秒(ACL 付与後) | cookie による再生開始の遅延は小さい。IPC ソケットは起動直後に作られるので `CONNECT_TIMEOUT`(5 秒)には影響しない |

### 1-6. 所要時間

| 項目 | 実測 | 備考 |
|---|---|---|
| yt-dlp 起動(`--version`) | 9.3〜10.3 秒(並列負荷時 17 秒) | `~/bin/yt-dlp`(スタンドアロン版)固有。homebrew 版(2026.03.17)は 0.2〜3.5 秒。tuitube は PATH 先頭の `~/bin` 版を使う。本設計の範囲外だが検索の体感を決める要因 |
| `ytsearch10` | cookie 無し 13.2 秒 / safari 13.2 秒 / chrome 24〜26 秒(ACL 付与前・ダイアログ待ち込みと推定)/ chrome ACL 付与後は +2 秒以下 | |
| フィード(`--playlist-end 30`) | 11〜23 秒 | |
| 現行 `SEARCH_TIMEOUT` | 30 秒(actions.rs L14) | cookie 付き失敗 → cookie 無し再試行を 1 つの timeout に入れると 2 回分で超えうる |

### 1-7. ネットワーク無しの事前チェックは可能

URL を渡さずに `yt-dlp -v --cookies-from-browser <spec>` を実行すると cookie 抽出だけ走り、`yt-dlp: error: You must provide at least one URL.` で exit 2 になる。stdout(`-v` 時)に `Extracting cookies from chrome` / `Extracted 410 cookies from chrome`、拒否時は `Extracted 0 cookies from chrome (444 could not be decrypted)` と stderr の WARNING、ブラウザ無しは exit 1 と ERROR、非対応名は exit 2 と `unsupported browser`。`--version` や `--list-extractors` では抽出が走らない。所要は起動コストの約 10 秒。v1 では採用しない(§2-7)。

### 1-8. yt-dlp wiki の注意事項

wiki「Extractors › Exporting YouTube cookies」(2026/09/18 取得)より:

- "By using your account with yt-dlp, you run the risk of it being banned (temporarily or permanently). Be mindful with the request rate and amount of downloads you make with an account. Use it only when necessary, or consider using a throwaway account."
- "YouTube rotates account cookies frequently on open YouTube browser tabs as a security measure." これはエクスポートした cookie ファイルが失効する話。本方式は実行ごとにブラウザから読み直すため影響は小さいと推定(要観察)

また yt-dlp は既定で `~/.config/yt-dlp/config` を読むので、そこに `--cookies-from-browser chrome` を書けば tuitube のコード変更無しで検索・再生の両方に効く。ただし失敗の分類・フォールバック・画面表示は無い。

## 2. 設計判断

### 2-1. 使われ方

利用者はターミナルにいる。GUI 側で起きること(キーチェーンのダイアログ、TCC の拒否)に気づきにくい前提で組む。

| 場面 | 起きること | 設計 |
|---|---|---|
| `TUITUBE_COOKIES_FROM_BROWSER=chrome tuitube` で起動し、最初の検索 | この Mac で初めてなら `security` がキーチェーンの許可ダイアログを出す(推定)。利用者が気づかないと「検索中...」のまま 30 秒でタイムアウト | タイムアウトの文言にダイアログの案内と「常に許可」を選ぶよう書く。「許可」だけだと検索と再生のたびに出る |
| Safari を指定、端末にフルディスクアクセスが無い | `Operation not permitted` で即失敗(ダイアログ無し) | cookie 無しで検索し直し、システム設定の場所を案内する |
| 存在しないブラウザ・プロファイルを指定 | yt-dlp が exit 1/2 で止まる | 同じく cookie 無しで検索し直し、yt-dlp の文言をそのまま見せる |
| キーチェーンで「拒否」を押した | yt-dlp は成功扱いで非ログインの結果を返す | stderr の WARNING で検知し、以後 cookie を渡さない。理由を表示 |
| おすすめ・履歴を見たい | 検索欄に `:ytrec` / `:ythis` / `:ytsubs` / `:ytwatchlater` と入力して Enter | 既存の入力フローを流用。cookie が無効なら yt-dlp を起動せず即メッセージ(10 秒待たせない) |
| ログインしていないブラウザで `:ytrec` | 0 件・exit 0 | 「検索結果が 0 件」ではなく、ログインを確認するよう案内する |
| 途中でブラウザ側をログアウトした・cookie を消した | yt-dlp は成功扱い。結果が非ログイン相当になる | 検知できない。制限として明記(§6) |
| 再生 | 検索で cookie が効いた時だけ mpv にも渡す。ログイン限定(年齢制限等)の動画が再生できる可能性がある | 視聴履歴には残らない見込み(yt-dlp・mpv は再生統計を送らない。推定・未検証) |
| 使い終わって閉じる・別の日にまた開く | 状態はプロセス内だけ。永続化しない | 次回もまず検索で判定する。キーチェーンの「常に許可」は OS 側に残るので 2 回目以降は待ちが無い |
| 複数のブラウザ・アカウントに入っている | 1 つしか指定できない | `chrome:Profile 1` のようにプロファイルで選ぶ |

### 2-2. 設定の持ち方

| 案 | 長所 | 短所 | 判断 |
|---|---|---|---|
| 環境変数 `TUITUBE_COOKIES_FROM_BROWSER`(spec をそのまま) | 依存追加なし。shell rc に 1 行。yt-dlp の書式(プロファイル・パス指定)をそのまま使える | 起動ごとの切替は `ENV=… tuitube` と書く | **採用** |
| 起動オプション `--cookies-from-browser` | 発見しやすい | tuitube に引数解析が無く、clap 等の依存が増える | 後から足せる。v1 では見送り |
| 設定ファイル | 他の設定も置ける | 仕組みが無い。この機能のためだけに作るのは過剰 | 見送り |
| yt-dlp.conf に書いてもらう | コード変更ゼロで検索・再生に効く | tuitube が失敗を分類できず、フォールバックも表示も無い | 設計の前提には使わない。利用者がそうしても壊れない |

値の扱い:

- 未設定・空・空白のみ → 無効(従来どおり)。前後の空白は trim
- それ以外は検証せず yt-dlp に渡す。対応ブラウザの一覧を tuitube に持たない。yt-dlp の更新で一覧が変わるため。非対応名は yt-dlp の `unsupported browser …` の文言をそのまま画面に出す
- 表示用にブラウザ名だけを切り出す(`:` と `+` より前)。`chrome:Profile 1` → `chrome`

### 2-3. cookie の状態(状態機械)

`CookieState` を `App` に持つ。検索・再生・表示がすべて `App` を見るため。

| 状態 | 意味 | 検索に渡す | 再生に渡す |
|---|---|---|---|
| `Off` | 環境変数なし | 渡さない | 渡さない |
| `Armed(source)` | 指定あり。まだ一度も検索で確認していない | 渡す | 渡さない(再生に至る前に必ず検索がある) |
| `Active(source)` | 検索で cookie が問題なく効いた | 渡す | 渡す |
| `Suspended { source, reason }` | 読めない・復号できない・タイムアウトのいずれか | 渡さない | 渡さない |

遷移(§3-4 の表)。`Suspended` はプロセス終了まで固定し、自動で再試行しない。失敗のたびに yt-dlp を 2 回(約 10 秒 × 2)待たせないため。再有効化は tuitube の再起動(v1)。再有効化キーは §6 の将来項目。

### 2-4. 失敗の分類とフォールバック

yt-dlp の exit code と stderr から `CookieOutcome` を決める。判定文言は §1-1 の実測どおりに固定する。

| outcome | 判定 | 結果の扱い | 状態 | 表示 |
|---|---|---|---|---|
| `NotUsed` | cookie を渡さなかった | そのまま | 変えない | 無し |
| `Ok` | exit 0、stderr に cookie 関連の WARNING 無し | そのまま | `Armed` → `Active` | ステータス行に `cookies: chrome` |
| `Degraded(reason)` | stderr に `cannot decrypt v10 cookies` / `find-generic-password failed` / `could not be decrypted` | そのまま(既に非ログイン相当) | → `Suspended` | 通知: 復号できなかった。キーチェーンで「常に許可」 |
| `Unreadable(reason)` | exit ≠ 0 かつ stderr に `could not find <browser> cookies database` / `unsupported browser specified for cookies` / `custom safari cookies database not found` / `Operation not permitted` / `Permission denied` / `_parse_browser_specification()` | **同じ target を cookie 無しで 1 回だけ再実行**し、その結果を返す | → `Suspended` | 通知: 読めなかった理由 + cookie 無しで検索した旨。Safari の `Operation not permitted` はフルディスクアクセスの案内を足す |
| `LoginRequired` | stderr に `Login details are needed` | 結果なし | 変えない(cookie は読めている。ログインしていないだけ) | エラー: そのフィードにはログインが必要 |
| `TimedOut` | cookie 付きの試行がタイムアウト | 結果なし | `Armed` → `Suspended`(ダイアログ案内付き)。`Active` なら変えない | エラー: タイムアウト。`Armed` 時はダイアログの案内を足す |
| `Unknown` | 上記以外の失敗(ネットワーク等) | 従来どおりエラー | 変えない | 従来の文言 |

タイムアウトは yt-dlp 1 回の実行ごとに `YT_DLP_TIMEOUT`(30 秒)とし、再試行があれば最大 2 回分待つ。現行の `start_search` 外側の `timeout(SEARCH_TIMEOUT, …)`(actions.rs L107-113)は `run_search` の中へ移す。

### 2-5. フィード

- 入力欄の文字列が `:ytrec` / `:ythis` / `:ythistory` / `:ytsubs` / `:ytwatchlater` のいずれかと完全一致(trim 後)なら `Target::Feed`、それ以外は従来の `ytsearch10:<query>`。`:` で始まる別の文字列(`:ytfoo` 等)は検索語として扱う
- フィードには `--playlist-end 30`(`FEED_LIMIT`)を付ける。`:ytrec` は制限無しで 167 件返るため
- すべてのフィードを「ログイン必須」として扱う。`:ytrec` は yt-dlp 上は cookie 不要だが実測で 0 件になるため。`CookieState` が `Off` / `Suspended` のときは yt-dlp を起動せずに案内を出す
- 0 件のときの文言をフィード別にする(`set_results` に target を渡す)
- 結果一覧の描画は変えない。履歴は投稿者が `-` になる(§1-3)。`parse_line` に `channel` へのフォールバックだけ入れる(後で見る等の取りこぼしを減らす)

### 2-6. mpv 側

- `MpvController::launch` に追加引数を渡せるようにし、`Active` のときだけ `--ytdl-raw-options-append=cookies-from-browser=<spec>` を URL の前に 1 つ足す
- `log_detail()` は `[e][ytdl_hook] ERROR:` で始まる行があればそれを優先し、無ければ従来どおり最後の `[e]`/`[fatal]` 行
- mpv が終了し、そのエラー文言が cookie ストアを指す場合は `Suspended` にして「Enter でもう一度再生すると cookie 無しで再生する」旨を出す。自動で mpv を作り直しはしない(v1)。判定は `cookie_store_failure()`。`Operation not permitted` / `Permission denied` は mpv がストリームやファイルの失敗でも出すので、同じ行が cookie を指すときだけ cookie 由来として扱う
- 劣化(キーチェーン拒否)は再生側では検知できない(§1-5)。検索側で `Suspended` になっていれば渡さないので、実際に起きるのは「検索の後にブラウザ側の状態が変わった」場合だけ

### 2-7. 事前チェックは v1 で採用しない

URL 無し起動(§1-7)で起動時に cookie の可否を判定できるが、起動が約 10 秒遅れる。最初の検索で同じ判定ができるため v1 では入れない。利用者から「起動時に確認してほしい」という要望が出たら、環境変数(例 `TUITUBE_COOKIES_CHECK=1`)で任意にする。

### 2-8. セキュリティ・プライバシー

- tuitube は cookie の値を読まない・保存しない・ログに出さない。扱うのはブラウザ名(spec)だけ。`ps` に見えるのも spec のみ
- `--cookies-from-browser` で渡す場合、yt-dlp は cookie をメモリ内で使う。この判断は cookies-file-design.md で覆し、ヘッドレス環境向けに `--cookies FILE` も選べるようにした。file 方式では利用者が置いた cookies.txt を yt-dlp が読み書きする(終了時に書き戻す)
- アカウント制限のリスク(§1-8)は利用者向け説明に転記する
- 検索結果 JSON にアカウントを示す項目は含まれない(実測で確認したフィールド一覧に該当なし)

## 3. アーキテクチャ

### 3-1. データフロー

```
起動
  CookieState::from_env()  ─→ App.cookies (Off | Armed)

検索 (Enter)
  query ─→ cookies::target_for ─→ Target(Search|Feed)
    先行検索を abort し nonce を進める(断る場合も含む)
    Feed かつ cookies.for_search()==None ─→ 即エラー表示(yt-dlp 起動なし)
    それ以外 ─→ search::run_search(runner, target, cookies.for_search())
       ├ yt-dlp <url> --flat-playlist --dump-json [--playlist-end 30] [--cookies-from-browser spec]
       ├ classify(exit, stderr) ─→ CookieOutcome
       └ Unreadable なら cookie 無しで 1 回再実行
    ─→ AppEvent::SearchDone { nonce, target, report }
main::handle_event
    app.cookies.observe(&report.outcome)
    app.notice = フォールバック/劣化の説明(あれば)
    app.set_results(results, &target) / app.error

再生 (Enter)
  extra = app.cookies.for_playback().map(mpv_arg)
  MpvController::launch(url, nonce, tx, video, &extra)
    mpv … [--ytdl-raw-options-append=cookies-from-browser=spec] url
  MpvExited { error } ─→ cookie_store_failure(error) が Some なら app.cookies.suspend(reason)

描画
  status_line: error > notice > 検索中 (cookies: chrome) > idle + cookies ラベル
```

### 3-2. モジュール構成

| ファイル | 役割 | 変更 |
|---|---|---|
| `src/cookies.rs`(新規) | 環境変数の読み取り、spec の保持、yt-dlp/mpv 引数の生成、stderr の分類、状態機械、フィード keyword、利用者向け文言。外部プロセスに触れない純粋ロジック | 新規 |
| `src/search.rs` | yt-dlp 実行の注入(`YtDlp` トレイト)、引数生成、分類→再試行、`parse_line` のフォールバック | 変更 |
| `src/mpv.rs` | 起動引数の純粋関数化と追加引数、`log_detail` の優先順 | 変更 |
| `src/actions.rs` | `start_search`: target 決定・ログイン必須フィードの短絡・cookie 受け渡し。`start_playback`: 追加引数 | 変更 |
| `src/app.rs` | `App.cookies`、`App.notice`、`set_results(results, &target)`、ステータス行 | 変更 |
| `src/main.rs` | `mod cookies`、`App` 初期化で `CookieState::from_env()`、`SearchDone` と `MpvExited` の処理 | 変更 |
| `src/ui.rs` | 入力モードのヘルプにフィード keyword | 変更 |
| `src/input.rs` | 変更なし(Enter → `start_search` のまま) | なし |

### 3-3. 型と関数(TDD の足場)

シグネチャと意図のみ。本体はテストを先に書いてから埋める。

```rust
// src/cookies.rs
pub const ENV_VAR: &str = "TUITUBE_COOKIES_FROM_BROWSER";
pub const FEED_LIMIT: usize = 30;

/// yt-dlp の BROWSER[+KEYRING][:PROFILE][::CONTAINER] をそのまま保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieSource { spec: String }

impl CookieSource {
    /// None・空・空白のみは None。前後の空白だけ落とす。
    pub fn from_env_value(value: Option<&str>) -> Option<Self>;
    pub fn from_env() -> Option<Self>;                 // std::env::var(ENV_VAR)
    pub fn spec(&self) -> &str;
    /// 表示用。':' '+' より前。"chrome:Profile 1" → "chrome"
    pub fn browser(&self) -> &str;
    /// ["--cookies-from-browser", spec]。2 つの argv として渡す(シェル経由ではない)。
    pub fn yt_dlp_args(&self) -> [String; 2];
    /// "--ytdl-raw-options-append=cookies-from-browser=" + spec
    pub fn mpv_arg(&self) -> String;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieOutcome {
    NotUsed,
    Ok,
    Degraded(String),     // 復号できず、非ログイン相当の結果になった
    Unreadable(String),   // 読めず、検索が実行されなかった
    LoginRequired,        // cookie は読めたがログインしていない(フィード)
    TimedOut,
    Unknown,
}

/// yt-dlp の exit code と stderr から判定する。cookie を渡していない場合は呼ばない。
pub fn classify(exit_code: Option<i32>, stderr: &str) -> CookieOutcome;

/// mpv の失敗文言から cookie ストア由来の行だけを拾う(exit code を見ない mpv 経路用)。
pub fn cookie_store_failure(text: &str) -> Option<String>;

/// 利用者向けの説明文。browser() が safari のときの Operation not permitted は
/// フルディスクアクセスの案内を含む。
pub fn describe(outcome: &CookieOutcome, source: &CookieSource) -> String;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CookieState {
    #[default]
    Off,
    Armed(CookieSource),
    Active(CookieSource),
    Suspended { source: CookieSource, reason: String },
}

impl CookieState {
    pub fn from_env() -> Self;
    pub fn for_search(&self) -> Option<&CookieSource>;    // Armed | Active
    pub fn for_playback(&self) -> Option<&CookieSource>;  // Active のみ
    pub fn observe(&mut self, outcome: &CookieOutcome);   // §3-4 の遷移
    pub fn suspend(&mut self, reason: String);            // mpv 側の失敗用
    /// "cookies: chrome" / "cookies: chrome (停止)"。Off は None。
    pub fn label(&self) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed { Recommended, History, Subscriptions, WatchLater }

impl Feed {
    /// 入力文字列(trim 済み)が keyword と完全一致するものだけ。":ythistory" も History。
    pub fn parse(query: &str) -> Option<Feed>;
    pub fn keyword(self) -> &'static str;   // yt-dlp に渡す ":ytrec" 等
    pub fn label(self) -> &'static str;     // "おすすめ" 等
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target { Search(String), Feed(Feed) }

impl Target {
    pub fn for_query(query: &str) -> Target;
    pub fn yt_dlp_url(&self) -> String;     // "ytsearch10:<q>" / ":ytrec"
    pub fn empty_message(&self) -> String;  // 0 件時の文言
    pub fn requires_login(&self) -> bool;   // Feed は全て true
}
```

```rust
// src/search.rs
pub const YT_DLP_TIMEOUT: Duration = Duration::from_secs(30);   // 1 回の実行ごと

/// 本番は tokio Command、テストは台本どおりの Output を返す偽物。
pub trait YtDlp {
    fn run(&self, args: Vec<String>) -> impl Future<Output = std::io::Result<std::process::Output>> + Send;
}
pub struct RealYtDlp;    // Command::new("yt-dlp").args(args).kill_on_drop(true).output()

/// [url, "--flat-playlist", "--dump-json"] + Feed なら ["--playlist-end", "30"] + cookie 引数
pub fn yt_dlp_args(target: &Target, cookies: Option<&CookieSource>) -> Vec<String>;

#[derive(Debug)]
pub struct SearchReport {
    pub results: Result<Vec<SearchResult>, String>,
    pub outcome: CookieOutcome,
    pub fell_back: bool,   // cookie 無しで再実行した
}

/// cookie 付きで実行 → classify → Unreadable なら cookie 無しで 1 回だけ再実行。各回 YT_DLP_TIMEOUT。
pub async fn run_search(runner: &impl YtDlp, target: &Target, cookies: Option<&CookieSource>) -> SearchReport;

fn parse_line(line: &str) -> Option<SearchResult>;   // uploader → channel の順で採る
```

```rust
// src/mpv.rs
/// 現行 launch() 内の引数組み立てを純粋関数に出す。extra は URL の直前。
pub fn launch_args(socket: &Path, log: &Path, geometry: Geometry, extra: &[String], url: &str) -> Vec<String>;

impl MpvController {
    pub async fn launch(url: &str, nonce: u64, events: UnboundedSender<AppEvent>, video: VideoSink, extra: &[String]) -> Result<Self, String>;
}

/// "[e][ytdl_hook] ERROR:" の行を優先。無ければ従来どおり最後の [e]/[fatal]。
fn log_detail(path: &Path) -> String;
```

```rust
// src/app.rs
pub enum AppEvent {
    …,
    SearchDone { nonce: u64, target: Target, report: SearchReport },
    …
}

pub struct App {
    …,
    pub cookies: CookieState,
    /// エラーではない知らせ(フォールバックした・劣化した)。次の検索開始で消す。
    pub notice: Option<String>,
}

impl App {
    pub fn set_results(&mut self, results: Vec<SearchResult>, target: &Target);
    // status_line: error > notice > 検索中 (+ label) > idle (+ label)
}
```

```rust
// src/actions.rs
pub fn start_search(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session);
//   start_search_with(app, tx, session, RealYtDlp) を呼ぶだけ

/// yt-dlp の実行者を注入する形。テストは偽物を渡し、外部プロセスに到達しない。
pub fn start_search_with<R: YtDlp + Send + Sync + 'static>(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session, runner: R);
//   target = Target::for_query(trimmed)
//   先行タスクを abort、search_nonce += 1、searching = false、notice = None
//   target.requires_login() && app.cookies.for_search().is_none() → app.error = 案内、return
//   cookies = app.cookies.for_search().cloned()
//   spawn { report = search::run_search(&runner, &target, cookies.as_ref()).await; SearchDone { nonce, target, report } }

pub async fn start_playback(app: &mut App, tx: &UnboundedSender<AppEvent>, session: &mut Session);
//   extra: Vec<String> = app.cookies.for_playback().map(|c| vec![c.mpv_arg()]).unwrap_or_default()
```

### 3-4. 状態遷移

| 現在 | outcome | 次 |
|---|---|---|
| `Off` | 何でも | `Off` |
| `Armed` | `Ok` | `Active` |
| `Armed` | `Degraded` / `Unreadable` | `Suspended`(reason = describe) |
| `Armed` | `TimedOut` | `Suspended`(reason = タイムアウト + キーチェーンダイアログの案内) |
| `Armed` | `LoginRequired` / `Unknown` / `NotUsed` | `Armed` |
| `Active` | `Ok` / `LoginRequired` / `Unknown` / `NotUsed` / `TimedOut` | `Active` |
| `Active` | `Degraded` / `Unreadable` | `Suspended` |
| `Suspended` | 何でも | `Suspended` |

`suspend(reason)` は `Armed` / `Active` から `Suspended` へ。`Off` は変えない。

### 3-5. yt-dlp 引数

| target | cookie | argv |
|---|---|---|
| `Search("rust tui")` | なし | `yt-dlp ytsearch10:rust tui --flat-playlist --dump-json`(現行と同一) |
| `Search("rust tui")` | `chrome:Profile 1` | `yt-dlp ytsearch10:rust tui --flat-playlist --dump-json --cookies-from-browser "chrome:Profile 1"` |
| `Feed(Recommended)` | `chrome` | `yt-dlp :ytrec --flat-playlist --dump-json --playlist-end 30 --cookies-from-browser chrome` |
| `Feed(History)` | なし | 実行しない(短絡) |

### 3-6. mpv 起動引数

現行(mpv.rs L279-285)の `--input-ipc-server` / `--log-file` / `--no-terminal` / `--vo=kitty` / `--vo-kitty-*` の後、URL の直前に `--ytdl-raw-options-append=cookies-from-browser=<spec>` を 1 つ。`Active` 以外では何も足さない。

### 3-7. 文言

| 場面 | 文言 |
|---|---|
| ステータス(idle/検索中、`Armed`/`Active`) | `… cookies: chrome` |
| ステータス(`Suspended`) | `… cookies: chrome (停止)` |
| `Unreadable`(一般) | `cookie を読めませんでした (chrome): <yt-dlp の ERROR 本文>。cookie 無しで検索しました` |
| `Unreadable`(`safari` 指定かつ `Operation not permitted`) | `Safari の cookie を読めませんでした。端末アプリにフルディスクアクセスを許可してください (システム設定 → プライバシーとセキュリティ → フルディスクアクセス)。cookie 無しで検索しました` |
| `Degraded` | `cookie を復号できませんでした (chrome)。キーチェーンのダイアログで「常に許可」を選んでください。以後は cookie 無しで動作します` |
| `TimedOut`(`Armed`) | `検索がタイムアウトしました (30 秒)。cookie 連携の初回は macOS のキーチェーン許可ダイアログが別ウィンドウで出ている可能性があります。「常に許可」を選び、tuitube を再起動してください。以後は cookie 無しで動作します` |
| `LoginRequired` | `<フィード名> にはログインが必要です。ブラウザ (chrome) で YouTube にログインしているか確認してください` |
| フィード要求時に `Off` | `<フィード名> には cookie 連携が必要です。TUITUBE_COOKIES_FROM_BROWSER を設定してください` |
| フィード要求時に `Suspended` | `<フィード名> は cookie 連携が停止中のため使えません: <reason>` |
| フィード 0 件 | `<フィード名> が空でした。ブラウザで YouTube にログインしているか確認してください` |
| mpv 失敗が `Unreadable` | `<mpv の文言>。cookie 連携を停止しました。Enter でもう一度再生すると cookie 無しで再生します` |

### 3-8. ライフサイクル別の動き

- 起動: `CookieState::from_env()`。`Armed` ならステータス行に `cookies: chrome` が出る
- 最初の検索(`Armed`): cookie 付きで実行。`Ok` → `Active`。以後の再生に cookie が渡る
- 検索の連打: 先行タスクを abort し nonce を進める。ログイン必須フィードを断るときも同じ扱いにするので、断りのメッセージと先行検索の結果一覧が混ざらない。`SearchDone` は nonce で捨てるので、古い試行の `outcome` で状態を動かさない(`observe` は nonce 一致時のみ)
- 再生中の別検索: 検索の結果で `Suspended` になっても再生中の mpv には影響しない。次の再生から渡さない
- 終了: 状態は捨てる。yt-dlp は `kill_on_drop`、mpv は既存の quit → kill

## 4. テスト戦略

### 4-1. 外部プロセス無しで検証できる単位

| 単位 | 場所 | 依存 |
|---|---|---|
| 環境変数値 → `CookieSource`、`browser()`、`yt_dlp_args()`、`mpv_arg()` | cookies.rs | なし |
| `classify(exit, stderr)`(§1-1 の実測文言をフィクスチャに) | cookies.rs | なし |
| `describe()` の文言 | cookies.rs | なし |
| `CookieState::observe` / `suspend` / `for_search` / `for_playback` / `label` | cookies.rs | なし |
| `Feed::parse` / `Target::for_query` / `yt_dlp_url` / `empty_message` | cookies.rs | なし |
| `yt_dlp_args(target, cookies)` | search.rs | なし |
| `run_search` の再試行・分類(`FakeYtDlp` に `Output` の台本を持たせる) | search.rs | tokio(プロセスなし) |
| `parse_line` の `channel` フォールバック | search.rs | なし |
| `launch_args()` | mpv.rs | なし |
| `log_detail()`(§1-5 のログをフィクスチャに) | mpv.rs | 一時ファイル |
| `App::set_results(…, &target)` / `status_line` | app.rs | なし |
| `start_search_with` の短絡・先行タスクの打ち切り | actions.rs | なし(偽のランナーを注入し、タスクを積まないことを確認) |
| `handle_event(SearchDone)` / `handle_event(MpvExited)` の状態遷移 | main.rs | なし |
| `help_text(Mode::Input)` | ui.rs | なし |

`FakeYtDlp` は `Mutex<VecDeque<io::Result<Output>>>` の台本と `Mutex<Vec<Vec<String>>>` の呼び出し記録を持つ。`Output` の `status` は `std::os::unix::process::ExitStatusExt::from_raw(code << 8)` で作る。

### 4-2. 最初に書く Red テスト

cookies.rs(すべて純粋関数):

1. `from_env_value_trims_and_rejects_blank`: `None` → `None`、`Some("  ")` → `None`、`Some(" chrome:Profile 1 ")` → `spec() == "chrome:Profile 1"`
2. `browser_is_the_part_before_profile_or_keyring`: `"chrome:Profile 1"` → `"chrome"`、`"chrome+basictext"` → `"chrome"`、`"firefox::work"` → `"firefox"`、`"safari"` → `"safari"`
3. `yt_dlp_args_are_two_argv_entries`: `["--cookies-from-browser", "chrome:Profile 1"]`(引用符やエスケープを含まない)
4. `mpv_arg_uses_the_append_form_verbatim`: `"--ytdl-raw-options-append=cookies-from-browser=chrome:Profile 1"`
5. `classify_ok_when_exit_zero_and_no_cookie_warning`: `(Some(0), "")` → `Ok`
6. `classify_degraded_on_keychain_warnings`: `(Some(0), "WARNING: find-generic-password failed\nWARNING: cannot decrypt v10 cookies: no key found\n")` → `Degraded`
7. `classify_unreadable_for_each_observed_error`(パラメタ化):
   - `(Some(1), "ERROR: could not find firefox cookies database in '/Users/x/Library/Application Support/Firefox/Profiles'")`
   - `(Some(1), "ERROR: could not find chrome cookies database in \"/Users/x/Library/Application Support/Google/Chrome/NoSuchProfile\"")`
   - `(Some(2), "yt-dlp: error: unsupported browser specified for cookies: \"arc\". Supported browsers are: brave, chrome, chromium, edge, firefox, opera, safari, vivaldi, whale")`
   - `(Some(1), "ERROR: [Errno 1] Operation not permitted: '/Users/x/Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies'")`
   - `(Some(1), "ERROR: [Errno 13] Permission denied: '/tmp/x.binarycookies'")`
   - `(Some(1), "ERROR: custom safari cookies database not found")`
   - `(Some(1), "ERROR: _parse_browser_specification() missing 1 required positional argument: 'browser_name'")`
8. `classify_login_required`: `(Some(1), "ERROR: [youtube:history] Login details are needed to download this content. Use --cookies-from-browser or --cookies for the authentication. …")` → `LoginRequired`
9. `classify_unknown_for_unrelated_failures`: `(Some(1), "ERROR: [youtube:search] Unable to download webpage: …")` → `Unknown`。`(None, "")`(シグナル終了)→ `Unknown`
10. `describe_mentions_full_disk_access_for_safari_tcc`: `Operation not permitted` を含む `Unreadable` + `safari` → 文言に「フルディスクアクセス」
11. `state_transitions_follow_the_table`: §3-4 の全行をパラメタ化
12. `for_playback_is_some_only_when_active`
13. `label_is_none_when_off_and_marks_suspended`
14. `feed_parse_matches_keywords_exactly`: `":ytrec"` → `Recommended`、`":ythistory"` と `":ythis"` → `History`、`":ytfoo"` → `None`、`"ytrec"` → `None`
15. `target_for_query_wraps_searches_and_passes_feeds`: `"rust tui"` → `Search("rust tui")`、`yt_dlp_url()` が `"ytsearch10:rust tui"`。`":ytsubs"` → `Feed(Subscriptions)`、`yt_dlp_url()` が `":ytsubs"`
16. `feeds_require_login_and_searches_do_not`

search.rs:

17. `yt_dlp_args_without_cookies_match_the_current_command`: `["ytsearch10:q", "--flat-playlist", "--dump-json"]`(回帰)
18. `yt_dlp_args_append_cookie_flags_after_the_fixed_part`
19. `yt_dlp_args_limit_feeds`: `--playlist-end 30` を含み、`Search` には含まない
20. `parse_line_falls_back_to_channel_when_uploader_is_missing`。`uploader` も `channel` も無い(履歴の行)は `None` のまま

mpv.rs:

21. `launch_args_place_extra_once_right_before_the_url`
22. `log_detail_prefers_the_ytdl_hook_error_line`: §1-5 の 3 行(ERROR / youtube-dl failed / Failed to recognize file format.)を書いたログで `"ERROR: could not find firefox cookies database in '…'"` が返る。ERROR 行が無いログでは従来の結果(既存テスト `reads_failure_reason_from_mpv_log` を保つ)

### 4-3. 続いて書くテスト

search.rs(`FakeYtDlp`):

23. `run_search_retries_without_cookies_when_the_store_is_unreadable`: 台本 = [exit 1 + firefox の ERROR, exit 0 + 結果 1 行]。呼び出し 2 回、1 回目の argv に `--cookies-from-browser` があり 2 回目には無い。`results` は 2 回目のもの、`fell_back == true`、`outcome == Unreadable`
24. `run_search_does_not_retry_on_unrelated_failure`: 呼び出し 1 回、`results` は `Err`、`outcome == Unknown`、`fell_back == false`
25. `run_search_keeps_results_but_reports_degradation`: exit 0 + WARNING 2 行 + 結果 → `results` あり、`outcome == Degraded`、呼び出し 1 回
26. `run_search_reports_login_required_without_retry`
27. `run_search_reports_not_used_without_cookies`: cookie 無しでは stderr に何が出ても `NotUsed`
28. `run_search_times_out_per_attempt`(`start_paused`): 完了しない偽物 → `Err` に「タイムアウト」、`outcome == TimedOut`、再試行しない

app.rs / actions.rs / main.rs / ui.rs:

29. `idle_and_searching_status_show_the_cookie_label`
30. `notice_shows_when_there_is_no_error_and_error_wins`
31. `empty_feed_results_explain_the_login_requirement`(`set_results(vec![], &Target::Feed(Recommended))`)
32. `empty_search_results_keep_the_current_message`(回帰)
33. `feed_requiring_login_is_refused_before_spawning_when_cookies_are_off`: `session.search_task.is_none()`、`app.error` に環境変数名
34. `feed_requiring_login_is_refused_when_suspended`
35. `start_search_clears_the_notice`
36. `search_done_advances_the_cookie_state`(`handle_event` に `Armed` + `outcome: Ok` → `Active`)
37. `search_done_from_a_superseded_nonce_does_not_touch_the_state`
38. `search_done_with_fallback_sets_the_notice`
39. `mpv_exit_with_an_unreadable_cookie_store_suspends`(`MpvExited { error: Some("ERROR: could not find chrome cookies database …") }`)
40. `input_help_mentions_feed_keywords`

### 4-4. 既存テストの更新

- `AppEvent::SearchDone` の形が変わる。actions.rs の `search_bumps_the_nonce_and_clears_the_error` は `SearchDone` を受け取らないので影響なし
- `MpvController::launch` の引数追加。呼び出しは `start_playback` だけ
- `App::set_results` の引数追加。app.rs の `empty_results_set_error_and_stay_in_input` / `results_switch_to_list_mode` に `&Target::Search(…)` を足す
- mpv.rs `reads_failure_reason_from_mpv_log` は ERROR 行を含まないので結果不変。フィクスチャを流用して 22 を追加

### 4-5. 実機確認項目(自動テスト不能)

1. `TUITUBE_COOKIES_FROM_BROWSER=chrome` で起動 → 検索 → ステータスに `cookies: chrome` → 同じ語を cookie 無しと比べて結果が変わる
2. `:ytrec` / `:ythis` / `:ytsubs` / `:ytwatchlater` で一覧が出る。履歴の投稿者が `-` で描画が崩れない
3. `chrome:Profile 1` で別プロファイル
4. `firefox`(未インストール)を指定 → 検索結果は出る(cookie 無し)+ 通知 → ステータスが `(停止)` → 再生は cookie 無しで動く
5. `safari` をフルディスクアクセス無しの Terminal.app から → フォールバック + フルディスクアクセスの案内
6. キーチェーンの ACL から `/usr/bin/security` を外した状態(または別 Mac)で初回検索 → ダイアログを放置して 30 秒タイムアウトの文言を確認 → 「常に許可」→ 再起動で `Active`
7. 「拒否」を押した場合に `Degraded` の通知が出て、以後 `(停止)`
8. `Active` で年齢制限動画が再生できるか(任意)
9. 再生後にブラウザ側の視聴履歴に残らないことの確認(推定の裏取り)

## 5. 影響範囲

| ファイル | 変更点 | 現行の該当箇所 |
|---|---|---|
| `src/cookies.rs`(新規) | §3-3 の型と関数 | — |
| `src/search.rs` | `YtDlp` トレイトと `RealYtDlp`、`yt_dlp_args`、`run_search`、`SearchReport`、`parse_line` のフォールバック、`YT_DLP_TIMEOUT` | `search()` L48-72、`parse_line` L23-46 |
| `src/mpv.rs` | `launch_args`、`launch` の `extra` 引数、`log_detail` の優先順 | `launch` L264-297(引数組み立て L279-285)、`log_detail` L227-245 |
| `src/actions.rs` | `start_search_with` の target 決定・先行タスクの打ち切り・短絡・cookie 受け渡し・notice クリア・timeout 撤去、`start_playback` の `extra`、`SEARCH_TIMEOUT` 削除 | `SEARCH_TIMEOUT` L14、`start_search` L92-116(timeout L107-113)、`start_playback` L118-143(launch 呼び出し L126) |
| `src/app.rs` | `AppEvent::SearchDone` の形、`App.cookies`、`App.notice`、`set_results(…, &Target)`、`search_status` | `SearchDone` L22-25、`App` L101-133、`set_results` L154-163、`search_status` L195-203 |
| `src/main.rs` | `mod cookies;`、`App` 初期化、`SearchDone` / `MpvExited` の処理 | `run()` L70、`handle_event` L192-205 / L224-228 |
| `src/ui.rs` | `help_text(Mode::Input)` | L158-166 |
| `Cargo.toml` | 変更なし(依存追加なし) | — |
| 利用者向け説明 | README が無いため本 doc の §2-1 と §3-7 を参照先にする。README を作る場合は環境変数・「常に許可」・Safari のフルディスクアクセス・アカウント制限リスク(§1-8)を載せる | — |

## 6. 制限・非対象

- ブラウザ側でログアウト・cookie 削除した後の変化は検知できない。yt-dlp は成功扱いで非ログインの結果を返し、表示は `cookies: chrome` のまま
- 再生側(mpv → ytdl_hook)は `--no-warnings` のため劣化を検知できない。検索側の判定に依存する
- `Suspended` からの自動復帰・再有効化キーは無い。再起動で戻す。将来: `Ctrl+R` 等で `Armed` に戻す
- mpv が cookie 起因で失敗したときの自動再起動は無い。利用者が Enter でもう一度再生する
- 起動時の事前チェック(§1-7)は無い。将来: 環境変数で任意
- 視聴履歴は tuitube での再生では記録されない見込み(推定・未検証)
- 同時に 1 つのブラウザ・プロファイルだけ
- `:ytfav` / `:ytnotif` は対象外(未検証)。keyword 一覧に足すだけで増やせる
- Firefox / Brave / Edge / Vivaldi / Linux のキーリングは未検証。spec をそのまま渡すので動く可能性はあるが、失敗文言の分類は Chrome/Safari の実測に基づく
- yt-dlp の起動に約 10 秒かかる件(スタンドアロン版固有)は本設計の範囲外。homebrew 版に切り替えると 0.2〜3.5 秒になる
- アカウント制限のリスク(§1-8)は利用者の判断。tuitube 側で抑制はしない
- 履歴フィードの投稿者は yt-dlp の出力に無いため表示できない

## 7. 実装順

1. `src/cookies.rs`: §4-2 の 1〜16 を Red で書き、型と純粋関数を埋める
2. `src/search.rs`: 17〜20 → `yt_dlp_args` と `parse_line`。次に `YtDlp` / `FakeYtDlp` と 23〜28 → `run_search`。`search()` を `RealYtDlp` で包む
3. `src/mpv.rs`: 21〜22 → `launch_args` と `log_detail`。`launch` に `extra` を通す
4. `src/app.rs` / `src/actions.rs` / `src/main.rs`: 29〜39 → `App.cookies` / `notice`、`SearchDone` の形、短絡、状態遷移の配線。既存テストの引数を直す(§4-4)
5. `src/ui.rs`: 40 → ヘルプ
6. §4-5 の実機確認。文言は実機で見てから調整する
