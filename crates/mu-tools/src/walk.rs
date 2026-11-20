//! The directory walk options and the runner for them, shared by the search tools and context discovery.
use omp_xfs::walk;
use std::future::Future;

pub fn walk_options() -> walk::Options {
    walk::Options { hidden: false, gitignore: true, skip_git: true, ..Default::default() }
}

/// omp-xfs spawns its walk workers on the ambient runtime, and mu's is single-threaded. Each walk gets a
/// multi-threaded runtime of its own, entered from a scoped thread so this works inside or outside a runtime,
/// and gone when the walk ends: an idle mu keeps no worker threads (one per CPU).
pub fn block_on_walk<F: Future + Send>(f: F) -> F::Output
where
    F::Output: Send,
{
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .thread_name("mu-walk")
                .enable_all()
                .build()
                .expect("walk runtime");
            let out = rt.block_on(f);
            rt.shutdown_background();
            out
        })
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}
