//! Runs a very large async test body on its own big thread stack.
use std::future::Future;

/// The default 2 MiB test stack is too small for the deep async state of an exhaustive contract
/// in a debug build, so the body gets a 32 MiB thread and its own runtime.
pub fn run<F: Future<Output = ()>>(body: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(body()))
        })
        .unwrap()
        .join()
        .unwrap();
}
