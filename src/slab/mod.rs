use alloc::{boxed::Box, collections::BTreeMap};
use bit_field::BitField;
use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicUsize, Ordering::*},
};
use smallvec::{smallvec, SmallVec};
use spin::{Mutex, RwLock};

mod atomic;
use atomic::AtomicPtr;

const PAGE_SIZE: usize = 4096;

pub trait MemCacheUtils: Send + Sync {
    type PreemptGuard;

    fn allocate_pages(&self, pages: usize) -> NonNull<u8>;
    fn deallocate_pages(&self, page_start: NonNull<u8>, pages: usize);

    fn cpu_id(&self) -> usize;

    #[must_use]
    fn preempt_disable(&self) -> Self::PreemptGuard;
}

pub struct MemCache<Utils: MemCacheUtils> {
    cpu_slabs: SmallVec<[CachePadded<MemCacheCpu>; 8]>,
    /// The size of an object including meta data
    size: usize,
    /// Pages per slab.
    pages: usize,
    max_partial: usize,
    partial: Mutex<SlabList>,
    slab_map: RwLock<BTreeMap<usize, NonNull<Slab>>>,
    utils: Utils,
}

struct MemCacheCpu {
    freelist: AtomicPtr<FreeObject>,
    slab: UnsafeCell<Option<NonNull<Slab>>>,
}

impl MemCacheCpu {
    fn new() -> Self {
        Self {
            freelist: AtomicPtr::new(null_mut()),
            slab: UnsafeCell::new(None),
        }
    }
}

impl Default for MemCacheCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl MemCacheCpu {
    #[inline]
    fn freelist_pop(&self) -> Option<NonNull<FreeObject>> {
        let object = self
            .freelist
            .fetch_update(SeqCst, SeqCst, |head| unsafe {
                Some(head.as_ref()?.next.as_ptr())
            })
            .unwrap_or(null_mut());

        NonNull::new(object)
    }

    #[inline]
    fn deactivate_slab(&self) {
        unsafe {
            if let Some(slab) = *self.slab.get() {
                // set frozen to false.
                slab.as_ref()
                    .flags
                    .fetch_update(AcqRel, Acquire, |mut flags| Some(*flags.set_bit(63, false)))
                    .unwrap();

                (*self.slab.get()).take();
            };
        }
    }

    #[inline]
    fn replace_slab(&self, new_slab: NonNull<Slab>) {
        unsafe {
            new_slab
                .as_ref()
                .flags
                .fetch_update(AcqRel, Acquire, |mut flags| {
                    let objects = flags.get_bits(32..63);
                    flags.set_bit(63, true).set_bits(0..32, objects);
                    Some(flags)
                })
                .unwrap();
            self.replace_freelist(new_slab.as_ref().take_freelist());
            (*self.slab.get()).replace(new_slab);
        }
    }

    #[inline]
    fn replace_freelist(&self, freelist: Option<NonNull<FreeObject>>) {
        self.freelist
            .fetch_update(Relaxed, Relaxed, |_| Some(freelist.as_ptr()))
            .unwrap();
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

        let mut cpu_slabs = smallvec![];
        for _ in 0..cpu_count {
            cpu_slabs.push(Default::default());
        }

        Self {
            cpu_slabs,
            size,
            pages,
            max_partial,
            partial: Default::default(),
            slab_map: Default::default(),
            utils,
        }
    }

    ///
    /// # Safety
    pub unsafe fn allocate(&self) -> NonNull<u8> {
        let mut object: Option<NonNull<FreeObject>>;
        let cache_cpu = &self.cpu_slabs[self.utils.cpu_id()];

        loop {
            // fast path
            object = cache_cpu.freelist_pop();
            if object.is_some() {
                break;
            }

            let _preempt_guard = self.utils.preempt_disable();
            if let Some(cpu_slab) = *cache_cpu.slab.get() {
                let freelist = cpu_slab.as_ref().take_freelist();
                if freelist.is_some() {
                    cache_cpu.replace_freelist(freelist);
                    continue;
                }
            }
            let mut partial = self.partial.lock();
            cache_cpu.deactivate_slab();
            if let Some(slab) = partial.pop() {
                cache_cpu.replace_slab(slab);
                drop(partial);
            } else {
                drop(partial);
                cache_cpu.replace_slab(self.new_slab());
            }
        }

        object.unwrap().cast()
    }

    ///
    /// # Safety
    pub unsafe fn deallocate(&self, object: NonNull<u8>) {
        let mut slab_ptr = self.find_slab(object);
        let slab = slab_ptr.as_mut();
        let mut object = object.cast::<FreeObject>();
        let cache_cpu = &self.cpu_slabs[self.utils.cpu_id()];

        // fast path
        let result = cache_cpu.freelist.fetch_update(AcqRel, Acquire, |head| {
            (*cache_cpu.slab.get() == Some(slab_ptr)).then(|| {
                object.as_mut().next = NonNull::new(head);
                object.as_ptr()
            })
        });
        if result.is_ok() {
            return;
        }

        if slab.is_full() {
            let _preempt_guard = self.utils.preempt_disable();
            let mut partial = self.partial.lock();
            // check again
            if slab.is_full() {
                partial.push(NonNull::new_unchecked(slab));
            }
        }
        slab.push_object(object);
        if slab.is_empty() {
            let _preempt_guard = self.utils.preempt_disable();
            let mut partial = self.partial.lock();
            // check again
            if partial.len() >= self.max_partial && slab.is_empty() {
                partial.remove(NonNull::new_unchecked(slab));
                self.discard_slab(NonNull::new_unchecked(slab));
            }
        }
    }

    #[inline]
    fn new_slab(&self) -> NonNull<Slab> {
        let page_start = self.utils.allocate_pages(self.pages);
        let slab = Box::new(Slab::new(page_start, self.pages, self.size));
        let slab = unsafe { NonNull::new_unchecked(Box::leak(slab)) };
        // preempt disabled
        let mut slab_map = self.slab_map.write();
        for page in (0..self.pages).map(|i| page_start.as_ptr() as usize + i * PAGE_SIZE) {
            slab_map.insert(page, slab);
        }

        slab
    }

    #[inline]
    fn discard_slab(&self, slab: NonNull<Slab>) {
        let slab = unsafe { Box::from_raw(slab.as_ptr()) };
        self.utils.deallocate_pages(slab.page_start, self.pages);
        // preempt disabled
        let mut slab_map = self.slab_map.write();
        for page in (0..self.pages).map(|i| slab.page_start.as_ptr() as usize + i * PAGE_SIZE) {
            slab_map.remove(&page);
        }
    }

    fn find_slab(&self, object: NonNull<u8>) -> NonNull<Slab> {
        let page = align_down(object.as_ptr() as usize, PAGE_SIZE);
        let _preempt_guard = self.utils.preempt_disable();
        let slab_map = self.slab_map.read();
        *slab_map.get(&page).unwrap()
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
            flags: AtomicUsize::new(*0.set_bits(32..63, objects)),
            prev_slab: None,
            next_slab: None,
        }
    }

    #[inline]
    fn is_full(&self) -> bool {
        let flags = self.flags.load(Acquire);
        // `!frozen && objects == inuse`
        !flags.get_bit(63) && flags.get_bits(32..63) == flags.get_bits(0..32)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        let flags = self.flags.load(Acquire);
        // `!frozen && inuse == 0`
        !flags.get_bit(63) && flags.get_bits(0..32) == 0
    }

    fn push_object(&self, mut object: NonNull<FreeObject>) {
        self.freelist
            .fetch_update(SeqCst, SeqCst, |current| unsafe {
                object.as_mut().next = NonNull::new(current);
                Some(object.as_ptr())
            })
            .unwrap();
        self.flags.fetch_sub(1, Release);
    }

    fn take_freelist(&self) -> Option<NonNull<FreeObject>> {
        let freelist = self
            .freelist
            .fetch_update(AcqRel, Acquire, |_| Some(null_mut()))
            .unwrap();
        self.flags
            .fetch_update(AcqRel, Acquire, |mut flags| {
                let objects = flags.get_bits(32..63);
                Some(*flags.set_bits(0..32, objects))
            })
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
    fn len(&self) -> usize {
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
    fn as_ptr(self) -> *mut T;
}

impl<T> AsPtr<T> for Option<NonNull<T>> {
    fn as_ptr(self) -> *mut T {
        match self {
            Some(p) => p.as_ptr(),
            None => null_mut(),
        }
    }
}
