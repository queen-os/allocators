use core::fmt;

#[inline]
const fn left_child(i: usize) -> usize {
    i * 2 + 1
}

#[inline]
const fn right_child(i: usize) -> usize {
    i * 2 + 2
}

#[inline]
const fn parent(i: usize) -> usize {
    (i + 1) / 2 - 1
}

const NODE_ALLOCATED: i8 = -1;

#[derive(Debug)]
pub struct FrameAlloc<'a> {
    frames: usize,
    allocated: usize,
    in_used: usize,
    tree: &'a mut [i8],
}

impl<'a> FrameAlloc<'a> {
    pub fn new(tree: &'a mut [i8]) -> Self {
        let frames = if (tree.len() + 1).is_power_of_two() {
            (tree.len() + 1) / 2
        } else {
            tree.len() / 2
        };
        let nodes = frames * 2 - 1;
        let tree = &mut tree[..nodes];

        let mut exponent = frames.log2() as i8 + 1;
        for (i, node) in tree.iter_mut().enumerate() {
            if (i + 1).is_power_of_two() {
                exponent -= 1;
            }
            *node = exponent;
        }

        Self {
            frames,
            allocated: 0,
            in_used: 0,
            tree,
        }
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.frames
    }

    #[inline]
    pub fn allocated_frames(&self) -> usize {
        self.allocated
    }

    #[inline]
    pub fn in_used_frames(&self) -> usize {
        self.in_used
    }

    #[inline]
    pub fn available_frames(&self) -> usize {
        self.frames - self.allocated
    }

    pub fn alloc(&mut self, count: usize) -> Option<usize> {
        let allocating_frames = count.next_power_of_two();
        let allocating_exponent = allocating_frames.log2() as i8;

        if self.node(0) < allocating_exponent {
            return None;
        }

        let mut i = 0;
        let mut node_exponent = self.frames.log2() as i8;

        while node_exponent != allocating_exponent {
            i = if self.node(left_child(i)) >= allocating_exponent {
                left_child(i)
            } else {
                right_child(i)
            };
            node_exponent -= 1;
        }

        *self.node_mut(i) = NODE_ALLOCATED;
        let offset = (i + 1) * allocating_frames - self.frames;

        while i != 0 {
            i = parent(i);

            let max_child = self.node(left_child(i)).max(self.node(right_child(i)));
            if max_child == core::mem::replace(self.node_mut(i), max_child) {
                break;
            }
        }

        self.allocated += allocating_frames;
        self.in_used += count;

        Some(offset)
    }

    pub fn alloc_at(&mut self, offset: usize, count: usize) {
        let allocating_frames = count.next_power_of_two();
        debug_assert!(offset.is_power_of_two());
        debug_assert!(offset + allocating_frames <= self.frames);
        debug_assert_eq!(offset % allocating_frames, 0);

        let mut i = (offset + self.frames) / allocating_frames - 1;

        *self.node_mut(i) = NODE_ALLOCATED;

        while i != 0 {
            i = parent(i);

            let max_child = self.node(left_child(i)).max(self.node(right_child(i)));
            if max_child == core::mem::replace(self.node_mut(i), max_child) {
                break;
            }
        }
    }

    pub fn free(&mut self, offset: usize, count: usize) {
        let freeing_frames = count.next_power_of_two();
        debug_assert!(offset < self.frames);
        debug_assert!(freeing_frames <= self.frames);

        let mut exponent = freeing_frames.log2() as i8;
        let mut i = (offset + self.frames) / freeing_frames - 1;

        *self.node_mut(i) = exponent;

        while i != 0 {
            i = parent(i);
            exponent += 1;

            let left_max = self.node(left_child(i));
            let right_max = self.node(right_child(i));

            if left_max == right_max && left_max == exponent - 1 {
                *self.node_mut(i) = exponent;
            } else {
                let max_child = left_max.max(right_max);
                if max_child == core::mem::replace(self.node_mut(i), max_child) {
                    break;
                }
            }
        }

        self.allocated -= freeing_frames;
        self.in_used -= count;
    }

    #[inline]
    fn node(&self, i: usize) -> i8 {
        #[cfg(debug_assertions)]
        {
            self.tree[i]
        }
        #[cfg(not(debug_assertions))]
        unsafe {
            *self.tree.get_unchecked(i)
        }
    }

    #[inline]
    fn node_mut(&mut self, i: usize) -> &mut i8 {
        #[cfg(debug_assertions)]
        {
            &mut self.tree[i]
        }
        #[cfg(not(debug_assertions))]
        unsafe {
            self.tree.get_unchecked_mut(i)
        }
    }
}

impl<'a> fmt::Display for FrameAlloc<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn display(
            f: &mut fmt::Formatter<'_>,
            tree: &[i8],
            i: usize,
            exponent: u32,
        ) -> fmt::Result {
            if tree[i] == NODE_ALLOCATED {
                for _ in 0..2usize.pow(exponent) {
                    write!(f, "*")?;
                }
            } else if exponent == 0 {
                write!(f, "_")?;
            } else {
                display(f, tree, left_child(i), exponent - 1)?;
                display(f, tree, right_child(i), exponent - 1)?;
            }

            Ok(())
        }

        write!(f, "[")?;
        display(f, self.tree, 0, self.frames.log2())?;
        write!(f, "]")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let mut buf = [0i8; 64];

        {
            let buddy = FrameAlloc::new(&mut buf);
            assert_eq!(buddy.frames(), 32);
            assert_eq!(buddy.allocated_frames(), 0);
            assert_eq!(buddy.available_frames(), 32);
            assert_eq!(buddy.in_used_frames(), 0);
        }

        assert_eq!(FrameAlloc::new(&mut buf[..63]).frames(), 32);

        assert_eq!(FrameAlloc::new(&mut buf[..33]).frames(), 16);
    }

    #[test]
    fn test_alloc_free() {
        let mut buf = [0i8; 64];
        let mut buddy = FrameAlloc::new(&mut buf);

        assert_eq!(buddy.frames(), 32);

        assert_eq!(buddy.alloc(5), Some(0));
        assert_eq!(buddy.allocated_frames(), 8);
        assert_eq!(buddy.in_used_frames(), 5);
        assert_eq!(buddy.available_frames(), 24);

        assert_eq!(buddy.alloc(1), Some(8));

        buddy.free(0, 5);
        buddy.free(8, 1);

        assert_eq!(buddy.allocated_frames(), 0);
        assert_eq!(buddy.in_used_frames(), 0);
        assert_eq!(buddy.allocated_frames(), 0);

        assert!(buddy.alloc(15).is_some());
        assert!(buddy.alloc(15).is_some());

        assert_eq!(buddy.allocated_frames(), 32);

        assert_eq!(buddy.alloc(1), None);
        assert_eq!(buddy.alloc(1), None);
    }

    #[test]
    fn test_alloc_at() {
        let mut buf = [0i8; 64];
        let mut buddy = FrameAlloc::new(&mut buf);

        assert_eq!(buddy.frames(), 32);

        buddy.alloc_at(8, 8);

        let offsets = core::iter::repeat_with(|| buddy.alloc(8))
            .take_while(Option::is_some)
            .flatten()
            .collect::<Vec<_>>();

        assert_eq!(offsets.len(), 3);
        assert!(!offsets.contains(&8));
    }
}
