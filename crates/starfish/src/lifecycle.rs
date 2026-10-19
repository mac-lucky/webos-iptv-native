// Copyright (c) 2026 Gleb Linnik
// Copyright (C) 2026 the webos-iptv-native authors
// SPDX-License-Identifier: GPL-3.0-or-later

//! The StarfishMediaAPIs object lifecycle: Load, the callback gate, quarantine and destroy.
//!
//! `this` is an over-allocated buffer constructed in place: the object's size is unknown, so it is
//! never `new`/`delete`d. Each Load gets a 64 KB, 16-byte-aligned slot that is never freed. A late
//! libpf callback thunk carries no generation token, only this address, so an address is reused
//! only for a slot whose object was destroyed with every proof (below), and only after
//! [`REUSE_AFTER_LOADS`] further Loads: by then a late thunk for that object has had whole sessions
//! to arrive and be dropped by its retired gate. Zapping through channels therefore cycles a few
//! slots instead of leaking 64 KB per channel change. A quarantined (or never destroyed) slot is
//! never reused. Slots stay in the registry until process exit; the interposer looks them up
//! without dereferencing their destroyed object storage.
//!
//! `callbackFunctionHook`'s exact ABI on ARM32/AAPCS is r0 = `StarfishMediaAPIs *this`, r1 = int,
//! r2/r3 = aligned int64, stack[0] = `const char *`. libplayerAPIs' fixed libpf callback thunk passes
//! the original object as `this`, then reaches that symbol through its R_ARM_JUMP_SLOT. The
//! executable exports its own definition so it preempts the library's, and the real implementation
//! is resolved with `RTLD_NEXT`. The gate counts an admitted call as in flight across the complete
//! real hook WITHOUT holding a lock during that call (a nested firmware callback must not deadlock).
//! Once a slot is retired, new calls drop before dereferencing the object, and teardown waits,
//! without a timeout, for in-flight calls to reach zero.
//!
//! Destroying (D1) is permitted only with three proofs: the gate retired, at least one intercepted
//! callback (the interposer was really crossed for this object), and the synchronous
//! UnloadCompleted (type 23) for this epoch. Anything less quarantines instead: the constructed
//! object is kept forever and every later Load is refused.
//!
//! Lock failures fail closed. The C checked every `pthread_mutex_lock`; here the only failure a
//! `std::sync::Mutex` has is poisoning, and it blocks the lifecycle the same way.
//!
//! Generic over [`Native`], the handful of LG calls the lifecycle makes, so the host tests drive
//! this exact code. The television's implementation and the exported verbs are in `tv`.

use std::alloc::{alloc_zeroed, Layout};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_uint, c_void};
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering::*};
use std::sync::{Condvar, Mutex, OnceLock};

use crate::elog;
use crate::text::Ptr;

/// Bytes reserved for one StarfishMediaAPIs object (its real size is not published).
pub(crate) const SMP_STORAGE_SIZE: usize = 65536;
/// The callback type the firmware emits once Unload has completed, synchronously.
pub(crate) const UNLOAD_COMPLETED: c_int = 23;
/// A destroyed slot is handed to a new Load only once this many further Loads came after its
/// destroy, so the slots in use cycle among `REUSE_AFTER_LOADS + 1`.
pub(crate) const REUSE_AFTER_LOADS: u32 = 2;

/// The LG entry points the lifecycle calls. `tv` implements it over libplayerAPIs.
pub(crate) trait Native: Sync {
    /// Resolve the real `callbackFunctionHook` and the Load-with-context overload. True only when
    /// both are proven to be libplayerAPIs'. Called at most once.
    fn resolve(&self) -> bool;
    /// `StarfishMediaAPIs::StarfishMediaAPIs(const char *uid)` with uid NULL.
    ///
    /// # Safety
    /// `object` is a fresh zeroed slot of [`SMP_STORAGE_SIZE`] bytes.
    unsafe fn construct(&self, object: *mut c_void);
    /// # Safety
    /// `object` was constructed by [`Native::construct`] and not destroyed.
    unsafe fn notify_foreground(&self, object: *mut c_void);
    /// The real, synchronous Load, with `epoch` as the callback context.
    ///
    /// # Safety
    /// `object` is constructed; `payload` is NUL-terminated; only called after `resolve` was true.
    unsafe fn load(&self, object: *mut c_void, payload: *const c_char, epoch: c_uint) -> c_int;
    /// The real `callbackFunctionHook`.
    ///
    /// # Safety
    /// The arguments are the firmware's own, passed through unchanged; only after `resolve` was true.
    unsafe fn forward(&self, object: *mut c_void, ty: c_int, num: i64, s: *const c_char);
    /// `StarfishMediaAPIs::~StarfishMediaAPIs()` (D1). The storage itself is never freed.
    ///
    /// # Safety
    /// `object` is constructed, unloaded, and no callback can reach it any more.
    unsafe fn destruct(&self, object: *mut c_void);
}

/// One StarfishMediaAPIs object's bytes. 16-byte alignment, as the C's `posix_memalign(16)`.
#[repr(C, align(16))]
struct Storage(UnsafeCell<[MaybeUninit<u8>; SMP_STORAGE_SIZE]>);

const _: () = assert!(size_of::<Storage>() == SMP_STORAGE_SIZE && align_of::<Storage>() >= 16);

// SAFETY: Rust never forms a reference into the bytes. Only raw pointers to them leave this module,
// handed to LG's C++ code, which synchronizes its own access to its own object.
unsafe impl Sync for Storage {}

/// The gate state, protected by [`Slot::gate`].
struct Gate {
    /// Hook calls admitted and not yet returned.
    inflight: u32,
    /// Callbacks are admitted (cleared by retirement, never set again).
    active: bool,
    /// True only once this Load is entering: callbacks before it are forwarded but not evidence.
    evidence_armed: bool,
    /// Retirement completed with nothing in flight.
    gate_retired: bool,
}

/// One Load's object and its callback gate. Leaked: never freed; reused only after a proven
/// destroy and the cool-down ([`REUSE_AFTER_LOADS`]).
pub(crate) struct Slot {
    /// The Load it serves; a reuse sets the next one before the slot is published.
    epoch: AtomicU32,
    storage: &'static Storage,
    gate: Mutex<Gate>,
    drained: Condvar,
    intercepted: AtomicU32,
    dropped: AtomicU32,
    unload_completed: AtomicBool,
}

impl Slot {
    /// A new zeroed slot for `epoch`, or `None` when the 64 KB allocation fails.
    fn allocate(epoch: c_uint) -> Option<&'static Slot> {
        let layout = Layout::new::<Storage>();
        // SAFETY: the layout has a non-zero size. Zeroed bytes are a valid `Storage` (an array of
        // `MaybeUninit<u8>` accepts any bits). The allocation is never freed, which is the point.
        let storage = unsafe { alloc_zeroed(layout).cast::<Storage>().as_ref()? };
        Some(Box::leak(Box::new(Slot {
            epoch: AtomicU32::new(epoch),
            storage,
            gate: Mutex::new(Gate {
                inflight: 0,
                active: true,
                evidence_armed: false,
                gate_retired: false,
            }),
            drained: Condvar::new(),
            intercepted: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
            unload_completed: AtomicBool::new(false),
        })))
    }

    /// The object address the firmware knows this slot by.
    pub(crate) fn object(&self) -> *mut c_void {
        self.storage.0.get().cast()
    }

    fn epoch(&self) -> c_uint {
        self.epoch.load(Acquire)
    }

    /// Make a destroyed slot new for `epoch`: zeroed bytes, an open gate, no evidence. False (and
    /// nothing changed) when its gate lock is poisoned or the gate is not retired and drained.
    ///
    /// # Safety
    /// The slot's object was destroyed with every proof and the slot is not published.
    unsafe fn renew(&self, epoch: c_uint) -> bool {
        let Ok(mut gate) = self.gate.lock() else {
            return false;
        };
        if gate.active || !gate.gate_retired || gate.inflight != 0 {
            return false;
        }
        // SAFETY: the object was destroyed and its gate admits nothing, so no call can reach
        // these bytes; the interposer takes this gate's lock before forwarding, and it is held
        // here. Rust never forms a reference into the storage. Zero is a valid `Storage`.
        unsafe { ptr::write_bytes(self.object().cast::<u8>(), 0, SMP_STORAGE_SIZE) };
        self.epoch.store(epoch, Release);
        self.intercepted.store(0, Relaxed);
        self.dropped.store(0, Relaxed);
        self.unload_completed.store(false, Release);
        *gate = Gate {
            inflight: 0,
            active: true,
            evidence_armed: false,
            gate_retired: false,
        };
        true
    }
}

/// The seam's process-wide lifecycle state (the C's file-scope statics).
pub(crate) struct Lifecycle<N> {
    native: N,
    /// Every slot ever published, for the interposer's lookup by address.
    slots: Mutex<Vec<&'static Slot>>,
    /// The current Load's slot: null or a leaked `&'static Slot`.
    current: AtomicPtr<Slot>,
    /// Loads accepted so far: the clock the reuse cool-down counts in (wrapping).
    loads: AtomicU32,
    /// Slots destroyed with every proof, oldest first, with [`Self::loads`] at their destroy.
    /// The only slots ever reused.
    reusable: Mutex<VecDeque<(&'static Slot, u32)>>,
    /// `pthread_once` + `g_hook_valid`: whether the hook resolution proved itself.
    hook: OnceLock<bool>,
    /// Sticky: once set, no Load is ever accepted again and no object is destroyed.
    blocked: AtomicBool,
    /// "An object exists". Written on the load thread right after the constructor, read on the
    /// main thread every frame: release/acquire pairs it with the constructor and `current`.
    ready: AtomicBool,
    /// Issue #74: `ready` goes true BEFORE the real, synchronous Load returns. On a chassis where
    /// that call blocks for a long time (k5lp video-output init), the main thread was free to
    /// Feed/Play into an object whose LoadCommon path was still running on the load thread: a null-
    /// argument SIGSEGV on one path, a Play-vs-LoadCommon mutex deadlock ending in an OMX_VideoComp
    /// abort on the other. This closes that window without changing what `sf_ready` answers:
    /// cleared the instant the real Load is about to be made, set once it returns.
    load_returned: AtomicBool,
}

impl<N: Native> Lifecycle<N> {
    pub(crate) const fn new(native: N) -> Self {
        Self {
            native,
            slots: Mutex::new(Vec::new()),
            current: AtomicPtr::new(ptr::null_mut()),
            loads: AtomicU32::new(0),
            reusable: Mutex::new(VecDeque::new()),
            hook: OnceLock::new(),
            blocked: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            load_returned: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn native(&self) -> &N {
        &self.native
    }

    fn block(&self) {
        self.blocked.store(true, Release);
    }

    fn is_blocked(&self) -> bool {
        self.blocked.load(Acquire)
    }

    /// Resolve the hook once; a failed resolution blocks the lifecycle for good.
    fn prepare_hook(&self) -> bool {
        *self.hook.get_or_init(|| {
            let valid = self.native.resolve();
            if !valid {
                self.block();
            }
            valid
        })
    }

    fn hook_valid(&self) -> bool {
        self.hook.get() == Some(&true)
    }

    fn current(&self) -> Option<&'static Slot> {
        // SAFETY: `current` only ever holds null or a pointer made from a leaked `&'static Slot`.
        unsafe { self.current.load(Acquire).as_ref() }
    }

    fn set_dispatch(&self, ready: bool, load_returned: bool) {
        self.ready.store(ready, Release);
        self.load_returned.store(load_returned, Release);
    }

    /// `sf_ready`: 1 while the current object is dispatchable; 0 can retain a quarantined object.
    pub(crate) fn ready(&self) -> bool {
        self.ready.load(Acquire)
    }

    /// The object every verb but Load dispatches through: only once it exists AND its Load returned.
    pub(crate) fn ready_object(&self) -> Option<*mut c_void> {
        let slot = self.current()?;
        (self.ready() && self.load_returned.load(Acquire)).then(|| slot.object())
    }

    /// `sf_load`, on the load thread. Returns Load's own result, 0 when refused.
    ///
    /// # Safety
    /// `payload` is NULL or NUL-terminated.
    pub(crate) unsafe fn load(&self, payload: *const c_char, epoch: c_uint) -> c_int {
        if payload.is_null()
            || epoch == 0
            || self.is_blocked()
            || self.ready()
            || self.current().is_some()
            || !self.prepare_hook()
        {
            elog!(
                "native lifecycle: Load refused blocked={} ready={} current={} epoch={epoch}",
                self.is_blocked() as c_int,
                self.ready() as c_int,
                Ptr::of(self.current.load(Acquire)),
            );
            return 0;
        }

        let n = self.loads.load(Relaxed).wrapping_add(1);
        let slot = match self.take_reusable(n, epoch) {
            Some(slot) => slot,
            None => {
                let Some(slot) = Slot::allocate(epoch) else {
                    self.block();
                    elog!("native lifecycle: Load refused (callback gate allocation/init failed)");
                    return 0;
                };
                // An unpublished candidate is not recycled: it never had a proven destroy.
                match self.slots.lock() {
                    Ok(mut slots) => slots.push(slot),
                    Err(_) => {
                        self.block();
                        return 0;
                    }
                }
                slot
            }
        };
        self.loads.store(n, Relaxed);
        self.current.store(ptr::from_ref(slot).cast_mut(), Release);

        let object = slot.object();
        // SAFETY: `object` is the fresh zeroed 64 KB slot just published.
        unsafe { self.native.construct(object) };
        // Close the dispatch window BEFORE opening it: `load_returned` first, then `ready`, so the
        // ordering is a local invariant rather than one every future `ready = true` site must keep.
        self.load_returned.store(false, Release);
        self.ready.store(true, Release); // release: the constructor's writes precede the flag
                                         // SAFETY: constructed just above.
        unsafe { self.native.notify_foreground(object) };
        match slot.gate.lock() {
            Ok(mut gate) => gate.evidence_armed = true,
            Err(_) => {
                self.block();
                self.set_dispatch(false, false); // the object stays quarantined; never D1/reuse
                return 0;
            }
        }
        // THE actual Load: synchronous on this thread, and on some chassis it blocks for a long
        // time inside video-output init (issue #74). `load_returned` stays false for all of it.
        // SAFETY: constructed object, payload checked non-NULL (NUL-terminated per this fn's
        // contract), and `prepare_hook` proved the Load-with-context overload above.
        let ok = unsafe { self.native.load(object, payload, epoch) };
        self.load_returned.store(true, Release);
        ok
    }

    /// The oldest destroyed slot whose cool-down has passed by Load number `n`, renewed for
    /// `epoch`; `None` to allocate a fresh one (none due, or a lock or gate that is not sound).
    fn take_reusable(&self, n: u32, epoch: c_uint) -> Option<&'static Slot> {
        let mut reusable = self.reusable.lock().ok()?;
        let &(slot, destroyed_at) = reusable.front()?;
        if n.wrapping_sub(destroyed_at) <= REUSE_AFTER_LOADS {
            return None;
        }
        reusable.pop_front();
        // SAFETY: only `destroy` queues a slot, after D1 with every proof; it is not current.
        if !unsafe { slot.renew(epoch) } {
            return None;
        }
        elog!(
            "native lifecycle: reusing object={} for epoch={epoch} ({} Loads after its destroy)",
            Ptr::of(slot.object()),
            n.wrapping_sub(destroyed_at) - 1
        );
        Some(slot)
    }

    /// The Load callback's side of the gate (`sf_cb`): UnloadCompleted for the current epoch is the
    /// proof `destroy` needs. Synthetic UnloadCompleted bypasses `callbackFunctionHook`, so this
    /// marker is independent of the interception counter.
    pub(crate) fn observe_event(&self, epoch: c_uint, ty: c_int) {
        if ty != UNLOAD_COMPLETED {
            return;
        }
        if let Some(slot) = self.current().filter(|s| s.epoch() == epoch) {
            slot.unload_completed.store(true, Release);
        }
    }

    /// `Err` means the registry lock itself failed: fail closed rather than forward a possibly owned
    /// object without its gate. `Ok(None)` is an object of some other in-process client, which keeps
    /// libplayerAPIs' ordinary behaviour.
    fn find_slot(&self, object: *mut c_void) -> Result<Option<&'static Slot>, ()> {
        let slots = self.slots.lock().map_err(drop)?;
        Ok(slots.iter().copied().find(|s| s.object() == object))
    }

    /// The `callbackFunctionHook` interposer.
    ///
    /// # Safety
    /// The arguments are the firmware's, passed through unchanged.
    pub(crate) unsafe fn intercept(
        &self,
        object: *mut c_void,
        ty: c_int,
        num: i64,
        s: *const c_char,
    ) {
        if !self.prepare_hook() {
            return;
        }
        let slot = match self.find_slot(object) {
            Err(()) => return self.block(),
            Ok(None) => {
                // SAFETY: not ours; the firmware's own call, forwarded to the hook it would have run.
                unsafe { self.native.forward(object, ty, num, s) };
                return;
            }
            Ok(Some(slot)) => slot,
        };

        match slot.gate.lock() {
            Err(_) => return self.block(),
            Ok(mut gate) if gate.active => {
                if gate.evidence_armed {
                    slot.intercepted.fetch_add(1, Relaxed);
                }
                gate.inflight += 1;
            }
            Ok(_) => {
                slot.dropped.fetch_add(1, Relaxed);
                return;
            }
        }

        // SAFETY: admitted while the gate was active, and counted in flight, so retirement waits
        // for this call and the object cannot be destroyed under it. No lock is held across it.
        unsafe { self.native.forward(object, ty, num, s) };

        let Ok(mut gate) = slot.gate.lock() else {
            return self.block();
        };
        if gate.inflight == 0 {
            self.block();
        } else {
            gate.inflight -= 1;
            if gate.inflight == 0 {
                slot.drained.notify_all();
            }
        }
    }

    /// `sf_callback_gate_retire`, after Unload: close admission and wait, without a timeout, for
    /// every admitted hook call. True proves this object's interposer was actually crossed.
    pub(crate) fn gate_retire(&self) -> bool {
        let Some(slot) = self.current() else {
            self.block();
            return false;
        };
        let Ok(mut gate) = slot.gate.lock() else {
            self.block();
            return false;
        };
        gate.active = false;
        let mut drained = true;
        while gate.inflight != 0 {
            match slot.drained.wait(gate) {
                Ok(g) => gate = g,
                Err(poisoned) => {
                    self.block();
                    drained = false;
                    gate = poisoned.into_inner();
                    break;
                }
            }
        }
        gate.gate_retired = drained && gate.inflight == 0;
        let intercepted = slot.intercepted.load(Relaxed);
        let dropped = slot.dropped.load(Relaxed);
        let proven =
            gate.gate_retired && self.hook_valid() && intercepted != 0 && !self.is_blocked();
        drop(gate);
        elog!(
            "native callback gate: epoch={} intercepted={intercepted} dropped={dropped} proven={}",
            slot.epoch(),
            proven as c_int
        );
        proven
    }

    /// `sf_callback_intercepts`: the runtime evidence counter of the current object.
    pub(crate) fn intercepts(&self) -> u32 {
        self.current().map_or(0, |s| s.intercepted.load(Relaxed))
    }

    /// `sf_quarantine`: keep the current constructed object forever and refuse every later Load.
    pub(crate) fn quarantine(&self) {
        let slot = self.current();
        if slot.is_some() {
            let _ = self.gate_retire();
        }
        self.block();
        self.set_dispatch(false, false);
        let object = slot.map_or(ptr::null_mut(), Slot::object);
        elog!(
            "native lifecycle: QUARANTINED epoch={} object={}; D1/reuse and future Load disabled",
            slot.map_or(0, Slot::epoch),
            Ptr::of(object),
        );
    }

    /// `sf_destroy`: D1 the current object when every proof holds, else quarantine. True when the
    /// object was destroyed and the next Load may run.
    ///
    /// # Safety
    /// Not concurrent with any other verb on the current object (the main-thread contract).
    pub(crate) unsafe fn destroy(&self) -> bool {
        let Some(slot) = self.current() else {
            return false;
        };
        if !self.ready() {
            return false;
        }
        // Read under the gate, then act with it released: quarantine retakes it.
        let safe = match slot.gate.lock() {
            Ok(gate) => {
                !gate.active
                    && gate.gate_retired
                    && slot.intercepted.load(Relaxed) != 0
                    && slot.unload_completed.load(Acquire)
                    && self.hook_valid()
                    && !self.is_blocked()
            }
            Err(_) => false,
        };
        if !safe {
            self.quarantine();
            return false;
        }
        // SAFETY: the gate is retired with nothing in flight and admits nothing more, the
        // interposer was proven crossed for this object, and its UnloadCompleted arrived.
        unsafe { self.native.destruct(slot.object()) };
        self.set_dispatch(false, false);
        self.current.store(ptr::null_mut(), Release);
        // Proven destroyed: the slot may serve a later Load once its cool-down has passed. A lock
        // failure only forgoes the reuse.
        if let Ok(mut reusable) = self.reusable.lock() {
            reusable.push_back((slot, self.loads.load(Relaxed)));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Construct(usize),
        Foreground(usize),
        Load(usize, c_uint),
        Forward(usize, c_int),
        Destruct(usize),
    }

    /// A forwarded hook type to hold in flight, the "entered" signal, and the release.
    type Park = (c_int, mpsc::Sender<()>, mpsc::Receiver<()>);

    /// A recording stand-in for libplayerAPIs.
    #[derive(Default)]
    struct Fake {
        resolve_fails: bool,
        load_result: c_int,
        calls: Mutex<Vec<Call>>,
        /// Set once the test leaked its lifecycle, so the fake can call back into it.
        lc: OnceLock<&'static Lifecycle<Fake>>,
        /// A hook callback the firmware emits from inside notifyForeground.
        hook_during_foreground: AtomicBool,
        /// A hook callback the firmware emits from inside the real Load, on the load thread.
        hook_during_load: AtomicBool,
        /// What `ready()`/`ready_object()` answered while the real Load was running.
        seen_during_load: Mutex<Option<(bool, bool)>>,
        /// Re-enter the interposer from inside the first forwarded call.
        reenter_once: AtomicBool,
        /// Block forwarded calls of this type until released.
        park_type: Mutex<Option<Park>>,
        resolves: AtomicUsize,
        /// Constructions that found the storage not all zero.
        dirty_constructs: AtomicUsize,
    }

    impl Fake {
        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn lc(&self) -> &'static Lifecycle<Fake> {
            self.lc.get().unwrap()
        }
    }

    impl Native for Fake {
        fn resolve(&self) -> bool {
            self.resolves.fetch_add(1, SeqCst);
            !self.resolve_fails
        }
        unsafe fn construct(&self, object: *mut c_void) {
            self.record(Call::Construct(object as usize));
            // Like the real constructor, write state into the storage, so a reuse that did not
            // zero it shows here.
            // SAFETY: `object` is a whole 64 KB slot no Rust reference points into.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(object.cast::<u8>(), SMP_STORAGE_SIZE) };
            if bytes.iter().any(|&b| b != 0) {
                self.dirty_constructs.fetch_add(1, SeqCst);
            }
            bytes[..64].fill(0xa5);
            bytes[SMP_STORAGE_SIZE - 1] = 0x5a;
        }
        unsafe fn notify_foreground(&self, object: *mut c_void) {
            self.record(Call::Foreground(object as usize));
            if self.hook_during_foreground.load(SeqCst) {
                // SAFETY: test stand-in; the pointers are never dereferenced.
                unsafe { self.lc().intercept(object, 5, 0, ptr::null()) };
            }
        }
        unsafe fn load(
            &self,
            object: *mut c_void,
            _payload: *const c_char,
            epoch: c_uint,
        ) -> c_int {
            self.record(Call::Load(object as usize, epoch));
            let lc = self.lc();
            *self.seen_during_load.lock().unwrap() =
                Some((lc.ready(), lc.ready_object().is_some()));
            if self.hook_during_load.load(SeqCst) {
                // SAFETY: test stand-in; the pointers are never dereferenced.
                unsafe { lc.intercept(object, 1, 0, ptr::null()) };
            }
            self.load_result
        }
        unsafe fn forward(&self, object: *mut c_void, ty: c_int, _num: i64, _s: *const c_char) {
            self.record(Call::Forward(object as usize, ty));
            if self.reenter_once.swap(false, SeqCst) {
                // SAFETY: test stand-in; a nested firmware callback for the same object.
                unsafe { self.lc().intercept(object, ty + 1, 0, ptr::null()) };
            }
            let park = self.park_type.lock().unwrap().take();
            if let Some((parked, entered, release)) = park {
                if parked == ty {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                } else {
                    *self.park_type.lock().unwrap() = Some((parked, entered, release));
                }
            }
        }
        unsafe fn destruct(&self, object: *mut c_void) {
            self.record(Call::Destruct(object as usize));
        }
    }

    fn seam(fake: Fake) -> &'static Lifecycle<Fake> {
        let lc: &'static Lifecycle<Fake> = Box::leak(Box::new(Lifecycle::new(fake)));
        lc.native().lc.set(lc).ok().unwrap();
        lc
    }

    fn fake() -> Fake {
        Fake {
            load_result: 1,
            ..Fake::default()
        }
    }

    const PAYLOAD: *const c_char = c"{\"args\":[]}".as_ptr();

    fn load(lc: &Lifecycle<Fake>, epoch: c_uint) -> c_int {
        // SAFETY: a static NUL-terminated payload.
        unsafe { lc.load(PAYLOAD, epoch) }
    }

    fn hook(lc: &Lifecycle<Fake>, object: *mut c_void, ty: c_int) {
        // SAFETY: test stand-in; the pointers are never dereferenced.
        unsafe { lc.intercept(object, ty, 0, ptr::null()) }
    }

    fn destroy(lc: &Lifecycle<Fake>) -> bool {
        // SAFETY: single-threaded test.
        unsafe { lc.destroy() }
    }

    /// Load, one intercepted callback, Unload's type 23, retire: every proof `destroy` needs.
    fn play_and_retire(lc: &Lifecycle<Fake>, epoch: c_uint) -> *mut c_void {
        assert_eq!(load(lc, epoch), 1);
        let object = lc.ready_object().unwrap();
        hook(lc, object, 0);
        lc.observe_event(epoch, UNLOAD_COMPLETED);
        assert!(lc.gate_retire());
        object
    }

    #[test]
    fn verbs_wait_for_load_to_return_not_just_for_the_object() {
        let lc = seam(fake());
        assert!(lc.ready_object().is_none());
        assert_eq!(load(lc, 7), 1);
        // During the real Load the object existed (sf_ready) but was not dispatchable (issue #74).
        assert_eq!(
            *lc.native().seen_during_load.lock().unwrap(),
            Some((true, false))
        );
        let object = lc.ready_object().expect("dispatchable once Load returned") as usize;
        assert_eq!(
            lc.native().calls(),
            [
                Call::Construct(object),
                Call::Foreground(object),
                Call::Load(object, 7)
            ]
        );
    }

    #[test]
    fn load_returns_the_library_result() {
        let lc = seam(Fake {
            load_result: 0,
            ..Fake::default()
        });
        assert_eq!(load(lc, 1), 0);
        assert!(
            lc.ready_object().is_some(),
            "a refused Load still constructed an object"
        );
    }

    #[test]
    fn object_storage_is_64k_and_16_aligned() {
        assert_eq!(std::mem::size_of::<Storage>(), SMP_STORAGE_SIZE);
        assert_eq!(std::mem::align_of::<Storage>(), 16);
        let lc = seam(fake());
        load(lc, 1);
        assert_eq!(lc.ready_object().unwrap() as usize % 16, 0);
    }

    #[test]
    fn invalid_arguments_are_refused_before_anything_is_built() {
        let lc = seam(fake());
        // SAFETY: NULL is allowed and refused.
        assert_eq!(unsafe { lc.load(ptr::null(), 1) }, 0);
        assert_eq!(load(lc, 0), 0);
        assert!(lc.native().calls().is_empty());
        assert_eq!(
            lc.native().resolves.load(SeqCst),
            0,
            "refused before resolving the hook"
        );
        assert_eq!(
            load(lc, 1),
            1,
            "argument refusals do not block the lifecycle"
        );
    }

    #[test]
    fn an_unproven_hook_blocks_every_load() {
        let lc = seam(Fake {
            resolve_fails: true,
            ..fake()
        });
        assert_eq!(load(lc, 1), 0);
        assert_eq!(load(lc, 2), 0);
        assert!(lc.native().calls().is_empty());
        assert_eq!(lc.native().resolves.load(SeqCst), 1, "resolved once");
        assert!(lc.is_blocked());
    }

    #[test]
    fn a_second_load_is_refused_while_an_object_is_current() {
        let lc = seam(fake());
        assert_eq!(load(lc, 1), 1);
        assert_eq!(load(lc, 2), 0);
        assert_eq!(
            lc.native()
                .calls()
                .iter()
                .filter(|c| matches!(c, Call::Construct(_)))
                .count(),
            1
        );
        assert!(!lc.is_blocked(), "a busy refusal is not a quarantine");
    }

    #[test]
    fn evidence_counts_from_load_entry_and_late_calls_drop_after_retirement() {
        let f = Fake {
            hook_during_foreground: AtomicBool::new(true),
            ..fake()
        };
        let lc = seam(f);
        assert_eq!(load(lc, 3), 1);
        let object = lc.ready_object().unwrap();
        // The notifyForeground callback was forwarded but is not evidence.
        assert_eq!(lc.intercepts(), 0);
        hook(lc, object, 0);
        hook(lc, object, 2);
        assert_eq!(lc.intercepts(), 2);
        lc.observe_event(3, UNLOAD_COMPLETED);
        assert!(lc.gate_retire());
        hook(lc, object, 0);
        let forwards = lc
            .native()
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Forward(..)))
            .count();
        assert_eq!(
            forwards, 3,
            "the post-retirement call was dropped, not forwarded"
        );
        assert_eq!(lc.current().unwrap().dropped.load(SeqCst), 1);
    }

    /// Evidence is armed BEFORE the real Load, and no gate lock is held across it: a callback the
    /// firmware fires from inside Load is admitted, forwarded and counted (a held gate would hang
    /// this test).
    #[test]
    fn a_callback_from_inside_load_is_evidence_and_does_not_block() {
        let lc = seam(Fake {
            hook_during_load: AtomicBool::new(true),
            ..fake()
        });
        assert_eq!(load(lc, 6), 1);
        let object = lc.ready_object().unwrap() as usize;
        assert_eq!(lc.intercepts(), 1);
        assert!(lc.native().calls().contains(&Call::Forward(object, 1)));
        lc.observe_event(6, UNLOAD_COMPLETED);
        assert!(
            lc.gate_retire(),
            "a Load-time callback alone proves the interposer"
        );
        assert!(destroy(lc));
    }

    #[test]
    fn a_foreign_object_passes_straight_through() {
        let lc = seam(fake());
        load(lc, 1);
        let foreign = 0x1000 as *mut c_void;
        hook(lc, foreign, 0);
        assert!(lc.native().calls().contains(&Call::Forward(0x1000, 0)));
        assert_eq!(lc.intercepts(), 0);
    }

    #[test]
    fn retirement_without_an_intercepted_call_is_not_proof() {
        let lc = seam(fake());
        load(lc, 1);
        lc.observe_event(1, UNLOAD_COMPLETED);
        assert!(!lc.gate_retire());
    }

    #[test]
    fn retirement_waits_for_an_in_flight_callback() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let f = fake();
        *f.park_type.lock().unwrap() = Some((9, entered_tx, release_rx));
        let lc = seam(f);
        load(lc, 1);
        let object = lc.ready_object().unwrap() as usize;
        hook(lc, object as *mut c_void, 0);

        let caller = thread::spawn(move || hook(lc, object as *mut c_void, 9));
        entered_rx.recv().unwrap();
        let retired = Arc::new(AtomicBool::new(false));
        let flag = retired.clone();
        let retirer = thread::spawn(move || {
            let proven = lc.gate_retire();
            flag.store(true, SeqCst);
            proven
        });
        thread::sleep(Duration::from_millis(100));
        assert!(
            !retired.load(SeqCst),
            "retired with a hook call still in flight"
        );
        release_tx.send(()).unwrap();
        caller.join().unwrap();
        assert!(retirer.join().unwrap());
        assert!(retired.load(SeqCst));
    }

    #[test]
    fn a_nested_callback_inside_the_real_hook_does_not_deadlock() {
        let f = Fake {
            reenter_once: AtomicBool::new(true),
            ..fake()
        };
        let lc = seam(f);
        load(lc, 1);
        let object = lc.ready_object().unwrap();
        hook(lc, object, 0);
        assert_eq!(lc.intercepts(), 2);
        assert!(lc.gate_retire());
    }

    #[test]
    fn unload_completed_counts_only_for_the_current_epoch() {
        let lc = seam(fake());
        load(lc, 4);
        let object = lc.ready_object().unwrap();
        hook(lc, object, 0);
        lc.observe_event(3, UNLOAD_COMPLETED);
        lc.observe_event(4, 0);
        assert!(lc.gate_retire());
        assert!(!destroy(lc), "no UnloadCompleted for epoch 4");
        assert!(lc.is_blocked());
    }

    #[test]
    fn destroy_with_every_proof_frees_the_lifecycle_for_the_next_load() {
        let lc = seam(fake());
        let object = play_and_retire(lc, 1) as usize;
        assert!(destroy(lc));
        assert_eq!(lc.native().calls().last(), Some(&Call::Destruct(object)));
        assert!(lc.current().is_none() && !lc.ready() && lc.ready_object().is_none());
        assert_eq!(load(lc, 2), 1);
    }

    #[test]
    fn destroy_without_retirement_quarantines() {
        let lc = seam(fake());
        load(lc, 1);
        let object = lc.ready_object().unwrap();
        hook(lc, object, 0);
        lc.observe_event(1, UNLOAD_COMPLETED);
        assert!(!destroy(lc));
        assert_quarantined(lc);
    }

    #[test]
    fn destroy_without_intercepted_evidence_quarantines() {
        let lc = seam(fake());
        load(lc, 1);
        lc.observe_event(1, UNLOAD_COMPLETED);
        let _ = lc.gate_retire();
        assert!(!destroy(lc));
        assert_quarantined(lc);
    }

    #[test]
    fn destroy_before_any_load_is_a_no_op() {
        let lc = seam(fake());
        assert!(!destroy(lc));
        assert!(!lc.is_blocked());
        assert_eq!(load(lc, 1), 1);
    }

    fn assert_quarantined(lc: &Lifecycle<Fake>) {
        assert!(
            !lc.native()
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Destruct(_))),
            "never D1"
        );
        assert!(lc.is_blocked() && !lc.ready() && lc.ready_object().is_none());
        assert!(lc.current().is_some(), "the quarantined object is kept");
        assert!(!destroy(lc));
        assert_eq!(load(lc, 99), 0, "no Load after quarantine");
    }

    #[test]
    fn quarantine_retires_the_gate_and_keeps_the_object() {
        let lc = seam(fake());
        load(lc, 1);
        let object = lc.ready_object().unwrap();
        lc.quarantine();
        hook(lc, object, 0);
        assert_eq!(
            lc.current().unwrap().dropped.load(SeqCst),
            1,
            "callbacks drop after quarantine"
        );
        assert_quarantined(lc);
    }

    #[test]
    fn quarantine_with_nothing_loaded_still_blocks() {
        let lc = seam(fake());
        lc.quarantine();
        assert_eq!(load(lc, 1), 0);
    }

    /// Zapping cycles a few slots instead of leaking one per Load: a slot destroyed with every
    /// proof serves again only after REUSE_AFTER_LOADS further Loads, zeroed, as a new session.
    #[test]
    fn a_destroyed_slot_is_reused_only_after_its_cool_down_and_zeroed() {
        let lc = seam(fake());
        let mut addresses = Vec::new();
        for epoch in 1..=6 {
            addresses.push(play_and_retire(lc, epoch) as usize);
            assert!(destroy(lc));
        }
        let ring = REUSE_AFTER_LOADS as usize + 1;
        for i in 0..ring {
            for j in 0..i {
                assert_ne!(addresses[i], addresses[j], "reused inside the cool-down");
            }
        }
        assert_eq!(
            &addresses[ring..],
            &addresses[..ring],
            "then reused oldest first"
        );
        assert_eq!(lc.slots.lock().unwrap().len(), ring);
        assert_eq!(
            lc.native().dirty_constructs.load(SeqCst),
            0,
            "every reused slot was zeroed"
        );
        // The reused slot is a new session: its own epoch, no evidence carried over.
        assert_eq!(load(lc, 7), 1);
        assert_eq!(lc.intercepts(), 0);
        lc.observe_event(6, UNLOAD_COMPLETED);
        let object = lc.ready_object().unwrap();
        hook(lc, object, 0);
        assert!(lc.gate_retire());
        assert!(
            !destroy(lc),
            "the old epoch's UnloadCompleted is not this one's"
        );
    }

    /// A thousand channel changes keep the registry at its ring of slots.
    #[test]
    fn a_thousand_loads_keep_the_registry_bounded() {
        let lc = seam(fake());
        for epoch in 1..=1000 {
            play_and_retire(lc, epoch);
            assert!(destroy(lc));
        }
        assert_eq!(
            lc.slots.lock().unwrap().len(),
            REUSE_AFTER_LOADS as usize + 1
        );
        assert_eq!(lc.native().dirty_constructs.load(SeqCst), 0);
        assert!(!lc.is_blocked());
    }

    /// A late thunk on a destroyed object, its slot waiting out the cool-down, never reaches the
    /// firmware.
    #[test]
    fn a_late_hook_to_a_freed_slot_is_dropped() {
        let lc = seam(fake());
        let freed = play_and_retire(lc, 1);
        assert!(destroy(lc));
        let before = lc.native().calls().len();
        hook(lc, freed, 0);
        assert_eq!(lc.native().calls().len(), before, "dropped, not forwarded");
        let slot = lc.slots.lock().unwrap()[0];
        assert_eq!(slot.dropped.load(SeqCst), 1);
        // Two more sessions on fresh slots: the late thunk is still dropped.
        for epoch in 2..=3 {
            assert_ne!(play_and_retire(lc, epoch), freed);
            assert!(destroy(lc));
            hook(lc, freed, 0);
        }
        assert_eq!(slot.dropped.load(SeqCst), 3);
    }

    /// A quarantined object is kept, never queued for reuse, and blocks every later Load; only
    /// proven destroys are queued.
    #[test]
    fn a_quarantined_slot_is_never_reused() {
        let lc = seam(fake());
        let proven = play_and_retire(lc, 1);
        assert!(destroy(lc));
        assert_eq!(load(lc, 2), 1);
        let quarantined = lc.ready_object().unwrap();
        lc.observe_event(2, UNLOAD_COMPLETED);
        let _ = lc.gate_retire();
        assert!(!destroy(lc), "no intercepted callback: quarantined");
        let queued: Vec<usize> = lc
            .reusable
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.object() as usize)
            .collect();
        assert_eq!(
            queued,
            [proven as usize],
            "only the proven destroy is queued"
        );
        assert!(!queued.contains(&(quarantined as usize)));
        assert!(
            !lc.native()
                .calls()
                .contains(&Call::Destruct(quarantined as usize)),
            "never D1"
        );
        assert!(
            lc.is_blocked() && lc.current().is_some_and(|s| s.object() == quarantined),
            "kept"
        );
        assert_eq!(
            load(lc, 3),
            0,
            "no Load after quarantine, so the queued slot is never used either"
        );
    }
}
