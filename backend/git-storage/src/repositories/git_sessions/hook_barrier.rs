use super::broker::Operation;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use tokio::sync::oneshot;

struct Barrier {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

/// The receive seam a test holds. Every phase uses this one map and wait.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    PreReceive,
    PostReceive,
    Prepared,
}

static BARRIERS: LazyLock<Mutex<HashMap<(PathBuf, Phase), Barrier>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn before_phase(operation: &Operation, phase: Phase) {
    let barrier = BARRIERS
        .lock()
        .unwrap()
        .remove(&(operation.path.clone(), phase));
    if let Some(barrier) = barrier {
        barrier.entered.send(()).unwrap();
        barrier.release.await.unwrap();
    }
}

/// Holds the next hook of this phase for the repository at `path` as it enters.
/// `reached` resolves once it is held, and sending on `release` lets it continue.
pub fn hold_next_hook(path: PathBuf, phase: Phase) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, reached) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    BARRIERS.lock().unwrap().insert(
        (path, phase),
        Barrier {
            entered,
            release: wait,
        },
    );
    (reached, release)
}
