use allocators::slab::{
    bench::{MultiThreadedBench, ThreadedMemCacheUtils},
    MemCache,
};
use std::sync::Arc;

#[inline]
fn dummy_mem_cache(cpu_count: usize) -> MemCache<ThreadedMemCacheUtils> {
    let utils = ThreadedMemCacheUtils::new();
    MemCache::new(cpu_count, 16, 16, 32, utils)
}

fn main() {
    let my_slab = Arc::new(dummy_mem_cache(8));
    let bench = MultiThreadedBench::new(my_slab);
    let objects = 100000;
    let elapsed = bench
        .thread(move |start, slab| {
            start.wait();
            for _ in 0..(objects / 1000 + 1) {
                let v: Vec<_> = (0..1000).map(|_| unsafe { slab.allocate() }).collect();
                for obj in v {
                    unsafe { slab.deallocate(obj) }
                }
            }
        })
        .thread(move |start, slab| {
            start.wait();
            for _ in 0..(objects / 1000 + 1) {
                let v: Vec<_> = (0..1000).map(|_| unsafe { slab.allocate() }).collect();
                for obj in v {
                    unsafe { slab.deallocate(obj) }
                }
            }
        })
        .thread(move |start, slab| {
            start.wait();
            for _ in 0..(objects / 1000 + 1) {
                let v: Vec<_> = (0..1000).map(|_| unsafe { slab.allocate() }).collect();
                for obj in v {
                    unsafe { slab.deallocate(obj) }
                }
            }
        })
        .thread(move |start, slab| {
            start.wait();
            for _ in 0..(objects / 1000 + 1) {
                let v: Vec<_> = (0..1000).map(|_| unsafe { slab.allocate() }).collect();
                for obj in v {
                    unsafe { slab.deallocate(obj) }
                }
            }
        })
        .run();
    println!("{}us", elapsed.as_micros());
}
