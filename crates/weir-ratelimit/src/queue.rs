//! Request queuing for rate-limited requests.

use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

/// Request queue backed by `tokio::sync::Notify`.
#[derive(Debug)]
pub struct RequestQueue {
    notify: Notify,
    timeout: Duration,
}

impl RequestQueue {
    /// Create a new request queue with the given timeout.
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            notify: Notify::new(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Wait for the bucket to become available. Returns `false` on timeout.
    pub async fn wait(&self) -> bool {
        timeout(self.timeout, self.notify.notified())
            .await
            .is_ok()
    }

    /// Wake one waiting request.
    pub fn wake_one(&self) {
        self.notify.notify_one();
    }

    /// Wake all waiting requests (e.g., on bucket reset).
    pub fn wake_all(&self) {
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_times_out() {
        let queue = RequestQueue::new(10); // 10ms timeout
        assert!(!queue.wait().await);
    }

    #[tokio::test]
    async fn wake_one_unblocks_waiter() {
        let queue = std::sync::Arc::new(RequestQueue::new(5000));
        let queue2 = queue.clone();

        let handle = tokio::spawn(async move {
            queue2.wait().await
        });

        // Small delay to ensure the waiter is registered
        tokio::time::sleep(Duration::from_millis(10)).await;
        queue.wake_one();

        assert!(handle.await.unwrap());
    }
}
