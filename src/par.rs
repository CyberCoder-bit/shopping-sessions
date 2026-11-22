//! Minimal work-sharing: `threads` scoped threads pull task indices from an atomic counter.
//! Each thread owns one state built by `init` (inside the thread, so its buffers are
//! first-touched there); all states are returned for merging.

use std::sync::atomic::{AtomicUsize, Ordering};

pub fn parallel_for<S, I, F>(threads: usize, n_tasks: usize, init: I, f: F) -> Vec<S>
where
    S: Send,
    I: Fn() -> S + Sync,
    F: Fn(&mut S, usize) + Sync,
{
    let next = AtomicUsize::new(0);
    let threads = threads.max(1).min(n_tasks.max(1));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut state = init();
                    loop {
                        let task = next.fetch_add(1, Ordering::Relaxed);
                        if task >= n_tasks {
                            break;
                        }
                        f(&mut state, task);
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("worker panicked")).collect()
    })
}
