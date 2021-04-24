use alloc::collections::BTreeSet;
use core::{
    cmp::min,
    ops::{Deref, Range},
    mem
};
use spin::Mutex;

#[inline]
fn prev_power_of_two(num: usize) -> usize {
    1 << (8 * (mem::size_of::<usize>()) - num.leading_zeros() as usize - 1)
}

/// A frame allocator using buddy system.
/// Requires a global allocator
#[derive(Debug, Default)]
pub struct FrameAlloc {
    // buddy system with max order of 32
    free_list: [BTreeSet<usize>; 32],

    // statistics
    allocated: usize,
    total: usize,
}

impl FrameAlloc {
    /// Create an empty frame allocator
    #[inline]
    pub fn new() -> Self {
        FrameAlloc::default()
    }

    /// Add a range of frame number [start, end) to the allocator
    pub fn add_frame(&mut self, start: usize, end: usize) {
        assert!(start <= end);

        let mut total = 0;
        let mut current_start = start;

        while current_start < end {
            let lowbit = if current_start > 0 {
                current_start & (!current_start + 1)
            } else {
                32
            };
            let size = min(lowbit, prev_power_of_two(end - current_start));
            total += size;

            self.free_list[size.trailing_zeros() as usize].insert(current_start);
            current_start += size;
        }

        self.total += total;
    }

    /// Add a range of frame to the allocator
    #[inline]
    pub fn insert(&mut self, range: Range<usize>) {
        self.add_frame(range.start, range.end);
    }

    /// Allocate a range of frames from the allocator, return the first frame of the allocated range
    pub fn alloc(&mut self, count: usize) -> Option<usize> {
        let size = count.next_power_of_two();
        let class = size.trailing_zeros() as usize;
        for i in class..self.free_list.len() {
            // Find the first non-empty size class
            if !self.free_list[i].is_empty() {
                // Split buffers
                for j in (class + 1..i + 1).rev() {
                    let block = *self.free_list[j].iter().next()?;
                    self.free_list[j - 1].insert(block + (1 << (j - 1)));
                    self.free_list[j - 1].insert(block);
                    self.free_list[j].remove(&block);
                }

                let result = *self.free_list[class].iter().next()?;
                self.free_list[class].remove(&result);
                self.allocated += size;
                return Some(result);
            }
        }
        None
    }

    /// Deallocate a range of frames [frame, frame+count) from the frame allocator.
    pub fn dealloc(&mut self, frame: usize, count: usize) {
        let size = count.next_power_of_two();
        let class = size.trailing_zeros() as usize;

        // Put back into free list
        self.free_list[class].insert(frame);

        // Merge free buddy lists
        let mut current_ptr = frame;
        let mut current_class = class;
        while current_class < self.free_list.len() {
            let buddy = current_ptr ^ (1 << current_class);
            if self.free_list[current_class].remove(&buddy) {
                // Free buddy found
                current_ptr = min(current_ptr, buddy);
                current_class += 1;
                self.free_list[current_class].insert(current_ptr);
            } else {
                break;
            }
        }

        self.allocated -= size;
    }
}

/// A locked version of [`FrameAlloc`]
#[derive(Debug, Default)]
pub struct LockedFrameAlloc(Mutex<FrameAlloc>);

impl LockedFrameAlloc {
    /// Creates an empty heap
    #[inline]
    pub fn new() -> Self {
        LockedFrameAlloc::default()
    }
}

impl Deref for LockedFrameAlloc {
    type Target = Mutex<FrameAlloc>;

    fn deref(&self) -> &Mutex<FrameAlloc> {
        &self.0
    }
}
