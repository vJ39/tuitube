//! クリップボードへの書き込み。yt-dlp / curl と同じく外部プロセスを呼ぶ形にして、
//! テストでは偽物へ差し替える。

use std::future::Future;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

pub const MISSING_PBCOPY: &str = "pbcopy が見つかりません";
/// キー処理の中で待つので、終わらない pbcopy に画面ごと付き合わないための期限。
pub const PBCOPY_TIMEOUT: Duration = Duration::from_secs(2);

/// 本番は pbcopy、テストは書かれた内容を溜める偽物。
pub trait Clipboard {
    fn copy(&self, text: String) -> impl Future<Output = std::io::Result<()>> + Send;
}

pub struct Pbcopy;

impl Clipboard for Pbcopy {
    async fn copy(&self, text: String) -> std::io::Result<()> {
        within_timeout(write_to_pbcopy(text)).await
    }
}

/// 期限が来たら諦める。落とした子プロセスは kill_on_drop が始末する。
/// 期限以外の失敗は理由をそのまま返す。ErrorKind を潰すと pbcopy の不在を言えなくなる。
async fn within_timeout(copying: impl Future<Output = std::io::Result<()>>) -> std::io::Result<()> {
    match timeout(PBCOPY_TIMEOUT, copying).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "pbcopy が {} 秒で終わりませんでした",
                PBCOPY_TIMEOUT.as_secs()
            ),
        )),
    }
}

async fn write_to_pbcopy(text: String) -> std::io::Result<()> {
    // 取り込み中にアプリを終了しても pbcopy を孤児にしない。
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("pbcopy の標準入力を開けません"))?;
    stdin.write_all(text.as_bytes()).await?;
    // 標準入力が閉じるまで pbcopy は終わらない。
    drop(stdin);
    let status = child.wait().await?;
    if status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "pbcopy が終了コード {} で終わりました",
        status.code().unwrap_or(-1)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_copy_that_never_finishes_is_given_up_on() {
        let error = within_timeout(std::future::pending::<std::io::Result<()>>())
            .await
            .expect_err("期限で打ち切る");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("2 秒"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_pbcopy_keeps_its_kind() {
        // NotFound のままでないと「pbcopy が見つかりません」と言えなくなる。
        let error = within_timeout(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))
        })
        .await
        .expect_err("理由を出す");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test(start_paused = true)]
    async fn a_copy_inside_the_deadline_goes_through() {
        within_timeout(async {
            tokio::time::sleep(PBCOPY_TIMEOUT / 2).await;
            Ok(())
        })
        .await
        .expect("期限内なら通す");
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CopyResult {
        Ok,
        /// pbcopy そのものが PATH に無い。
        Missing,
        /// 起動はしたが 0 以外で終わった。
        Failed,
    }

    /// 起動せず台本どおりに振る舞う pbcopy。書かれた内容を溜める。
    #[derive(Clone)]
    pub struct FakeClipboard {
        copied: Arc<Mutex<Vec<String>>>,
        result: CopyResult,
    }

    impl FakeClipboard {
        pub fn new(result: CopyResult) -> Self {
            Self {
                copied: Arc::new(Mutex::new(Vec::new())),
                result,
            }
        }

        pub fn copied(&self) -> Vec<String> {
            self.copied.lock().expect("溜め込み先").clone()
        }
    }

    impl Clipboard for FakeClipboard {
        fn copy(&self, text: String) -> impl Future<Output = std::io::Result<()>> + Send {
            self.copied.lock().expect("溜め込み先").push(text);
            let result = self.result;
            async move {
                match result {
                    CopyResult::Ok => Ok(()),
                    CopyResult::Missing => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no such file",
                    )),
                    CopyResult::Failed => Err(std::io::Error::other(
                        "pbcopy が終了コード 1 で終わりました",
                    )),
                }
            }
        }
    }
}
