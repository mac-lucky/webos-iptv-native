//! **The viewer's playback preferences, as the player reads them.**
//!
//! The persisted value is the app's (a profile setting); the copy here is the one the player reads
//! without a lock: a process-global [`LiveCell`] the app publishes into whenever the value changes
//! or a profile is seated (`player::preview`'s trailer autoplay switch is one). Nothing here
//! persists.

use std::marker::PhantomData;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;

/// A value that packs into the one byte a [`LiveCell`] holds.
pub trait Packed: Copy {
    /// What the cell holds before anything has published.
    const SEED: u8;
    fn to_u8(self) -> u8;
    fn from_u8(b: u8) -> Self;
}

/// A lock-free, process-global copy of one preference.
pub struct LiveCell<T: Packed> {
    a: AtomicU8,
    /// Names the global in the test-lock panic.
    #[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
    name: &'static str,
    _t: PhantomData<fn() -> T>,
}

impl<T: Packed> LiveCell<T> {
    pub const fn new(name: &'static str) -> Self {
        LiveCell { a: AtomicU8::new(T::SEED), name, _t: PhantomData }
    }

    /// Safe from any thread; a reader takes no lock.
    pub fn get(&self) -> T {
        T::from_u8(self.a.load(Relaxed))
    }

    /// Publish `v`. A cell is a process global read without a lock, so in tests a write without
    /// `testlock::serial()` would land in the middle of another test's read; the assertion turns
    /// that into a deterministic panic in the culprit.
    pub fn set(&self, v: T) {
        #[cfg(any(test, feature = "test-support"))]
        tv_base::testlock::assert_held(self.name);
        self.a.store(v.to_u8(), Relaxed);
    }
}

impl Packed for bool {
    const SEED: u8 = 1;
    fn to_u8(self) -> u8 {
        self as u8
    }
    fn from_u8(b: u8) -> Self {
        b != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bool_round_trips_through_its_byte_and_is_seeded_on() {
        for v in [false, true] {
            assert_eq!(<bool as Packed>::from_u8(v.to_u8()), v);
        }
        assert!(<bool as Packed>::from_u8(<bool as Packed>::SEED), "on until the app publishes");
    }
}
