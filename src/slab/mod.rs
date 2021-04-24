use alloc::boxed::Box;
use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering::*},
};
use dashmap::DashMap;
use smallvec::SmallVec;
use spin::Mutex;

// #[cfg(test)]
pub mod tid;

pub const PAGE_SIZE: usize = 4096;

pub trait MemCacheUtils: Send + Sync {
    type PreemptGuard;

    fn allocate_pages(&self, pages: usize) -> NonNull<u8>;
    fn deallocate_pages(&self, page_start: NonNull<u8>, pages: usize);

    fn cpu_id(&self) -> usize;

    #[must_use]
    fn preempt_disable(&self) -> Self::PreemptGuard;
}

pub struct MemCache<Utils: MemCacheUtils> {
    cpu_slabs: SmallVec<[CachePadded<UnsafeCell<MemCacheCpu>>; 8]>,
    /// The size of an object including meta data
    size: usize,
    /// Pages per slab.
    pages: usize,
    max_partial: usize,
    partial: Mutex<SlabList>,
    slab_map: DashMap<usize, NonNull<Slab>>,
    utils: Utils,
}

struct MemCacheCpu {
    freelist: AtomicPtr<FreeObject>,
    slab: Option<NonNull<Slab>>,
    partial: SlabList,
}

impl MemCacheCpu {
    #[inline]
    const fn new() -> Self {
        Self {
            freelist: AtomicPtr::new(null_mut()),
            slab: None,
            partial: SlabList::new(),
        }
    }

    #[inline]
    fn freelist_pop(&self) -> Option<NonNull<FreeObject>> {
        // because the `freelist` is not actually sharing with other cpus,
        // so the `Relaxed` order is just fine.
        let object = self
            .freelist
            .fetch_update(Relaxed, Relaxed, |head| unsafe {
                Some(head.as_ref()?.next.as_ptr())
            })
            .unwrap_or(null_mut());

        NonNull::new(object)
    }

    #[inline]
    fn replace_slab(&mut self, new_slab: NonNull<Slab>) {
        self.replace_freelist(unsafe { new_slab.as_ref().take_freelist(true) });
        let old_slab = self.slab.replace(new_slab);
        if let Some(mut slab_ptr) = old_slab {
            let slab = unsafe { slab_ptr.as_mut() };
            let mut is_full = true;
            slab.flags
                .fetch_update(AcqRel, Acquire, |flags| {
                    let flags = SlabFlags(flags);
                    is_full = flags.objects() == flags.inuse();
                    if is_full {
                        Some(flags.set_frozen(false).as_usize())
                    } else {
                        Some(flags.as_usize())
                    }
                })
                .unwrap();
            if !is_full {
                self.partial.push(slab_ptr);
            }
        };
    }

    #[inline]
    fn replace_freelist(&mut self, freelist: Option<NonNull<FreeObject>>) {
        *self.freelist.get_mut() = freelist.as_ptr();
    }
}

impl<Utils: MemCacheUtils> MemCache<Utils> {
    pub fn new(
        cpu_count: usize,
        size: usize,
        pages: usize,
        max_partial: usize,
        utils: Utils,
    ) -> Self {
        assert!(size >= 8);

        let mut cpu_slabs = SmallVec::new();
        for _ in 0..cpu_count {
            cpu_slabs.push(CachePadded::new(UnsafeCell::new(MemCacheCpu::new())));
        }

        Self {
            cpu_slabs,
            size,
            pages,
            max_partial,
            partial: Mutex::new(SlabList::new()),
            slab_map: Default::default(),
            utils,
        }
    }

    ///
    /// # Safety
    pub unsafe fn allocate(&self) -> NonNull<u8> {
        let mut object: Option<NonNull<FreeObject>>;
        let mut cache_cpu = NonNull::new_unchecked(self.cpu_slabs[self.utils.cpu_id()].get());

        loop {
            // fast path
            object = cache_cpu.as_ref().freelist_pop();
            if object.is_some() {
                break;
            }

            let _preempt_guard = self.utils.preempt_disable();
            // preemption disabled, it's safe to use a mutable reference.
            let cache_cpu = cache_cpu.as_mut();

            if let Some(cpu_slab) = cache_cpu.slab {
                let freelist = cpu_slab.as_ref().take_freelist(false);
                if freelist.is_some() {
                    cache_cpu.replace_freelist(freelist);
                    continue;
                }
            }

            if let Some(slab) = cache_cpu.partial.pop() {
                cache_cpu.replace_slab(slab);
            } else {
                let mut partial = self.partial.lock();
                if let Some(slab) = partial.pop() {
                    cache_cpu.replace_slab(slab);
                    drop(partial);
                } else {
                    drop(partial);
                    cache_cpu.replace_slab(self.new_slab());
                }
            }
        }

        object.unwrap().cast()
    }

    ///
    /// # Safety
    pub unsafe fn deallocate(&self, object: NonNull<u8>) {
        let mut cache_cpu = NonNull::new_unchecked(self.cpu_slabs[self.utils.cpu_id()].get());
        let slab_ptr = self.find_slab(object);
        let slab = slab_ptr.as_ref();
        let mut object = object.cast::<FreeObject>();

        // fast path
        let result = cache_cpu
            .as_ref()
            .freelist
            .fetch_update(AcqRel, Acquire, |head| {
                (cache_cpu.as_ref().slab == Some(slab_ptr)).then(|| {
                    object.as_mut().next = NonNull::new(head);
                    object.as_ptr()
                })
            });
        if result.is_ok() {
            return;
        }

        let (was_full, is_empty) = slab.push_object(object);
        if was_full {
            let _preempt_guard = self.utils.preempt_disable();
            cache_cpu.as_mut().partial.push(slab_ptr);
            // TODO: move to global partial
        } else if is_empty {
            let _preempt_guard = self.utils.preempt_disable();
            let mut partial = self.partial.lock();
            // check again
            if partial.len() >= self.max_partial && slab.is_empty() {
                partial.remove(slab_ptr);
                self.discard_slab(slab_ptr);
            }
        }
    }

    #[inline]
    fn new_slab(&self) -> NonNull<Slab> {
        let page_start = self.utils.allocate_pages(self.pages);
        let slab = Box::new(Slab::new(page_start, self.pages, self.size));
        let slab = unsafe { NonNull::new_unchecked(Box::leak(slab)) };
        // preempt disabled
        for page in (0..self.pages).map(|i| page_start.as_ptr() as usize + i * PAGE_SIZE) {
            self.slab_map.insert(page, slab);
        }

        slab
    }

    #[inline]
    fn discard_slab(&self, slab: NonNull<Slab>) {
        let slab = unsafe { Box::from_raw(slab.as_ptr()) };
        self.utils.deallocate_pages(slab.page_start, self.pages);
        // preempt disabled
        for page in (0..self.pages).map(|i| slab.page_start.as_ptr() as usize + i * PAGE_SIZE) {
            self.slab_map.remove(&page);
        }
    }

    #[inline]
    fn find_slab(&self, object: NonNull<u8>) -> NonNull<Slab> {
        let page = align_down(object.as_ptr() as usize, PAGE_SIZE);
        let _preempt_guard = self.utils.preempt_disable();
        *self.slab_map.get(&page).unwrap().value()
    }
}

impl<Utils: MemCacheUtils> Drop for MemCache<Utils> {
    fn drop(&mut self) {
        for cpu_slab in self.cpu_slabs.iter() {
            let cpu_slab = unsafe { cpu_slab.get().as_mut().unwrap() };
            while let Some(slab) = cpu_slab.partial.pop() {
                let slab = unsafe { slab.as_ref() };
                self.utils.deallocate_pages(slab.page_start, self.pages);
            }
            if let Some(slab) = cpu_slab.slab.take() {
                let slab = unsafe { slab.as_ref() };
                self.utils.deallocate_pages(slab.page_start, self.pages);
            }
        }
        let mut partial = self.partial.lock();
        while let Some(slab) = partial.pop() {
            let slab = unsafe { slab.as_ref() };
            self.utils.deallocate_pages(slab.page_start, self.pages);
        }
    }
}

/// Align address downwards.
///
/// Returns the greatest x with alignment `align` so that x <= addr. The alignment must be
///  a power of 2.
#[inline]
pub fn align_down(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "`align` must be a power of two");
    addr & !(align - 1)
}

unsafe impl<T: MemCacheUtils> Send for MemCache<T> {}
unsafe impl<T: MemCacheUtils> Sync for MemCache<T> {}

pub struct Slab {
    page_start: NonNull<u8>,
    freelist: AtomicPtr<FreeObject>,
    /// inuse: `0..32`, objects: `32..63`, frozen(in `cache_cpu`): `63..64`
    flags: AtomicUsize,
    prev_slab: Option<NonNull<Slab>>,
    next_slab: Option<NonNull<Slab>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
/// inuse: `0..32`, objects: `32..63`, frozen(in `cache_cpu`): `63..64`
struct SlabFlags(usize);

impl SlabFlags {
    const FROZEN_SHIFT: usize = 63;
    const FROZEN_BITMASK: usize = 0x1 << Self::FROZEN_SHIFT;
    const OBJECTS_SHIFT: usize = 32;
    const OBJECTS_BITMASK: usize = 0x7FFF_FFFF << Self::OBJECTS_SHIFT;
    const INUSE_SHIFT: usize = 0;
    const INUSE_BITMASK: usize = 0xFFFF_FFFF;

    #[inline]
    const fn new(frozen: bool, objects: usize, inuse: usize) -> Self {
        Self(
            ((frozen as usize) << Self::FROZEN_SHIFT)
                | (objects << Self::OBJECTS_SHIFT)
                | (inuse << Self::INUSE_SHIFT),
        )
    }

    #[inline]
    const fn is_frozen(&self) -> bool {
        (self.0 & Self::FROZEN_BITMASK >> Self::FROZEN_SHIFT) != 0
    }

    #[inline]
    const fn set_frozen(self, frozen: bool) -> Self {
        Self(self.0 & !Self::FROZEN_BITMASK | ((frozen as usize) << Self::FROZEN_SHIFT))
    }

    #[inline]
    const fn objects(&self) -> usize {
        self.0 & Self::OBJECTS_BITMASK >> Self::OBJECTS_SHIFT
    }

    #[inline]
    const fn set_objects(self, objects: usize) -> Self {
        Self(self.0 & !Self::OBJECTS_BITMASK | (objects << Self::OBJECTS_SHIFT))
    }

    #[inline]
    const fn inuse(&self) -> usize {
        self.0 & Self::INUSE_BITMASK >> Self::INUSE_SHIFT
    }

    #[inline]
    const fn set_inuse(self, inuse: usize) -> Self {
        Self(self.0 & !Self::INUSE_BITMASK | (inuse << Self::INUSE_SHIFT))
    }

    #[inline]
    const fn as_usize(self) -> usize {
        self.0
    }
}

unsafe impl Send for Slab {}
unsafe impl Sync for Slab {}

impl Slab {
    fn new(page_start: NonNull<u8>, pages: usize, size: usize) -> Self {
        let objects_per_page = PAGE_SIZE / size;
        let objects = objects_per_page * pages;
        unsafe {
            let mut last_object: *mut FreeObject = null_mut();
            for page in (0..pages).map(|i| page_start.as_ptr().add(PAGE_SIZE * i)) {
                if let Some(last_object) = last_object.as_mut() {
                    last_object.next.replace(NonNull::new_unchecked(page as _));
                }
                for (obj, next) in (0..objects_per_page - 1).map(|i| {
                    (
                        page.add(i * size).cast::<FreeObject>(),
                        page.add((i + 1) * size).cast::<FreeObject>(),
                    )
                }) {
                    obj.as_mut()
                        .unwrap()
                        .next
                        .replace(NonNull::new_unchecked(next));
                }
                last_object = page.add((objects_per_page - 1) * size).cast();
            }
            last_object.as_mut().unwrap().next.take();
        }

        Self {
            page_start,
            freelist: AtomicPtr::new(page_start.as_ptr().cast()),
            flags: AtomicUsize::new(SlabFlags::new(false, objects, 0).as_usize()),
            prev_slab: None,
            next_slab: None,
        }
    }

    #[inline]
    fn is_full(&self) -> bool {
        let flags = SlabFlags(self.flags.load(Acquire));
        !flags.is_frozen() && flags.objects() == flags.inuse()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        let flags = SlabFlags(self.flags.load(Acquire));
        !flags.is_frozen() && flags.inuse() == 0
    }

    /// Returns `(was_full, is_empty_now)`
    fn push_object(&self, mut object: NonNull<FreeObject>) -> (bool, bool) {
        self.freelist
            .fetch_update(AcqRel, Acquire, |current| unsafe {
                object.as_mut().next = NonNull::new(current);
                Some(object.as_ptr())
            })
            .unwrap();

        let flags = SlabFlags(self.flags.fetch_sub(1, Release));
        let was_full = !flags.is_frozen() && flags.objects() == flags.inuse();
        let is_empty = !flags.is_frozen() && flags.inuse() == 1;

        (was_full, is_empty)
    }

    fn take_freelist(&self, froze: bool) -> Option<NonNull<FreeObject>> {
        if froze {
            self.flags
                .fetch_update(AcqRel, Acquire, |flags| {
                    let flags = SlabFlags(flags);
                    Some(flags.set_inuse(flags.objects()).set_frozen(true).as_usize())
                })
                .unwrap();
        }
        let freelist = self
            .freelist
            .fetch_update(AcqRel, Acquire, |_| Some(null_mut()))
            .unwrap();
        NonNull::new(freelist)
    }
}

#[derive(Debug, Default)]
struct SlabList {
    head: Option<NonNull<Slab>>,
    len: usize,
}

impl SlabList {
    #[inline]
    const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    #[inline]
    const fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, mut slab: NonNull<Slab>) {
        unsafe {
            if let Some(mut head) = self.head {
                head.as_mut().prev_slab = Some(slab);
            }
            slab.as_mut().prev_slab.take();
            slab.as_mut().next_slab = self.head;
            self.head = Some(slab);
            self.len += 1;
        }
    }

    fn pop(&mut self) -> Option<NonNull<Slab>> {
        match self.head {
            Some(mut head_ptr) => unsafe {
                let head = head_ptr.as_mut();
                if let Some(mut next) = head.next_slab {
                    next.as_mut().prev_slab.take();
                }
                head.next_slab.take();
                self.len -= 1;
                self.head = head.next_slab;

                Some(head_ptr)
            },
            None => None,
        }
    }

    fn remove(&mut self, mut slab_ptr: NonNull<Slab>) {
        unsafe {
            let slab = slab_ptr.as_mut();
            if let Some(mut prev) = slab.prev_slab {
                prev.as_mut().next_slab = slab.next_slab;
            }
            if let Some(mut next) = slab.next_slab {
                next.as_mut().prev_slab = slab.prev_slab;
            }
            if self.head == Some(slab_ptr) {
                self.head = slab.next_slab;
            }
            slab.prev_slab.take();
            slab.next_slab.take();
            self.len -= 1;
        }
    }
}

#[repr(C)]
struct FreeObject {
    next: Option<NonNull<FreeObject>>,
}

trait AsPtr<T> {
    fn as_ptr(&self) -> *mut T;
}

impl<T> AsPtr<T> for Option<NonNull<T>> {
    fn as_ptr(&self) -> *mut T {
        match self {
            Some(p) => p.as_ptr(),
            None => null_mut(),
        }
    }
}

pub mod bench {
    use super::*;
    use std::{
        alloc::Layout,
        sync::{Arc, Barrier},
        thread::spawn,
        time::{Duration, Instant},
    };
    #[derive(Debug, Default, Clone)]
    pub struct ThreadedMemCacheUtils {}

    impl ThreadedMemCacheUtils {
        #[inline]
        pub fn new() -> Self {
            Self {}
        }
    }

    impl MemCacheUtils for ThreadedMemCacheUtils {
        type PreemptGuard = ();

        #[inline]
        fn allocate_pages(&self, pages: usize) -> NonNull<u8> {
            unsafe {
                let ptr = std::alloc::alloc(Layout::from_size_align_unchecked(
                    pages * PAGE_SIZE,
                    PAGE_SIZE,
                ));
                NonNull::new_unchecked(ptr)
            }
        }

        #[inline]
        fn deallocate_pages(&self, page_start: NonNull<u8>, pages: usize) {
            unsafe {
                std::alloc::dealloc(
                    page_start.as_ptr(),
                    Layout::from_size_align_unchecked(pages * PAGE_SIZE, PAGE_SIZE),
                );
            }
        }

        #[inline]
        fn cpu_id(&self) -> usize {
            tid::Tid::current().as_usize() % 4
        }

        #[inline]
        fn preempt_disable(&self) -> Self::PreemptGuard {}
    }

    #[derive(Clone)]
    pub struct MultiThreadedBench<T> {
        start: Arc<Barrier>,
        end: Arc<Barrier>,
        slab: Arc<T>,
    }

    impl<T: Send + Sync + 'static> MultiThreadedBench<T> {
        pub fn new(slab: Arc<T>) -> Self {
            Self {
                start: Arc::new(Barrier::new(5)),
                end: Arc::new(Barrier::new(5)),
                slab,
            }
        }

        pub fn thread(&self, f: impl FnOnce(&Barrier, &T) + Send + 'static) -> &Self {
            let start = self.start.clone();
            let end = self.end.clone();
            let slab = self.slab.clone();
            spawn(move || {
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
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread::spawn};

    use super::{bench::*, *};

    fn dummy_mem_cache(cpu_count: usize) -> MemCache<ThreadedMemCacheUtils> {
        let utils = ThreadedMemCacheUtils::new();
        MemCache::new(cpu_count, 16, 4, 8, utils)
    }

    #[test]
    fn usages() {
        let cpu_count = 8;
        let mem_cache = Arc::new(dummy_mem_cache(8));
        let threads: Vec<_> = (0..cpu_count)
            .into_iter()
            .map(|_| {
                let mem_cache = mem_cache.clone();
                spawn(move || unsafe {
                    let object = mem_cache.allocate();
                    mem_cache.deallocate(object);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}
