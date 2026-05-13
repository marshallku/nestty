//! Bounded worker pool for blocking action handlers.
//!
//! Jobs implement `Cancelable` so the pool can hand them back unchanged when
//! the bounded queue is full — the caller invokes `cancel` on the rejected
//! job synchronously (on the caller's thread), which gives time-critical
//! callers an immediate path to synthesize an error response without ever
//! blocking on the channel.
//!
//! Lifecycle: workers exit when the sender is dropped (sees `RecvError`).
//! `shutdown()` is idempotent and only closes the sender; `Drop` then joins.
//! Explicit `shutdown()` matters because the registry may hold the pool inside
//! a handler-capture cycle that prevents automatic drop.

use crossbeam_channel::{Sender, TrySendError, bounded};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

pub trait Cancelable: Send + 'static {
    /// Runs on a worker thread. Consumes the job.
    fn run(self: Box<Self>);
    /// Runs on the caller's thread when the queue rejects the job. Same
    /// completion contract as `run` (responder fires, completion events
    /// publish) but with an overload-flavored error.
    fn cancel(self: Box<Self>);
}

type Job = Box<dyn Cancelable>;

pub struct PoolStats {
    pub active: usize,
    pub queued: usize,
    pub capacity: usize,
    pub workers: usize,
}

pub struct ThreadPool {
    tx: Mutex<Option<Sender<Job>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    active: Arc<AtomicUsize>,
    capacity: usize,
    worker_count: usize,
}

impl ThreadPool {
    pub fn new(workers: usize, queue_cap: usize) -> Arc<Self> {
        assert!(workers >= 1, "ThreadPool requires at least one worker");
        assert!(queue_cap >= 1, "ThreadPool requires queue_cap >= 1");
        let (tx, rx) = bounded::<Job>(queue_cap);
        let active = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(workers);
        for i in 0..workers {
            let rx = rx.clone();
            let active = active.clone();
            let handle = thread::Builder::new()
                .name(format!("nestty-pool-{i}"))
                .spawn(move || worker_loop(rx, active))
                .expect("spawn ThreadPool worker");
            handles.push(handle);
        }
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            workers: Mutex::new(handles),
            active,
            capacity: queue_cap,
            worker_count: workers,
        })
    }

    /// Non-blocking. Returns the job back on Full so the caller can `cancel`
    /// it synchronously. Returns `Err(job)` after `shutdown()` so callers
    /// during teardown still get a defined cancellation path.
    pub fn try_execute(&self, job: Job) -> Result<(), Job> {
        let guard = self.tx.lock().unwrap();
        let Some(tx) = guard.as_ref() else {
            return Err(job);
        };
        match tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job)) | Err(TrySendError::Disconnected(job)) => Err(job),
        }
    }

    pub fn stats(&self) -> PoolStats {
        let queued = self
            .tx
            .lock()
            .unwrap()
            .as_ref()
            .map(|tx| tx.len())
            .unwrap_or(0);
        PoolStats {
            active: self.active.load(Ordering::SeqCst),
            queued,
            capacity: self.capacity,
            workers: self.worker_count,
        }
    }

    /// Idempotent. Closes the sender so workers exit after draining the
    /// queue. Does NOT join — `Drop` does that.
    pub fn shutdown(&self) {
        drop(self.tx.lock().unwrap().take());
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.shutdown();
        let handles = std::mem::take(&mut *self.workers.lock().unwrap());
        // Drop can fire from within a worker thread if a running job
        // releases the last `Arc<ThreadPool>`. Joining the current thread
        // would deadlock; detach it instead — `shutdown()` already closed
        // the sender, so it will exit naturally on the next `recv`.
        let current = thread::current().id();
        for h in handles {
            if h.thread().id() == current {
                continue;
            }
            let _ = h.join();
        }
    }
}

fn worker_loop(rx: crossbeam_channel::Receiver<Job>, active: Arc<AtomicUsize>) {
    while let Ok(job) = rx.recv() {
        active.fetch_add(1, Ordering::SeqCst);
        // catch_unwind keeps the worker alive across a panicking handler.
        // We honor the existing "panic = no responder fires" contract by
        // not synthesizing a fallback response here; the lost responder is
        // a known, documented gap (see action_registry.rs module preamble).
        let _ = catch_unwind(AssertUnwindSafe(|| job.run()));
        active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    struct NoopJob {
        ran: Arc<AtomicBool>,
        canceled: Arc<AtomicBool>,
    }
    impl Cancelable for NoopJob {
        fn run(self: Box<Self>) {
            self.ran.store(true, Ordering::SeqCst);
        }
        fn cancel(self: Box<Self>) {
            self.canceled.store(true, Ordering::SeqCst);
        }
    }

    struct SleepJob(Duration, Arc<AtomicUsize>);
    impl Cancelable for SleepJob {
        fn run(self: Box<Self>) {
            thread::sleep(self.0);
            self.1.fetch_add(1, Ordering::SeqCst);
        }
        fn cancel(self: Box<Self>) {}
    }

    #[test]
    fn job_runs_on_worker() {
        let pool = ThreadPool::new(2, 4);
        let ran = Arc::new(AtomicBool::new(false));
        let canceled = Arc::new(AtomicBool::new(false));
        pool.try_execute(Box::new(NoopJob {
            ran: ran.clone(),
            canceled: canceled.clone(),
        }))
        .ok()
        .unwrap();
        for _ in 0..50 {
            if ran.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ran.load(Ordering::SeqCst), "job did not run");
        assert!(!canceled.load(Ordering::SeqCst));
    }

    #[test]
    fn saturation_returns_job_back() {
        // 1 worker, 1 queue slot. Once the worker has picked up the first
        // job and the queue slot holds the second, the third try_execute
        // must bounce back so the caller can `cancel` it.
        let pool = ThreadPool::new(1, 1);
        let counter = Arc::new(AtomicUsize::new(0));
        // Long-held jobs (1s) so timing slack doesn't free a slot mid-test.
        pool.try_execute(Box::new(SleepJob(Duration::from_secs(1), counter.clone())))
            .ok()
            .unwrap();
        // Let the worker pull job #1 out of the queue so slot frees up for #2.
        thread::sleep(Duration::from_millis(80));
        pool.try_execute(Box::new(SleepJob(Duration::from_secs(1), counter.clone())))
            .ok()
            .unwrap();
        // Worker still busy, slot still occupied → third must come back.
        let third = pool.try_execute(Box::new(SleepJob(Duration::from_secs(1), counter.clone())));
        assert!(third.is_err(), "third job should have been rejected");
    }

    #[test]
    fn worker_survives_panic() {
        struct PanicJob;
        impl Cancelable for PanicJob {
            fn run(self: Box<Self>) {
                panic!("intentional");
            }
            fn cancel(self: Box<Self>) {}
        }
        let pool = ThreadPool::new(1, 4);
        pool.try_execute(Box::new(PanicJob)).ok().unwrap();
        // Sibling job after the panic should still execute.
        let ran = Arc::new(AtomicBool::new(false));
        let canceled = Arc::new(AtomicBool::new(false));
        thread::sleep(Duration::from_millis(20));
        pool.try_execute(Box::new(NoopJob {
            ran: ran.clone(),
            canceled,
        }))
        .ok()
        .unwrap();
        for _ in 0..50 {
            if ran.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ran.load(Ordering::SeqCst),
            "sibling did not run after panic"
        );
    }

    #[test]
    fn shutdown_is_idempotent_and_drains() {
        let pool = ThreadPool::new(2, 4);
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            pool.try_execute(Box::new(SleepJob(
                Duration::from_millis(20),
                counter.clone(),
            )))
            .ok()
            .unwrap();
        }
        pool.shutdown();
        pool.shutdown(); // second call must not panic
        // After try_execute is shut, jobs come back via Err.
        let ran = Arc::new(AtomicBool::new(false));
        let canceled = Arc::new(AtomicBool::new(false));
        let rejected = pool.try_execute(Box::new(NoopJob {
            ran: ran.clone(),
            canceled: canceled.clone(),
        }));
        assert!(rejected.is_err());
        // Existing queued jobs still complete (drain).
        drop(pool); // joins
        assert_eq!(counter.load(Ordering::SeqCst), 3, "queued jobs must drain");
    }
}
