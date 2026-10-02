use crate::{Error, internal::HttpClient};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use http::{HeaderMap, Method};
use std::{future::Future, time::Duration};
use tokio_util::sync::CancellationToken;

pub async fn poll<T, F, Fut>(
    timeout: Duration,
    cancellation: &CancellationToken,
    mut read: F,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(T, bool, Duration), Error>>,
{
    if timeout.is_zero() {
        return Err(Error::invalid("poll timeout"));
    }
    let operation = async {
        loop {
            let (value, done, delay) = read().await?;
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            if done {
                return Ok(value);
            }
            if delay.is_zero() {
                return Err(Error::invalid("poll interval"));
            }
            tokio::time::sleep(delay).await;
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled()),
        result = tokio::time::timeout(timeout, operation) => result.unwrap_or_else(|_| Err(Error::new("TIMEOUT", "operation timed out"))),
    }
}

/// Holds cleanup for one known pending approval. Disarm after credential completion, not resource delivery.
pub struct ApprovalCleanup {
    armed: bool,
    cancellation: CancellationToken,
    cleanup: Shared<BoxFuture<'static, Result<(), Error>>>,
    runtime: tokio::runtime::Handle,
}

impl ApprovalCleanup {
    pub fn new(client: HttpClient, approval_id: &str) -> Result<Self, Error> {
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::invalid)?;
        if approval_id.is_empty() || matches!(approval_id, "." | "..") {
            return Err(Error::invalid("approval ID"));
        }
        let id: String = approval_id
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let cleanup = async move {
            let cancellation = CancellationToken::new();
            // This deadline includes token retrieval; it does not reuse the cancelled payment's token.
            tokio::time::timeout(
                Duration::from_secs(5),
                client.request(
                    Method::POST,
                    &format!("/v1/approvals/{id}/cancel"),
                    None,
                    HeaderMap::new(),
                    0,
                    &cancellation,
                ),
            )
            .await
            .map_err(|_| Error::new("TIMEOUT", "approval cancellation timed out"))??;
            Ok(())
        }
        .boxed()
        .shared();
        Ok(Self {
            armed: true,
            cancellation: CancellationToken::new(),
            cleanup,
            runtime,
        })
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn cancel(&self) -> Result<(), Error> {
        self.cancellation.cancel();
        if self.armed {
            self.cleanup.clone().await
        } else {
            Ok(())
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ApprovalCleanup {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
            // Spawn only on the caller-owned runtime; shutdown may drop this best-effort attempt.
            self.runtime.spawn(self.cleanup.clone());
        }
    }
}

fn cancelled() -> Error {
    Error::new("CANCELLED", "operation cancelled")
}
