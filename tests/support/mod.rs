//! Shared helpers for the integration and unit tests.

/// Drive a future to completion. The tools read files and spawn processes, so the tests
/// need a reactor; a current-thread runtime is all any of them require.
pub fn block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a test runtime")
        .block_on(future)
}
