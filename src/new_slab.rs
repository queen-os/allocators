use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    ptr::{null_mut, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering::*},
};
use spin::Mutex;

pub const PAGE_SIZE: usize = 4096;

pub trait Env {
    type PreemptGuard;

    #[must_use]
    fn preempt_disable(&self) -> Self::PreemptGuard;

    fn allocate_pages(&self, count: usize) -> NonNull<u8>;
    fn deallocate_pages(&self, start_page: NonNull<u8>, count: usize);

    fn cpu_id(&self) -> usize;
}

pub struct SlabAlloc<
    E: Env,
    const OBJECT_SIZE: usize,
    const PAGES_PER_SLAB: usize = 16,
    const MAX_PARTIALS: usize = 16,
    const MAX_PARTIALS_PER_CPU: usize = 8,
    const CPU_COUNT: usize = 32,
> {
    cpu_slabs: [CachePadded<UnsafeCell<SlabAllocPerCpu<MAX_PARTIALS_PER_CPU>>>; CPU_COUNT],
    partials: Mutex<SlabList>,
    env: E,
}

impl<
        E: Env,
        const OBJECT_SIZE: usize,
        const PAGES_PER_SLAB: usize,
        const MAX_PARTIALS: usize,
        const MAX_PARTIALS_PER_CPU: usize,
        const CPU_COUNT: usize,
    > SlabAlloc<E, OBJECT_SIZE, PAGES_PER_SLAB, MAX_PARTIALS, MAX_PARTIALS_PER_CPU, CPU_COUNT>
{
    fn new_slab(&self) -> NonNull<Slab> {
        let ptr = self.env.allocate_pages(PAGES_PER_SLAB);
        unsafe { Slab::init::<OBJECT_SIZE, PAGES_PER_SLAB>(ptr) }
        ptr.cast()
    }
}

struct SlabAllocPerCpu<const MAX_PARTIALS: usize> {
    freelist: AtomicPtr<FreeObject>,
    slab: Option<NonNull<Slab>>,
    partials: SlabList,
}

#[repr(C)]
struct FreeObject {
    next: Option<NonNull<FreeObject>>,
}

struct Slab {
    freelist: AtomicPtr<FreeObject>,
    /// [`SlabFlags`]
    flags: AtomicUsize,
    prev_slab: Option<NonNull<Slab>>,
    next_slab: Option<NonNull<Slab>>,
}

impl Slab {
    unsafe fn init<const OBJECT_SIZE: usize, const PAGES: usize>(ptr: NonNull<u8>) {
        
    }
}

#[derive(Debug, Default)]
struct SlabList {
    head: Option<NonNull<Slab>>,
    len: usize,
}

impl SlabList {
    const fn new() -> Self {
        Self { head: None, len: 0 }
    }

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
