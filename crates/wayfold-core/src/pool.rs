//! Bounded blocking worker pools. Filesystem and network calls never run on the UI thread
//! or on GPUI's executor; a stalled network mount can only occupy the `remote` pool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Cooperative cancellation, checked between directory chunks and file chunks.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub struct Pool {
    tx: async_channel::Sender<Job>,
}

impl Pool {
    pub fn new(name: &str, threads: usize) -> Self {
        let (tx, rx) = async_channel::unbounded::<Job>();
        for i in 0..threads {
            let rx = rx.clone();
            thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv_blocking() {
                        job();
                    }
                })
                .expect("spawn worker thread");
        }
        Self { tx }
    }

    pub fn execute(&self, job: impl FnOnce() + Send + 'static) {
        // The receivers live as long as the process, so send only fails at shutdown.
        let _ = self.tx.send_blocking(Box::new(job));
    }

    /// Run `f` on the pool and await its result from async (GPUI foreground) code.
    pub fn run<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(&self, f: F) -> impl Future<Output = Option<T>> + use<T, F> {
        let (tx, rx) = async_channel::bounded(1);
        self.execute(move || {
            let _ = tx.send_blocking(f());
        });
        async move { rx.recv().await.ok() }
    }
}

/// Local disks: listing, stat, copy.
pub fn local() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool::new("wp-fs", 6))
}

/// Network mounts: kernel calls may block for a long time and cannot be interrupted.
pub fn remote() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool::new("wp-net", 4))
}

/// CPU-bound decoding (preview, checksums, archive listing).
pub fn cpu() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool::new("wp-cpu", 4))
}

/// Pick the pool for a path: network mounts get the isolated pool.
pub fn for_path(path: &std::path::Path) -> &'static Pool {
    if crate::fs::is_network_path(path) { remote() } else { local() }
}
