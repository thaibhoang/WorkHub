//! Quiescing before a restart: once [`Drain::start`] is called, workers stop
//! taking new work, and [`Drain::busy`] counts what is still in flight.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Debug, Default)]
pub struct Drain {
    draining: AtomicBool,
    busy: AtomicUsize,
}

/// Counts as in flight until dropped.
#[derive(Debug)]
pub struct DrainGuard(Arc<Drain>);

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drain {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Stop taking new work (for good: the process is about to be replaced),
    /// unless [`Drain::cancel`] is called.
    pub fn start(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// Back to normal (the restart was abandoned).
    pub fn cancel(&self) {
        self.draining.store(false, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Work in flight.
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }

    /// Claim a unit of new work: `None` while draining. The count goes up before
    /// the check, so work that got a guard is always counted by a drain that
    /// started at the same time.
    pub fn enter(self: &Arc<Self>) -> Option<DrainGuard> {
        let guard = self.track();
        (!self.is_draining()).then_some(guard)
    }

    /// Count work that must finish even while draining (it was already accepted).
    pub fn track(self: &Arc<Self>) -> DrainGuard {
        self.busy.fetch_add(1, Ordering::SeqCst);
        DrainGuard(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_count_until_dropped_and_draining_refuses_new_work() {
        let d = Drain::new();
        let a = d.enter().unwrap();
        let b = d.track();
        assert_eq!(d.busy(), 2);
        d.start();
        assert!(d.enter().is_none());
        assert_eq!(d.busy(), 2, "a refused claim is not counted");
        drop(a);
        drop(b);
        assert_eq!(d.busy(), 0);
        d.cancel();
        assert!(d.enter().is_some());
    }
}
