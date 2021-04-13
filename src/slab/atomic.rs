use core::{
    cell::UnsafeCell,
    fmt, intrinsics,
    sync::atomic::Ordering::{self, *},
};

const UPPER_BITS: usize = 0xffff_0000_0000_0000;
const UPPER_SHIFT: usize = 48;

#[repr(C, align(8))]
pub struct AtomicPtr<T> {
    p: UnsafeCell<*mut T>,
}

unsafe impl<T> Send for AtomicPtr<T> {}
unsafe impl<T> Sync for AtomicPtr<T> {}

impl<T> Default for AtomicPtr<T> {
    fn default() -> Self {
        Self::new(core::ptr::null_mut())
    }
}

/// Compound pointer with 16-bit tid and the 48-bit real address.
#[derive(PartialEq, Eq)]
pub struct CompoundPtr<T>(*mut T);

impl<T> Clone for CompoundPtr<T> {
    fn clone(&self) -> Self {
        CompoundPtr(self.0)
    }
}

impl<T> Copy for CompoundPtr<T> {}

impl<T> Default for CompoundPtr<T> {
    fn default() -> Self {
        Self::new(core::ptr::null_mut())
    }
}

impl<T> fmt::Debug for CompoundPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.as_ptr(), f)
    }
}

impl<T> CompoundPtr<T> {
    #[inline]
    pub fn new(ptr: *mut T) -> Self {
        CompoundPtr(with_tid(ptr, 0))
    }

    #[inline]
    pub fn with_tid(ptr: *mut T, tid: u16) -> Self {
        CompoundPtr(with_tid(ptr, tid))
    }

    #[inline]
    pub fn new_next(self, ptr: *mut T) -> Self {
        CompoundPtr(with_tid(ptr, self.tid() + 1))
    }

    #[inline]
    pub fn as_ptr(self) -> *mut T {
        erase_tid(self.0)
    }

    #[inline]
    pub unsafe fn as_ref<'a>(self) -> Option<&'a T> {
        self.as_ptr().as_ref()
    }

    #[inline]
    pub unsafe fn as_mut<'a>(self) -> Option<&'a mut T> {
        self.as_ptr().as_mut()
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self.as_ptr().is_null()
    }

    #[inline]
    pub fn tid(self) -> u16 {
        take_tid(self.0)
    }

    #[inline]
    pub fn increment_tid(&mut self) {
        *self = CompoundPtr::with_tid(self.as_ptr(), self.tid() + 1);
    }

    #[inline]
    pub fn inner(self) -> *mut T {
        self.0
    }
}

impl<T> AtomicPtr<T> {
    #[inline]
    pub fn new(ptr: *mut T) -> Self {
        Self {
            p: UnsafeCell::new(with_tid(ptr, 0)),
        }
    }

    /// Loads a value from the pointer.
    ///
    /// `load` takes an [`Ordering`] argument which describes the memory ordering
    /// of this operation. Possible values are [`SeqCst`], [`Acquire`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Release`] or [`AcqRel`].
    #[inline]
    pub fn load(&self, order: Ordering) -> CompoundPtr<T> {
        CompoundPtr(atomic_load(self.p.get(), order))
    }

    /// Stores a value into the pointer.
    ///
    /// `store` takes an [`Ordering`] argument which describes the memory ordering
    /// of this operation. Possible values are [`SeqCst`], [`Release`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Acquire`] or [`AcqRel`].
    #[inline]
    pub fn store(&self, ptr: CompoundPtr<T>, order: Ordering) {
        atomic_store(self.p.get(), ptr.inner(), order);
    }

    /// Stores a value into the pointer, returning the previous value.
    ///
    /// `swap` takes an [`Ordering`] argument which describes the memory ordering
    /// of this operation. All ordering modes are possible. Note that using
    /// [`Acquire`] makes the store part of this operation [`Relaxed`], and
    /// using [`Release`] makes the load part [`Relaxed`].
    ///
    /// **Note:** This method is only available on platforms that support atomic
    /// operations on pointers.
    #[inline]
    pub fn swap(&self, ptr: CompoundPtr<T>, order: Ordering) -> CompoundPtr<T> {
        CompoundPtr(atomic_swap(self.p.get(), ptr.inner(), order))
    }

    /// Stores a value into the pointer if the current value is the same as the `current` value.
    ///
    /// The return value is a result indicating whether the new value was written and containing
    /// the previous value. On success this value is guaranteed to be equal to `current`.
    ///
    /// `compare_exchange` takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using [`Acquire`] as success ordering makes the store part
    /// of this operation [`Relaxed`], and using [`Release`] makes the successful load
    /// [`Relaxed`]. The failure ordering can only be [`SeqCst`], [`Acquire`] or [`Relaxed`]
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// **Note:** This method is only available on platforms that support atomic
    /// operations on pointers.
    pub fn compare_exchange(
        &self,
        current: CompoundPtr<T>,
        new: CompoundPtr<T>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<CompoundPtr<T>, CompoundPtr<T>> {
        match atomic_compare_exchange(self.p.get(), current.inner(), new.inner(), success, failure)
        {
            Ok(ptr) => Ok(CompoundPtr(ptr)),
            Err(ptr) => Err(CompoundPtr(ptr)),
        }
    }

    /// Stores a value into the pointer if the current value is the same as the `current` value.
    ///
    /// Unlike [`AtomicPtr::compare_exchange`], this function is allowed to spuriously fail even when the
    /// comparison succeeds, which can result in more efficient code on some platforms. The
    /// return value is a result indicating whether the new value was written and containing the
    /// previous value.
    ///
    /// `compare_exchange_weak` takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using [`Acquire`] as success ordering makes the store part
    /// of this operation [`Relaxed`], and using [`Release`] makes the successful load
    /// [`Relaxed`]. The failure ordering can only be [`SeqCst`], [`Acquire`] or [`Relaxed`]
    /// and must be equivalent to or weaker than the success ordering.
    ///
    /// **Note:** This method is only available on platforms that support atomic
    /// operations on pointers.
    pub fn compare_exchange_weak(
        &self,
        current: CompoundPtr<T>,
        new: CompoundPtr<T>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<CompoundPtr<T>, CompoundPtr<T>> {
        match atomic_compare_exchange_weak(
            self.p.get(),
            current.inner(),
            new.inner(),
            success,
            failure,
        ) {
            Ok(ptr) => Ok(CompoundPtr(ptr)),
            Err(ptr) => Err(CompoundPtr(ptr)),
        }
    }

    /// Fetches the value, and applies a function to it that returns an optional
    /// new value. Returns a `Result` of `Ok(previous_value)` if the function
    /// returned `Some(_)`, else `Err(previous_value)`.
    ///
    /// Note: This may call the function multiple times if the value has been
    /// changed from other threads in the meantime, as long as the function
    /// returns `Some(_)`, but the function will have been applied only once to
    /// the stored value.
    ///
    /// `fetch_update` takes two [`Ordering`] arguments to describe the memory
    /// ordering of this operation. The first describes the required ordering for
    /// when the operation finally succeeds while the second describes the
    /// required ordering for loads. These correspond to the success and failure
    /// orderings of [`AtomicPtr::compare_exchange`] respectively.
    ///
    /// Using [`Acquire`] as success ordering makes the store part of this
    /// operation [`Relaxed`], and using [`Release`] makes the final successful
    /// load [`Relaxed`]. The (failed) load ordering can only be [`SeqCst`],
    /// [`Acquire`] or [`Relaxed`] and must be equivalent to or weaker than the
    /// success ordering.
    ///
    /// **Note:** This method is only available on platforms that support atomic
    /// operations on pointers.
    #[inline]
    pub fn fetch_update<F>(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        mut f: F,
    ) -> Result<*mut T, *mut T>
    where
        F: FnMut(*mut T) -> Option<*mut T>,
    {
        let mut prev = self.load(fetch_order);
        while let Some(next) = f(prev.as_ptr()) {
            match self.compare_exchange_weak(prev, prev.new_next(next), set_order, fetch_order) {
                Ok(x) => return Ok(x.as_ptr()),
                Err(next_prev) => prev = next_prev,
            }
        }
        Err(prev.as_ptr())
    }
}

#[inline]
fn with_tid<T>(ptr: *mut T, tid: u16) -> *mut T {
    ((ptr as usize & !UPPER_BITS) | ((tid as usize) << UPPER_SHIFT)) as *mut _
}

#[inline]
fn take_tid<T>(ptr: *mut T) -> u16 {
    (ptr as usize & UPPER_BITS >> UPPER_SHIFT) as u16
}

#[inline]
#[cfg(not(test))]
fn erase_tid<T>(ptr: *mut T) -> *mut T {
    (ptr as usize | UPPER_BITS) as *mut _
}

#[inline]
#[cfg(test)]
fn erase_tid<T>(ptr: *mut T) -> *mut T {
    (ptr as usize & !UPPER_BITS) as *mut _
}

#[inline]
fn atomic_store<T: Copy>(dst: *mut T, val: T, order: Ordering) {
    // SAFETY: the caller must uphold the safety contract for `atomic_store`.
    unsafe {
        match order {
            Release => intrinsics::atomic_store_rel(dst, val),
            Relaxed => intrinsics::atomic_store_relaxed(dst, val),
            SeqCst => intrinsics::atomic_store(dst, val),
            Acquire => panic!("there is no such thing as an acquire store"),
            AcqRel => panic!("there is no such thing as an acquire/release store"),
            _ => panic!(),
        }
    }
}

#[inline]
fn atomic_load<T: Copy>(dst: *const T, order: Ordering) -> T {
    // SAFETY: the caller must uphold the safety contract for `atomic_load`.
    unsafe {
        match order {
            Acquire => intrinsics::atomic_load_acq(dst),
            Relaxed => intrinsics::atomic_load_relaxed(dst),
            SeqCst => intrinsics::atomic_load(dst),
            Release => panic!("there is no such thing as a release load"),
            AcqRel => panic!("there is no such thing as an acquire/release load"),
            _ => panic!(),
        }
    }
}

#[inline]
fn atomic_swap<T: Copy>(dst: *mut T, val: T, order: Ordering) -> T {
    // SAFETY: the caller must uphold the safety contract for `atomic_swap`.
    unsafe {
        match order {
            Acquire => intrinsics::atomic_xchg_acq(dst, val),
            Release => intrinsics::atomic_xchg_rel(dst, val),
            AcqRel => intrinsics::atomic_xchg_acqrel(dst, val),
            Relaxed => intrinsics::atomic_xchg_relaxed(dst, val),
            SeqCst => intrinsics::atomic_xchg(dst, val),
            _ => panic!(),
        }
    }
}

/// Returns the previous value (like __sync_fetch_and_add).
#[inline]
fn atomic_add<T: Copy>(dst: *mut T, val: T, order: Ordering) -> T {
    // SAFETY: the caller must uphold the safety contract for `atomic_add`.
    unsafe {
        match order {
            Acquire => intrinsics::atomic_xadd_acq(dst, val),
            Release => intrinsics::atomic_xadd_rel(dst, val),
            AcqRel => intrinsics::atomic_xadd_acqrel(dst, val),
            Relaxed => intrinsics::atomic_xadd_relaxed(dst, val),
            SeqCst => intrinsics::atomic_xadd(dst, val),
            _ => panic!(),
        }
    }
}

/// Returns the previous value (like __sync_fetch_and_sub).
#[inline]
fn atomic_sub<T: Copy>(dst: *mut T, val: T, order: Ordering) -> T {
    // SAFETY: the caller must uphold the safety contract for `atomic_sub`.
    unsafe {
        match order {
            Acquire => intrinsics::atomic_xsub_acq(dst, val),
            Release => intrinsics::atomic_xsub_rel(dst, val),
            AcqRel => intrinsics::atomic_xsub_acqrel(dst, val),
            Relaxed => intrinsics::atomic_xsub_relaxed(dst, val),
            SeqCst => intrinsics::atomic_xsub(dst, val),
            _ => panic!(),
        }
    }
}

#[inline]
fn atomic_compare_exchange<T: Copy>(
    dst: *mut T,
    old: T,
    new: T,
    success: Ordering,
    failure: Ordering,
) -> Result<T, T> {
    // SAFETY: the caller must uphold the safety contract for `atomic_compare_exchange`.
    let (val, ok) = unsafe {
        match (success, failure) {
            (Acquire, Acquire) => intrinsics::atomic_cxchg_acq(dst, old, new),
            (Release, Relaxed) => intrinsics::atomic_cxchg_rel(dst, old, new),
            (AcqRel, Acquire) => intrinsics::atomic_cxchg_acqrel(dst, old, new),
            (Relaxed, Relaxed) => intrinsics::atomic_cxchg_relaxed(dst, old, new),
            (SeqCst, SeqCst) => intrinsics::atomic_cxchg(dst, old, new),
            (Acquire, Relaxed) => intrinsics::atomic_cxchg_acq_failrelaxed(dst, old, new),
            (AcqRel, Relaxed) => intrinsics::atomic_cxchg_acqrel_failrelaxed(dst, old, new),
            (SeqCst, Relaxed) => intrinsics::atomic_cxchg_failrelaxed(dst, old, new),
            (SeqCst, Acquire) => intrinsics::atomic_cxchg_failacq(dst, old, new),
            (_, AcqRel) => panic!("there is no such thing as an acquire/release failure ordering"),
            (_, Release) => panic!("there is no such thing as a release failure ordering"),
            _ => panic!("a failure ordering can't be stronger than a success ordering"),
        }
    };
    if ok {
        Ok(val)
    } else {
        Err(val)
    }
}

#[inline]
fn atomic_compare_exchange_weak<T: Copy>(
    dst: *mut T,
    old: T,
    new: T,
    success: Ordering,
    failure: Ordering,
) -> Result<T, T> {
    // SAFETY: the caller must uphold the safety contract for `atomic_compare_exchange_weak`.
    let (val, ok) = unsafe {
        match (success, failure) {
            (Acquire, Acquire) => intrinsics::atomic_cxchgweak_acq(dst, old, new),
            (Release, Relaxed) => intrinsics::atomic_cxchgweak_rel(dst, old, new),
            (AcqRel, Acquire) => intrinsics::atomic_cxchgweak_acqrel(dst, old, new),
            (Relaxed, Relaxed) => intrinsics::atomic_cxchgweak_relaxed(dst, old, new),
            (SeqCst, SeqCst) => intrinsics::atomic_cxchgweak(dst, old, new),
            (Acquire, Relaxed) => intrinsics::atomic_cxchgweak_acq_failrelaxed(dst, old, new),
            (AcqRel, Relaxed) => intrinsics::atomic_cxchgweak_acqrel_failrelaxed(dst, old, new),
            (SeqCst, Relaxed) => intrinsics::atomic_cxchgweak_failrelaxed(dst, old, new),
            (SeqCst, Acquire) => intrinsics::atomic_cxchgweak_failacq(dst, old, new),
            (_, AcqRel) => panic!("there is no such thing as an acquire/release failure ordering"),
            (_, Release) => panic!("there is no such thing as a release failure ordering"),
            _ => panic!("a failure ordering can't be stronger than a success ordering"),
        }
    };
    if ok {
        Ok(val)
    } else {
        Err(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let ptr: *mut _ = &mut 5;
        let some_ptr = AtomicPtr::new(ptr);
        let value = some_ptr.load(Ordering::Relaxed);
        assert_eq!(value, ptr);
        let ptr: *mut _ = &mut 10;
        some_ptr.store(ptr, Ordering::Relaxed);
        let value = some_ptr.load(Ordering::Relaxed);
        assert_eq!(value, ptr);
        let new: *mut _ = &mut 15;
        let value = some_ptr.swap(new, Ordering::Relaxed);
        assert_eq!(value, ptr);
        // TODO: more tests
    }
}
