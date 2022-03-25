use super::*;
use crate::frame::buddy_system::LockedFrameAlloc;
use spin::{Lazy, Mutex};
use std::{
    alloc::Layout,
    cell::{Cell, UnsafeCell},
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::{Duration, Instant},
};
use threadpool::ThreadPool;

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

#[derive(Debug)]
pub struct StdEnv {
    chunk: NonNull<u8>,
    page_alloc: LockedFrameAlloc,
    pages: Box<[Page]>,
}
#[derive(Debug, Default)]
struct Page {
    slab: UnsafeCell<Option<NonNull<u8>>>,
}

impl Page {
    #[inline]
    fn set_slab(&self, slab: Option<NonNull<u8>>) {
        unsafe {
            *self.slab.get() = slab;
        }
    }

    #[inline]
    fn get_slab(&self) -> Option<NonNull<u8>> {
        unsafe { *self.slab.get() }
    }
}

impl StdEnv {
    #[inline]
    pub fn new(pages_count: usize) -> Self {
        let chunk = unsafe {
            NonNull::new_unchecked(std::alloc::alloc(Layout::from_size_align_unchecked(
                pages_count * PAGE_SIZE,
                PAGE_SIZE,
            )))
        };
        let page_alloc = LockedFrameAlloc::new();
        page_alloc.lock().insert(0..pages_count);
        let mut pages = Vec::with_capacity(pages_count);
        for _ in 0..pages_count {
            pages.push(Page::default());
        }
        let pages = pages.into_boxed_slice();
        Self {
            chunk,
            page_alloc,
            pages,
        }
    }

    #[inline]
    fn page_addr_to_index(&self, page: NonNull<u8>) -> usize {
        (page.as_ptr() as usize - self.chunk.as_ptr() as usize) / PAGE_SIZE
    }
}

impl Drop for StdEnv {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(
                self.chunk.as_mut(),
                Layout::from_size_align_unchecked(self.pages.len() * PAGE_SIZE, PAGE_SIZE),
            );
        }
    }
}

impl Env for StdEnv {
    type PreemptGuard = ();

    #[inline]
    fn allocate_pages(&self, pages: usize) -> NonNull<u8> {
        let index = self.page_alloc.lock().alloc(pages).unwrap();
        unsafe {
            let ptr = self.chunk.as_ptr().add(index * PAGE_SIZE);
            NonNull::new_unchecked(ptr)
        }
    }

    #[inline]
    fn deallocate_pages(&self, page_start: NonNull<u8>, pages: usize) {
        let index = self.page_addr_to_index(page_start);
        self.page_alloc.lock().dealloc(index, pages);
        for i in index..pages {
            self.pages[i].set_slab(None);
        }
    }

    #[inline]
    fn cpu_id(&self) -> usize {
        Tid::current().as_usize() % 4
    }

    #[inline]
    fn preempt_disable(&self) -> Self::PreemptGuard {}

    #[inline]
    fn set_pages_slab(&self, page_start: NonNull<u8>, pages: usize, slab: NonNull<u8>) {
        let index = self.page_addr_to_index(page_start);
        for i in index..index+pages {
            self.pages[i].set_slab(Some(slab));
        }
    }

    #[inline]
    fn find_slab_by_page(&self, page_start: NonNull<u8>) -> NonNull<u8> {
        let index = self.page_addr_to_index(page_start);
        self.pages[index].get_slab().unwrap()
    }
}

#[derive(Clone)]
pub struct MultiThreadedBench<T> {
    start: Arc<Barrier>,
    end: Arc<Barrier>,
    slab: Arc<T>,
    pool: Arc<ThreadPool>,
}

impl<T: Send + Sync + 'static> MultiThreadedBench<T> {
    pub fn new(slab: Arc<T>, pool: Arc<ThreadPool>) -> Self {
        Self {
            start: Arc::new(Barrier::new(5)),
            end: Arc::new(Barrier::new(5)),
            slab,
            pool,
        }
    }

    pub fn thread(&self, f: impl FnOnce(&Barrier, &T) + Send + 'static) -> &Self {
        let start = self.start.clone();
        let end = self.end.clone();
        let slab = self.slab.clone();
        self.pool.execute(move || {
            f(&*start, &*slab);
            end.wait();
        });
        self
    }

    pub fn run(&self) -> Duration {
        self.start.wait();
        let t0 = Instant::now();
        self.end.wait();
        t0.elapsed()
    }
}
