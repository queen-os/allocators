use spin::{Lazy, Mutex};
use std::{
    cell::{Cell, UnsafeCell},
    collections::VecDeque,
    marker::PhantomData,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Uniquely identifies a thread.
pub struct Tid {
    id: usize,
    _not_send: PhantomData<UnsafeCell<()>>,
}

#[derive(Debug)]
struct Registration(Cell<Option<usize>>);

struct Registry {
    next: AtomicUsize,
    free: Mutex<VecDeque<usize>>,
}

static REGISTRY: Lazy<Registry> = Lazy::new(|| Registry {
    next: AtomicUsize::new(0),
    free: Mutex::new(VecDeque::new()),
});

thread_local! {
    static REGISTRATION: Registration = Registration::new();
}

impl Tid {
    #[inline(always)]
    pub fn as_usize(&self) -> usize {
        self.id
    }

    #[inline(always)]
    fn from_usize(id: usize) -> Self {
        Self {
            id,
            _not_send: PhantomData,
        }
    }
}

impl Tid {
    #[inline]
    pub fn current() -> Self {
        REGISTRATION.try_with(Registration::current).unwrap()
    }

    #[inline]
    pub fn is_current(self) -> bool {
        REGISTRATION
            .try_with(|r| self == r.current())
            .unwrap_or(false)
    }

    #[inline(always)]
    pub fn new(id: usize) -> Self {
        Self::from_usize(id)
    }
}

impl PartialEq for Tid {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Tid {}

impl Clone for Tid {
    fn clone(&self) -> Self {
        Self::new(self.id)
    }
}

impl Copy for Tid {}

impl Registration {
    fn new() -> Self {
        Self(Cell::new(None))
    }

    #[inline(always)]
    fn current(&self) -> Tid {
        if let Some(tid) = self.0.get().map(Tid::new) {
            tid
        } else {
            self.register()
        }
    }

    #[cold]
    fn register(&self) -> Tid {
        let mut free = REGISTRY.free.lock();
        let id = free
            .pop_back()
            .unwrap_or_else(|| REGISTRY.next.fetch_add(1, Ordering::Relaxed));
        self.0.set(Some(id));
        Tid::new(id)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(id) = self.0.get() {
            REGISTRY.free.lock().push_back(id);
        }
    }
}
