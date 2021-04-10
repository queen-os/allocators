use bit_field::BitField;
use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicUsize, Ordering::*},
};
use smallvec::SmallVec;
use spin::Mutex;

mod atomic;
use atomic::AtomicPtr;

const PAGE_SIZE: usize = 4096;

pub trait MemCacheUtils: Send + Sync {
    fn allocate_pages(&self, pages: usize) -> NonNull<u8>;
    fn deallocate_pages(&self, page_start: NonNull<u8>, pages: usize);

    fn cpu_id(&self) -> usize;

    fn allocate<T>(&self, x: T) -> NonNull<T>;
    fn deallocate<T>(&self, ptr: NonNull<T>);

    fn preempt_enable(&self);
    fn preempt_disable(&self);
}

pub struct MemCache<Utils: MemCacheUtils> {
    cpu_slabs: SmallVec<[CachePadded<MemCacheCpu>; 8]>,
    /// The size of an object including meta data
    size: usize,
    /// Pages per slab.
    pages: usize,
    max_partial: usize,
    partial: Mutex<SlabList>,
    utils: Utils,
}

struct MemCacheCpu {
    freelist: AtomicPtr<FreeObject>,
    slab: UnsafeCell<*mut Slab>,
}

impl MemCacheCpu {
    #[inline]
    fn freelist_pop(&self) -> *mut FreeObject {
        self.freelist
            .fetch_update(SeqCst, SeqCst, |head| unsafe { Some(head.as_ref()?.next) })
            .unwrap_or(null_mut())
    }

    #[inline]
    fn deactivate_slab(&self) {
        unsafe {
            if let Some(slab) = (*self.slab.get()).as_ref() {
                // set frozen to false.
                slab.flags
                    .fetch_update(AcqRel, Acquire, |mut flags| Some(*flags.set_bit(63, false)))
                    .unwrap();

                *self.slab.get() = null_mut();
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
            *self.slab.get() = new_slab.as_ptr();
        }
    }

    #[inline]
    fn replace_freelist(&self, freelist: *mut FreeObject) {
        self.freelist
            .fetch_update(Relaxed, Relaxed, |_| Some(freelist))
            .unwrap();
    }
}

impl<Utils: MemCacheUtils> MemCache<Utils> {
    ///
    /// # Safety
    pub unsafe fn allocate(&self) -> NonNull<u8> {
        let mut object: *mut FreeObject;
        let cache_cpu = &self.cpu_slabs[self.utils.cpu_id()];

        loop {
            // fast path
            object = cache_cpu.freelist_pop();
            if !object.is_null() {
                break;
            }

            self.utils.preempt_disable();
            if let Some(cpu_slab) = NonNull::new(*cache_cpu.slab.get()) {
                let freelist = cpu_slab.as_ref().take_freelist();
                if !freelist.is_null() {
                    cache_cpu.replace_freelist(freelist);
                    self.utils.preempt_enable();
                    continue;
                }
            }
            let mut partial = self.partial.lock();
            cache_cpu.deactivate_slab();
            if let Some(slab) = NonNull::new(partial.pop()) {
                cache_cpu.replace_slab(slab);
                drop(partial);
            } else {
                drop(partial);
                cache_cpu.replace_slab(self.new_slab());
            }
            self.utils.preempt_enable();
        }

        NonNull::new_unchecked(object).cast()
    }

    ///
    /// # Safety
    pub unsafe fn deallocate(&self, object: NonNull<u8>) {
        let slab = self.find_slab(object).as_mut();
        let mut object = object.cast::<FreeObject>();
        let cache_cpu = &self.cpu_slabs[self.utils.cpu_id()];

        // fast path
        let result = cache_cpu.freelist.fetch_update(AcqRel, Acquire, |head| {
            (*cache_cpu.slab.get() == slab).then(|| {
                object.as_mut().next = head;
                object.as_ptr()
            })
        });
        if result.is_ok() {
            return;
        }

        if slab.is_full() {
            let mut partial = self.partial.lock();
            // check again
            if slab.is_full() {
                partial.push(NonNull::new_unchecked(slab));
            }
            slab.push_object(object);
        } else {
            slab.push_object(object);
            if slab.is_empty() {
                let mut partial = self.partial.lock();
                // check again
                if partial.len() >= self.max_partial && slab.is_empty() {
                    partial.remove(NonNull::new_unchecked(slab));
                    self.discard_slab(NonNull::new_unchecked(slab));
                }
            }
        }
    }

    #[inline]
    fn new_slab(&self) -> NonNull<Slab> {
        let page_start = self.utils.allocate_pages(self.pages);
        self.utils
            .allocate(Slab::new(page_start, self.pages, self.size))
    }

    #[inline]
    fn discard_slab(&self, slab: NonNull<Slab>) {
        unsafe {
            self.utils
                .deallocate_pages(slab.as_ref().page_start, self.pages);
        }
        self.utils.deallocate(slab);
        // TODO: remove from slab map
    }

    fn find_slab(&self, object: NonNull<u8>) -> NonNull<Slab> {
        todo!()
    }
}

unsafe impl<T: MemCacheUtils> Send for MemCache<T> {}
unsafe impl<T: MemCacheUtils> Sync for MemCache<T> {}

pub struct Slab {
    page_start: NonNull<u8>,
    freelist: AtomicPtr<FreeObject>,
    /// inuse: `0..32`, objects: `32..63`, frozen(in `cache_cpu`): `63..64`
    flags: AtomicUsize,
    prev_slab: *mut Slab,
    next_slab: *mut Slab,
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
                    last_object.next = page as *mut _;
                }
                for (obj, next) in (0..objects_per_page - 1).map(|i| {
                    (
                        page.add(i * size).cast::<FreeObject>(),
                        page.add((i + 1) * size).cast::<FreeObject>(),
                    )
                }) {
                    obj.as_mut().unwrap().next = next;
                }
                last_object = page.add((objects_per_page - 1) * size).cast();
            }
            last_object.as_mut().unwrap().next = null_mut();
        }

        Self {
            page_start,
            freelist: AtomicPtr::new(page_start.as_ptr().cast()),
            flags: AtomicUsize::new(*0.set_bits(32..63, objects)),
            prev_slab: null_mut(),
            next_slab: null_mut(),
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
                object.as_mut().next = current;
                Some(object.as_ptr())
            })
            .unwrap();
        self.flags.fetch_sub(1, Release);
    }

    fn take_freelist(&self) -> *mut FreeObject {
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
        freelist
    }
}

struct SlabList {
    head: *mut Slab,
    len: usize,
}

impl SlabList {
    #[inline]
    fn new() -> Self {
        Self {
            head: null_mut(),
            len: 0,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    fn push(&mut self, mut slab: NonNull<Slab>) {
        unsafe {
            let head = self.head;
            if !head.is_null() {
                head.as_mut().unwrap().prev_slab = slab.as_ptr();
            }
            slab.as_mut().prev_slab = null_mut();
            slab.as_mut().next_slab = head;
            self.head = slab.as_ptr();
            self.len += 1;
        }
    }

    fn pop(&mut self) -> *mut Slab {
        if self.is_empty() {
            return null_mut();
        }

        let head = unsafe { self.head.as_mut().unwrap() };
        let next = head.next_slab;
        if let Some(next) = unsafe { next.as_mut() } {
            next.prev_slab = null_mut();
        }
        self.head = next;
        head.next_slab = null_mut();
        self.len -= 1;

        head
    }

    fn remove(&mut self, mut slab: NonNull<Slab>) {
        unsafe {
            let slab = slab.as_mut();
            if let Some(prev) = slab.prev_slab.as_mut() {
                prev.next_slab = slab.next_slab;
            }
            if let Some(next) = slab.next_slab.as_mut() {
                next.prev_slab = slab.prev_slab;
            }
            if self.head == slab {
                self.head = slab.next_slab;
            }
            slab.prev_slab = null_mut();
            slab.next_slab = null_mut();
            self.len -= 1;
        }
    }
}

struct FreeObject {
    next: *mut FreeObject,
}
