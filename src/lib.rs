#![cfg_attr(not(test), no_std)]
#![feature(core_intrinsics)]

extern crate alloc;

pub mod frame;
pub mod heap;
pub mod slab;