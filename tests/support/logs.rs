//! Captures what the code under test logs, so a test can read it or wait for the event that
//! shows a background task reached a point.
use std::time::Duration;

/// A log file and the subscriber that writes to it. Attach `dispatch` to a future with
/// `WithSubscriber`, or make it the thread's default to cover the tasks a current-thread
/// runtime polls.
pub struct Log {
    file: tempfile::NamedTempFile,
    pub dispatch: tracing::Dispatch,
}

impl Log {
    pub fn capture() -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        let writer = file.reopen().unwrap();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.try_clone().unwrap())
                .finish(),
        );
        Self { file, dispatch }
    }

    pub fn text(&self) -> String {
        std::fs::read_to_string(self.file.path()).unwrap()
    }

    /// Waits until a logged line satisfies `seen`, and panics with `missing` if none does. The
    /// cap leaves a loaded runner plenty of room; the wait ends as soon as the line appears.
    pub async fn wait_for(&self, seen: impl Fn(&str) -> bool, missing: &str) {
        tokio::time::timeout(Duration::from_secs(120), async {
            while !self.text().lines().any(&seen) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{missing}"));
    }
}
