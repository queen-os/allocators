use core::{
    alloc::{GlobalAlloc, Layout},
    cmp::max,
    iter::Iterator,
    marker::PhantomData,
    mem::size_of,
    ops::{Deref, Range},
    ptr::{null, null_mut, NonNull},
};

use spin::Mutex;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct BlockMetaInfo(usize);

impl BlockMetaInfo {
    const SIZE: usize = size_of::<Self>();

    #[inline]
    fn new(payload_size: usize, used: bool) -> Self {
        debug_assert!(payload_size.trailing_zeros() >= 1);
        // The last bit is "allocated bit" since payload_size is always multiple of 2.
        BlockMetaInfo(payload_size | used as usize)
    }

    #[inline]
    fn is_used(&self) -> bool {
        (self.0 & 1) == 1
    }

    #[inline]
    fn payload_size(&self) -> usize {
        self.0 & !1
    }

    #[inline]
    fn set_used(&mut self, used: bool) {
        if used {
            self.0 |= 1;
        } else {
            self.0 &= !1;
        }
    }

    #[inline]
    fn set_payload_size(&mut self, payload_size: usize) {
        self.0 = payload_size | self.is_used() as usize;
    }
}

#[repr(C)]
struct Block {
    header: BlockMetaInfo,
    payload: [u8; 1],
    // footer: BlockMetaInfo,
}

impl Block {
    unsafe fn new_free_block(
        mut block: NonNull<Self>,
        payload_size: usize,
        prev: *const Self,
        next: *const Self,
    ) -> NonNull<Self> {
        block
            .as_mut()
            .set_meta_info(BlockMetaInfo::new(payload_size, false));
        block.as_mut().set_logical_prev(prev);
        block.as_mut().set_logical_next(next);
        block
    }

    #[inline]
    unsafe fn from_payload(payload: NonNull<u8>) -> NonNull<Self> {
        NonNull::new_unchecked(payload.as_ptr().sub(BlockMetaInfo::SIZE)).cast()
    }

    unsafe fn from_footer(footer: NonNull<u8>) -> NonNull<Self> {
        let payload_size = footer.cast::<BlockMetaInfo>().as_ref().payload_size();
        NonNull::new_unchecked(
            footer
                .as_ptr()
                .sub(payload_size as usize + BlockMetaInfo::SIZE),
        )
        .cast()
    }

    #[inline]
    fn is_used(&self) -> bool {
        self.header.is_used()
    }

    #[inline]
    fn payload_size(&self) -> usize {
        self.header.payload_size()
    }

    #[inline]
    fn payload_mut_ptr(&mut self) -> *mut u8 {
        self.payload.as_mut_ptr()
    }

    #[inline]
    fn size(&self) -> usize {
        self.payload_size() + BlockMetaInfo::SIZE * 2
    }

    #[inline]
    fn set_used(&mut self, used: bool) {
        self.header.set_used(used);
        self.footer_mut().set_used(used);
    }

    #[inline]
    fn set_payload_size(&mut self, payload_size: usize) {
        // set header first, so could set footer correctly
        self.header.set_payload_size(payload_size);
        self.footer_mut().set_payload_size(payload_size);
    }

    #[inline]
    fn set_meta_info(&mut self, meta_info: BlockMetaInfo) {
        // set header first, so could set footer correctly
        self.header = meta_info;
        *self.footer_mut() = meta_info;
    }

    fn footer_mut(&mut self) -> &mut BlockMetaInfo {
        unsafe {
            self.payload_mut_ptr()
                .add(self.payload_size())
                .cast::<BlockMetaInfo>()
                .as_mut()
                .unwrap()
        }
    }

    /// # Safety
    /// Must ensure `self` is unused.
    #[inline]
    unsafe fn logical_prev(&mut self) -> *mut Self {
        debug_assert!(!self.is_used());
        *(self.payload.as_mut_ptr().cast::<*mut Block>())
    }

    /// # Safety
    /// Must ensure there's previous one.
    #[inline]
    unsafe fn physical_prev(&mut self) -> *mut Self {
        Block::from_footer(NonNull::new_unchecked(
            (self as *mut _ as *mut u8).sub(BlockMetaInfo::SIZE),
        ))
        .as_ptr()
    }

    /// # Safety
    /// Must ensure `self` is unused.
    #[inline]
    unsafe fn set_logical_prev(&mut self, prev: *const Self) {
        debug_assert!(!self.is_used());
        *self.payload.as_mut_ptr().cast::<*const Block>() = prev;
    }

    /// # Safety
    /// Must ensure `self` is unused.
    #[inline]
    unsafe fn logical_next(&mut self) -> *mut Self {
        debug_assert!(!self.is_used());
        *(self.payload.as_mut_ptr().cast::<*mut Block>().add(1))
    }

    /// # Safety
    /// Must ensure there's next one.
    #[inline]
    unsafe fn physical_next(&mut self) -> *mut Self {
        (self as *mut _ as *mut u8).add(self.size()).cast()
    }

    /// # Safety
    /// Must ensure `self` is unused.
    #[inline]
    unsafe fn set_logical_next(&mut self, next: *const Self) {
        debug_assert!(!self.is_used());
        *self.payload.as_mut_ptr().cast::<*const Block>().add(1) = next;
    }

    /// Set this as used and split this into 2 blocks
    /// if `new_payload_size >= size_of::<*mut Block>() * 2`.
    ///
    /// Returns the latter free block if there's one.
    unsafe fn alloc(&mut self, size: usize) -> *mut Self {
        let prev = self.logical_prev();
        let next = self.logical_next();
        let new_payload_size = self
            .payload_size()
            .saturating_sub(size + BlockMetaInfo::SIZE * 2);
        if new_payload_size < size_of::<*mut Block>() * 2 {
            if let Some(prev) = prev.as_mut() {
                prev.set_logical_next(next);
            }
            if let Some(next) = next.as_mut() {
                next.set_logical_prev(prev);
            }
            self.set_used(true);
            null_mut()
        } else {
            let free_block = Block::new_free_block(
                NonNull::new_unchecked(
                    (self as *mut _ as *mut u8).add(size + BlockMetaInfo::SIZE * 2),
                )
                .cast(),
                new_payload_size,
                prev,
                next,
            );
            if let Some(prev) = prev.as_mut() {
                prev.set_logical_next(free_block.as_ptr());
            }
            if let Some(next) = next.as_mut() {
                next.set_logical_prev(free_block.as_ptr());
            }
            self.set_payload_size(size);
            self.set_used(true);
            free_block.as_ptr()
        }
    }

    unsafe fn unsplit(
        &mut self,
        mut right: NonNull<Block>,
        set_logical_prev_next_from_right: bool,
    ) {
        if set_logical_prev_next_from_right {
            let prev = right.as_mut().logical_prev();
            let next = right.as_mut().logical_next();
            self.set_logical_prev(prev);
            self.set_logical_next(next);
            if let Some(prev) = prev.as_mut() {
                prev.set_logical_next(self as *mut _);
            }
            if let Some(next) = next.as_mut() {
                next.set_logical_prev(self as *mut _);
            }
        }
        let new_payload_size = self.payload_size() + right.as_ref().size();
        right.as_mut().set_payload_size(new_payload_size);
        self.set_payload_size(new_payload_size);
    }
}

/// Heap allocator using explicit free list.
pub struct HeapAlloc {
    heap: *mut u8,
    head_block: *mut Block,

    allocated_user: usize,
    allocated_real: usize,
    total: usize,
}

unsafe impl Send for HeapAlloc {}

impl HeapAlloc {
    /// Create an empty heap.
    pub const fn new() -> Self {
        HeapAlloc {
            heap: null_mut(),
            head_block: null_mut(),
            allocated_user: 0,
            allocated_real: 0,
            total: 0,
        }
    }

    /// Init heap with a range of memory [start, end).
    /// # Safety
    pub unsafe fn init(&mut self, start: NonNull<u8>, size: usize) {
        self.heap = start.as_ptr();
        let payload_size = size - BlockMetaInfo::SIZE * 2;
        let block = Block::new_free_block(start.cast(), payload_size, null(), null());
        self.head_block = block.as_ptr();
        self.total = size;
    }

    #[inline]
    pub fn heap(&self) -> Range<usize> {
        let start = self.heap as usize;
        start..start + self.total
    }

    /// The number of bytes that user requested
    #[inline]
    pub fn stats_alloc_user(&self) -> usize {
        self.allocated_user
    }

    /// The number of bytes that actually allocated
    #[inline]
    pub fn stats_alloc_real(&self) -> usize {
        self.allocated_real
    }

    /// The total number of bytes in the heap
    #[inline]
    pub fn stats_total_bytes(&self) -> usize {
        self.total
    }

    /// Allocate a range of memory from the heap satisfying `layout` requirements.
    pub fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = max(
            if layout.size() % 2 == 0 {
                layout.size()
            } else {
                layout.size() + 1
            },
            max(layout.align(), size_of::<*mut Block>() * 2),
        );
        self.block_iter_mut()
            .find(|block| unsafe { block.as_ref().payload_size() >= size })
            .map(|mut block| unsafe {
                let new_free_block = block.as_mut().alloc(size);
                if block.as_ptr() == self.head_block {
                    self.head_block = new_free_block;
                }
                self.allocated_real += block.as_ref().size();
                self.allocated_user += block.as_ref().payload_size();
                block.as_mut().payload_mut_ptr()
            })
            .unwrap_or(null_mut())
    }

    /// Deallocate a range of memory from the heap
    /// # Safety
    /// `ptr` must be valid.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, _layout: Layout) {
        let mut block = Block::from_payload(ptr);
        block.as_mut().set_used(false);
        self.allocated_real -= block.as_ref().size();
        self.allocated_user -= block.as_ref().payload_size();

        if self.head_block.is_null() {
            self.head_block = block.as_ptr();
            return;
        }
        if block.as_ptr() != self.heap.cast() {
            let mut physical_prev = NonNull::new_unchecked(block.as_mut().physical_prev());
            if !physical_prev.as_ref().is_used() {
                physical_prev.as_mut().unsplit(block, false);
                block = physical_prev;
            }
        }
        if block.as_ptr().cast::<u8>().add(block.as_ref().size()) < self.heap.add(self.total) {
            let physical_next = NonNull::new_unchecked(block.as_mut().physical_next());
            if !physical_next.as_ref().is_used() {
                block.as_mut().unsplit(physical_next, true);
            }
        }
    }

    fn block_iter_mut(&mut self) -> BlockIterMut {
        BlockIterMut {
            curr: self.head_block,
            _lifetime: Default::default(),
        }
    }
}

/// A mutable iterator over the linked list.
struct BlockIterMut<'a> {
    curr: *mut Block,
    _lifetime: PhantomData<&'a mut Block>,
}

impl<'a> Iterator for BlockIterMut<'a> {
    type Item = NonNull<Block>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let next = self.curr.as_mut()?.logical_next();
            let curr = self.curr;
            self.curr = next;
            Some(NonNull::new_unchecked(curr))
        }
    }
}

/// A locked version of [`HeapAlloc`]
pub struct LockedHeapAlloc(Mutex<HeapAlloc>);

impl LockedHeapAlloc {
    /// Creates an empty heap
    pub const fn new() -> Self {
        LockedHeapAlloc(Mutex::new(HeapAlloc::new()))
    }
}

impl Deref for LockedHeapAlloc {
    type Target = Mutex<HeapAlloc>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

unsafe impl GlobalAlloc for LockedHeapAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.lock().alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(ptr) = NonNull::new(ptr) {
            self.lock().dealloc(ptr, layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_meta_info() {
        let mut meta_info = BlockMetaInfo::new(128, true);
        assert!(meta_info.is_used());
        assert_eq!(meta_info.payload_size(), 128);
        meta_info.set_used(false);
        assert!(!meta_info.is_used());
        meta_info.set_payload_size(1024);
        assert_eq!(meta_info.payload_size(), 1024);
    }

    #[test]
    fn block() {
        unsafe {
            let mut space = [0u8; 64];
            let mut block1_ptr = Block::new_free_block(
                NonNull::new_unchecked(space.as_mut_ptr()).cast(),
                48,
                null(),
                null(),
            );
            let block1 = block1_ptr.as_mut();

            assert_eq!(block1.payload_size(), 48);
            assert_eq!(
                block1.size(),
                block1.payload_size() + BlockMetaInfo::SIZE * 2
            );
            assert!(block1.logical_prev().is_null());
            assert!(block1.logical_next().is_null());
            let prev = NonNull::dangling();
            block1.set_logical_prev(prev.as_ptr());
            assert_eq!(block1.logical_prev(), prev.as_ptr());
            block1.set_logical_prev(null());
            let next = NonNull::dangling();
            block1.set_logical_next(next.as_ptr());
            assert_eq!(block1.logical_next(), next.as_ptr());
            block1.set_logical_next(null());

            assert!(block1.alloc(42).is_null()); // no split
            assert!(block1.is_used());
            assert_eq!(block1.payload_size(), 48);
            block1.set_used(false);

            let block2_ptr = block1.alloc(16);
            assert!(!block2_ptr.is_null());
            let block2 = block2_ptr.as_mut().unwrap();
            dbg!(block1 as *mut _);
            assert_eq!(block1.physical_next(), block2 as *mut _);
            assert_eq!(block2.physical_prev(), block1 as *mut _);
            assert!(block1.is_used());
            assert!(!block2.is_used());
            assert_eq!(block1.payload_size(), 16);
            assert_eq!(block2.payload_size(), 16);

            block1.unsplit(NonNull::new_unchecked(block2_ptr), false);
            assert_eq!(block1.payload_size(), 48);
        }
    }

    #[test]
    fn heap_allocator() {
        unsafe {
            let mut space = [0u8; 1024];
            let mut allocator = HeapAlloc::new();
            allocator.init(NonNull::new_unchecked(space.as_mut_ptr()), 1024);
            assert_eq!(allocator.stats_total_bytes(), 1024);
            let layout = Layout::new::<usize>();
            let data = allocator.alloc(layout);
            assert!(!data.is_null());
            assert_eq!(allocator.stats_alloc_user(), layout.size());
            assert_eq!(
                allocator.stats_alloc_real(),
                layout.size() + BlockMetaInfo::SIZE * 2
            );
            allocator.dealloc(NonNull::new_unchecked(data), layout);
            assert_eq!(allocator.stats_alloc_user(), 0);
            assert_eq!(allocator.stats_alloc_real(), 0);

            // TODO: more test cases
        }
    }
}
