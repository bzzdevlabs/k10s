//! A node's instance memory budget (docs/NODE.md, "Memory"), shared by the hosted node and k10-metal: one reserved
//! instance per service placed on the node, burst instances from the shared rest of the limit.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering::*};

/// What the memory of a live instance is charged to (see [`MemBudget`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charge {
    #[default]
    None,
    /// Its service's reservation.
    Reserved,
    /// The shared pool (burst instances).
    Shared,
}

const INACTIVE: u8 = 0;
const FREE: u8 = 1;
const IN_USE: u8 = 2;
const RELEASED: u8 = 3;
const RELEASED_IN_USE: u8 = 4;

/// A service version's reservation of one instance (`bytes == 0`: none), active from install to release.
#[derive(Debug)]
pub struct Reservation {
    bytes: u64,
    state: AtomicU8,
}

impl Reservation {
    pub fn new(bytes: u64) -> Reservation {
        Reservation { bytes, state: AtomicU8::new(INACTIVE) }
    }

    /// Bytes held while active, else 0.
    pub fn bytes(&self) -> u64 {
        match self.state.load(Relaxed) {
            FREE | IN_USE => self.bytes,
            _ => 0,
        }
    }

    pub fn in_use(&self) -> bool {
        matches!(self.state.load(Relaxed), IN_USE | RELEASED_IN_USE)
    }

    fn take(&self) -> bool {
        self.bytes != 0 && self.state.compare_exchange(FREE, IN_USE, AcqRel, Relaxed).is_ok()
    }
}

/// The instance memory limit with per-service reservations and a shared pool for burst instances.
#[derive(Debug, Default)]
pub struct MemBudget {
    limit: AtomicU64,
    /// Active reservations + shared instances + retiring instances.
    committed: AtomicU64,
    reserved: AtomicU64,
    /// Instances of released reservations still alive: counted when admitting bursts, not by [`MemBudget::over`].
    retiring: AtomicU64,
    refused: AtomicU64,
}

impl MemBudget {
    pub const fn new(limit: u64) -> MemBudget {
        MemBudget {
            limit: AtomicU64::new(limit),
            committed: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            retiring: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Relaxed)
    }

    /// Sets the limit before anything is installed or charged.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Relaxed);
    }

    pub fn reserved(&self) -> u64 {
        self.reserved.load(Relaxed)
    }

    pub fn committed(&self) -> u64 {
        self.committed.load(Relaxed)
    }

    pub fn retiring(&self) -> u64 {
        self.retiring.load(Relaxed)
    }

    /// Bytes of burst instances.
    pub fn shared_used(&self) -> u64 {
        self.committed().saturating_sub(self.reserved()).saturating_sub(self.retiring())
    }

    /// Reservations beyond the limit (a reloaded or cluster state that does not fit: no bursts possible).
    pub fn overcommit(&self) -> u64 {
        self.reserved().saturating_sub(self.limit())
    }

    pub fn refused(&self) -> u64 {
        self.refused.load(Relaxed)
    }

    /// Over the limit (an apply added reservations while bursts held the pool): burst instances are destroyed.
    #[inline]
    pub fn over(&self) -> bool {
        self.committed.load(Relaxed).saturating_sub(self.retiring.load(Relaxed)) > self.limit()
    }

    pub fn install(&self, r: &Reservation) {
        if r.bytes != 0 && r.state.compare_exchange(INACTIVE, FREE, AcqRel, Relaxed).is_ok() {
            self.committed.fetch_add(r.bytes, Relaxed);
            self.reserved.fetch_add(r.bytes, Relaxed);
        }
    }

    /// Ends `r`: a free unit is uncommitted now, one in use turns retiring until its instance is freed.
    pub fn release(&self, r: &Reservation) {
        let mut cur = r.state.load(Relaxed);
        loop {
            let next = match cur {
                FREE => RELEASED,
                IN_USE => RELEASED_IN_USE,
                INACTIVE => RELEASED,
                _ => return,
            };
            match r.state.compare_exchange_weak(cur, next, AcqRel, Relaxed) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
        match cur {
            FREE => {
                self.committed.fetch_sub(r.bytes, Relaxed);
                self.reserved.fetch_sub(r.bytes, Relaxed);
            }
            IN_USE => {
                self.retiring.fetch_add(r.bytes, Relaxed);
                self.reserved.fetch_sub(r.bytes, Relaxed);
            }
            _ => {}
        }
    }

    /// Whether [`MemBudget::charge`] would succeed now (nothing charged).
    pub fn admits(&self, r: &Reservation, bytes: u64) -> bool {
        (r.bytes != 0 && r.state.load(Relaxed) == FREE) || self.committed().saturating_add(bytes) <= self.limit()
    }

    /// Charges a new instance of `bytes` to `r` if free, else to the shared pool; `None`: no room (the caller
    /// counts the refusal with [`MemBudget::refuse`], once per request).
    pub fn charge(&self, r: &Reservation, bytes: u64) -> Option<Charge> {
        if r.take() {
            return Some(Charge::Reserved);
        }
        let mut cur = self.committed.load(Relaxed);
        loop {
            if cur.saturating_add(bytes) > self.limit() {
                return None;
            }
            match self.committed.compare_exchange_weak(cur, cur + bytes, Relaxed, Relaxed) {
                Ok(_) => return Some(Charge::Shared),
                Err(c) => cur = c,
            }
        }
    }

    /// Counts a request refused a new instance for lack of room.
    pub fn refuse(&self) {
        self.refused.fetch_add(1, Relaxed);
    }

    /// Gives back the charge of an instance being freed.
    pub fn refund(&self, r: &Reservation, bytes: u64, c: Charge) {
        match c {
            Charge::None => {}
            Charge::Shared => {
                self.committed.fetch_sub(bytes, Relaxed);
            }
            Charge::Reserved => {
                if r.state.compare_exchange(IN_USE, FREE, AcqRel, Relaxed).is_err()
                    && r.state.compare_exchange(RELEASED_IN_USE, RELEASED, AcqRel, Relaxed).is_ok()
                {
                    self.committed.fetch_sub(bytes, Relaxed);
                    self.retiring.fetch_sub(bytes, Relaxed);
                }
            }
        }
    }

    /// While over, uncommits a shared instance the caller then frees uncharged; false otherwise.
    pub fn reclaim_over(&self, bytes: u64) -> bool {
        let mut cur = self.committed.load(Relaxed);
        while cur.saturating_sub(self.retiring.load(Relaxed)) > self.limit() {
            match self.committed.compare_exchange_weak(cur, cur.saturating_sub(bytes), Relaxed, Relaxed) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
        false
    }

    /// Moves a shared instance into `r` if free; true if it is now [`Charge::Reserved`].
    pub fn promote(&self, r: &Reservation, bytes: u64) -> bool {
        let ok = r.take();
        if ok {
            self.committed.fetch_sub(bytes, Relaxed);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_reservations_and_shared_pool() {
        const U: u64 = 100;
        let b = MemBudget::new(1000);
        let (a, h) = (Reservation::new(U), Reservation::new(U));
        assert_eq!(a.bytes(), 0);
        b.install(&a);
        b.install(&h);
        b.install(&h);
        assert_eq!((b.reserved(), b.committed(), a.bytes()), (200, 200, U));
        // The hog: its reservation, then the 800-byte shared pool.
        assert!(b.admits(&h, U));
        assert_eq!(b.charge(&h, U), Some(Charge::Reserved));
        let mut shared = 0;
        while b.charge(&h, U) == Some(Charge::Shared) {
            shared += 1;
        }
        assert_eq!((shared, b.refused()), (8, 0));
        assert!(!b.admits(&h, U));
        b.refuse();
        assert_eq!(b.refused(), 1);
        // `a` still gets its reserved instance, but no burst.
        assert!(b.admits(&a, U));
        assert_eq!(b.charge(&a, U), Some(Charge::Reserved));
        assert!(!b.admits(&a, U));
        assert_eq!(b.charge(&a, U), None);
        assert_eq!((b.committed(), b.shared_used()), (1000, 800));
        b.refund(&h, U, Charge::Shared);
        assert!(b.admits(&a, U));
        assert_eq!(b.charge(&a, U), Some(Charge::Shared));
        // The hog's reserved instance dies; a shared one moves into the reservation.
        b.refund(&h, U, Charge::Reserved);
        assert_eq!(b.committed(), 1000);
        assert!(b.promote(&h, U));
        assert_eq!(b.committed(), 900);
        assert!(!b.promote(&h, U));
        // Released while its instance lives: retiring until that instance is freed.
        b.release(&a);
        assert_eq!((b.reserved(), b.retiring(), b.committed(), a.bytes()), (100, 100, 900, 0));
        assert_eq!(b.charge(&a, U), Some(Charge::Shared));
        b.refund(&a, U, Charge::Shared);
        b.refund(&a, U, Charge::Reserved);
        assert_eq!((b.committed(), b.retiring()), (800, 0));
        b.release(&a);
        assert_eq!(b.committed(), 800);
    }

    #[test]
    fn updates_do_not_count_twice_and_relief_claims_the_overage() {
        const U: u64 = 100;
        let b = MemBudget::new(500);
        let (old, new) = (Reservation::new(U), Reservation::new(U));
        b.install(&old);
        assert_eq!(b.charge(&old, U), Some(Charge::Reserved));
        while b.charge(&old, U) == Some(Charge::Shared) {}
        // An update installs the new version before releasing the old one.
        b.install(&new);
        assert!(b.over());
        b.release(&old);
        assert!(!b.over(), "the old version's live instance is retiring, not over");
        assert_eq!(b.charge(&new, U), Some(Charge::Reserved));
        assert_eq!(b.charge(&new, U), None, "retiring bytes still bound bursts");
        // Reservations added while bursts hold the pool: relief claims exactly the overage.
        b.install(&Reservation::new(3 * U));
        assert!(b.over());
        let mut n = 0;
        while b.reclaim_over(U) {
            n += 1;
        }
        assert_eq!((n, b.over()), (3, false));
        // State that does not fit (reload, cluster): reported as over-commit.
        b.install(&Reservation::new(1000));
        assert_eq!(b.overcommit(), 400 + 1000 - 500);
    }

    #[test]
    fn limit_set_before_use() {
        static B: MemBudget = MemBudget::new(0);
        let r = Reservation::new(10);
        assert!(!B.admits(&r, 10));
        B.set_limit(15);
        B.install(&r);
        assert_eq!((B.limit(), B.reserved()), (15, 10));
        assert_eq!(B.charge(&r, 10), Some(Charge::Reserved));
        assert!(B.admits(&r, 5) && !B.admits(&r, 6));
    }

    #[cfg(feature = "std")]
    #[test]
    fn budget_release_races_with_refund() {
        use std::sync::Arc;
        for _ in 0..2000 {
            let b = Arc::new(MemBudget::new(1 << 20));
            let r = Arc::new(Reservation::new(10));
            b.install(&r);
            assert_eq!(b.charge(&r, 10), Some(Charge::Reserved));
            let t = {
                let (b, r) = (b.clone(), r.clone());
                std::thread::spawn(move || b.refund(&r, 10, Charge::Reserved))
            };
            b.release(&r);
            t.join().unwrap();
            assert_eq!((b.committed(), b.reserved(), b.retiring()), (0, 0, 0));
        }
    }
}
