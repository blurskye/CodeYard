use super::broker::Operation;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use tokio::sync::oneshot;

pub(super) struct Barrier {
    pub(super) entered: oneshot::Sender<()>,
    pub(super) release: oneshot::Receiver<()>,
}
pub(super) static BARRIERS: LazyLock<Mutex<HashMap<(PathBuf, bool), Barrier>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn before_authorize(operation: &Operation, post: bool) {
    let barrier = BARRIERS
        .lock()
        .unwrap()
        .remove(&(operation.path.clone(), post));
    if let Some(barrier) = barrier {
        barrier.entered.send(()).unwrap();
        barrier.release.await.unwrap();
    }
}

/// Holds the next hook of this kind for the repository at `path` as it enters.
/// `reached` resolves once it is held, and sending on `release` lets it continue.
pub fn hold_next_hook(path: PathBuf, post: bool) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, reached) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    BARRIERS.lock().unwrap().insert(
        (path, post),
        Barrier {
            entered,
            release: wait,
        },
    );
    (reached, release)
}
