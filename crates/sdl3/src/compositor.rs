//! One lock for every request sequence that ends in a commit of an SDL
//! window's `wl_surface`.
//!
//! A present is several requests on that surface - acquire point, attach,
//! damage, frame callback, commit - and libwayland orders only single
//! requests. A commit from another thread landing inside a present commits
//! half of it: under explicit sync that is an acquire point with no buffer, a
//! protocol error that closes the display connection. SDL commits the surface
//! itself on show, hide, maximize, fullscreen, pointer warps and some
//! configure events, and Rosemary presents from Metal workers and guest GL
//! threads, so both sides take this lock.
//!
//! Re-entrant on one thread: SDL dispatches window events, and with them hit
//! test callbacks, inside calls that already hold it.

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard};

static COMMITS: Mutex<()> = Mutex::new(());

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Held for as long as a sequence of surface requests must not be split.
pub struct Guard {
    _held: Option<MutexGuard<'static, ()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Hold across any call that may commit an SDL window's surface: a WSI
/// present, an EGL swap, or an SDL call that commits or dispatches events.
pub fn guard() -> Guard {
    let outer = DEPTH.with(|depth| {
        let outer = depth.get() == 0;
        depth.set(depth.get() + 1);
        outer
    });
    Guard {
        _held: outer.then(|| COMMITS.lock().unwrap_or_else(|e| e.into_inner())),
    }
}

/// The lock if it is free now (or this thread holds it already), without
/// waiting. For a caller that has something better to do than wait out a
/// present: polling events on the main thread, which would otherwise stall
/// the game's frame for as long as the driver blocks in that present.
pub fn try_guard() -> Option<Guard> {
    let outer = DEPTH.with(|depth| depth.get() == 0);
    let held = if outer {
        match COMMITS.try_lock() {
            Ok(held) => Some(held),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    } else {
        None
    };
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    Some(Guard { _held: held })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_guard_never_waits_on_another_thread() {
        let held = guard();
        let other = std::thread::spawn(|| try_guard().is_none()).join().unwrap();
        assert!(other, "another thread got the lock this one holds");
        // This thread holds it, so it may take it again.
        assert!(try_guard().is_some());
        drop(held);
        let free = std::thread::spawn(|| try_guard().is_some()).join().unwrap();
        assert!(free, "a free lock was refused");
    }

    #[test]
    fn a_thread_can_take_the_lock_it_holds() {
        let outer = guard();
        let inner = guard();
        drop(inner);
        drop(outer);
        // Released: another thread can take it now.
        std::thread::spawn(|| drop(guard())).join().unwrap();
    }

    #[test]
    fn another_thread_waits_for_the_holder() {
        let held = guard();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _guard = guard();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "a second thread took a held lock"
        );
        drop(held);
        rx.recv().unwrap();
        waiter.join().unwrap();
    }
}
