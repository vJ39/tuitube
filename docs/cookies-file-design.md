# cookies.txtファイルによるcookie連携

ヘッドレス環境(ブラウザもキーチェーンも無いLinuxサーバー等)向けに、ブラウザから手動エクスポートしたcookies.txt(Netscape形式)を`--cookies`でyt-dlp/mpvに渡す方式を、既存の`--cookies-from-browser`方式と並ぶ選択肢として追加する。

## 設定ファイル

`[cookies]`セクションに`file`キーを追加する。

```toml
[cookies]
# browser = "chrome"
file = "/home/ubuntu/.config/tuitube/cookies.txt"
```

- `browser`と`file`は排他。両方指定されていたら`file`を優先し、`browser`を無視した旨のnoticeを出す。
- 環境変数`TUITUBE_COOKIES_FROM_BROWSER`は既存どおりbrowser方式専用のまま(file方式のenv var上書きはv1では追加しない)。
- ファイルが存在しない/読めない/書き込めない場合は、既存の`cache_dir`検証と同様にnoticeを出し、cookie連携Offとして起動する。書き込みも見るのは、yt-dlpが終了時に`--cookies`のファイルへcookieを書き戻すため(読み取り専用だと検索のたびにPermissionErrorで落ちる)。中身を変えずに判定できるようappendで開く。
- 排他のnotice(`browser`を無視した旨)は、ファイルが使えると確かめた後に出す。使えないときは`browser`も使わないので、先に出すと「fileを使います」と「cookie連携なしで動きます」が並んで矛盾する。

## CookieSource

既存の`struct CookieSource { spec: String }`(browser方式専用)を、enumに変える。

```rust
pub enum CookieSource {
    Browser(String),  // yt-dlp の BROWSER[+KEYRING][:PROFILE][::CONTAINER]
    File(PathBuf),     // cookies.txt のパス
}
```

- `from_spec(Option<&str>) -> Option<Self>`: 既存どおりBrowser側の構築に使う(呼び出し元は変えない)
- 新規`from_file(Option<&Path>) -> Option<Self>`: File側の構築
- `yt_dlp_args(&self) -> Vec<String>`: Browser=`["--cookies-from-browser", spec]`、File=`["--cookies", path]`
- `mpv_arg(&self) -> String`: Browser=`--ytdl-raw-options-append=cookies-from-browser=<spec>`、File=`--ytdl-raw-options-append=cookies=<path>`
- 表示用ラベル: Browserは既存の`browser()`(spec先頭のブラウザ名)、Fileはファイル名(basename)を返す。`CookieState::label()`/`describe()`/`login_required_message()`はこのラベルを使うよう調整する
- `Target::empty_message()`も`CookieSource`を受け取り、ラベルで確認先を示す。file方式ではブラウザのログイン状態は無関係で、確認するのはcookies.txtの中身
- `CookieState::refusal()`のOff側は設定先として`[cookies] browser`と`file`の両方を出す。ブラウザを置けない環境の利用者にも設定方法が伝わるようにする(ステータス行は80桁に収まる長さを保つ)
- `cookie_store_failure()`も`CookieSource`を受け取る。`Operation not permitted`/`Permission denied`がcookie由来かの判定は、browser方式では行内の"cookie"、file方式では設定したパスとの一致で見る(ファイル名は利用者任せで"cookie"を含むとは限らない)
- Safari特有のTCC(フルディスクアクセス)案内は`describe()`内でBrowser("safari")の場合のみ出す。File方式では出さない(無関係)

## 影響範囲

- `src/cookies.rs`: CookieSource enum化、上記メソッド
- `src/settings.rs`: `RawCookies`に`file: Option<String>`追加、`validate_cookies`で排他判定・ファイル存在チェック・notice
- `src/search.rs`/`src/mpv.rs`: `CookieSource`を経由する呼び出し箇所はメソッド経由のままで変更不要な想定(実装時に確認)

## 対象外(v1)

- File方式の環境変数による上書き
- cookies.txtの中身の形式検証(Netscape形式かどうかのチェックはyt-dlp自身に任せる)
