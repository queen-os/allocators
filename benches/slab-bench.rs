use allocators::slab::{
    std_impl::{MultiThreadedBench, StdMemCacheUtils},
    MemCache,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use spin::RwLock;
use std::{sync::Arc, time::Duration};

const N_INSERTIONS: &[usize] = &[100, 300, 500, 700, 1000, 3000, 5000];

#[derive(Debug, Default, Clone, Copy)]
struct Foo {
    a: usize,
    b: usize,
}

#[inline]
fn dummy_mem_cache(cpu_count: usize) -> MemCache<StdMemCacheUtils> {
    let utils = StdMemCacheUtils::new(10240);
    MemCache::new(cpu_count, 16, 16, 32, utils)
}

fn insert_remove_local(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_remove_local");
    let g = group.measurement_time(Duration::from_secs(5));
    let pool = Arc::new(threadpool::ThreadPool::new(4));

    for objects in N_INSERTIONS {
        g.bench_with_input(
            BenchmarkId::new("my_slab", objects),
            objects,
            |b, &objects| {
                let my_slab = Arc::new(dummy_mem_cache(8));
                b.iter_custom(|iters| {
                    let mut total = Duration::from_secs(0);
                    for _ in 0..iters {
                        let bench = MultiThreadedBench::new(my_slab.clone(), pool.clone());
                        let elapsed = bench
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> =
                                        (0..100).map(|_| unsafe { slab.allocate() }).collect();
                                    for obj in v {
                                        unsafe { slab.deallocate(obj) }
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> =
                                        (0..100).map(|_| unsafe { slab.allocate() }).collect();
                                    for obj in v {
                                        unsafe { slab.deallocate(obj) }
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> =
                                        (0..100).map(|_| unsafe { slab.allocate() }).collect();
                                    for obj in v {
                                        unsafe { slab.deallocate(obj) }
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> =
                                        (0..100).map(|_| unsafe { slab.allocate() }).collect();
                                    for obj in v {
                                        unsafe { slab.deallocate(obj) }
                                    }
                                }
                            })
                            .run();
                        total += elapsed;
                    }
                    total
                })
            },
        );
        g.bench_with_input(BenchmarkId::new("heap", objects), objects, |b, &objects| {
            b.iter_custom(|iters| {
                let mut total = Duration::from_secs(0);
                for _ in 0..iters {
                    let bench = MultiThreadedBench::new(Arc::new(()), pool.clone());
                    let elapsed = bench
                        .thread(move |start, _| {
                            start.wait();
                            for _ in 0..(objects / 100 + 1) {
                                let _v: Vec<_> =
                                    (0..100).map(|_| Box::new(Foo::default())).collect();
                            }
                        })
                        .thread(move |start, _| {
                            start.wait();
                            for _ in 0..(objects / 100 + 1) {
                                let _v: Vec<_> =
                                    (0..100).map(|_| Box::new(Foo::default())).collect();
                            }
                        })
                        .thread(move |start, _| {
                            start.wait();
                            for _ in 0..(objects / 100 + 1) {
                                let _v: Vec<_> =
                                    (0..100).map(|_| Box::new(Foo::default())).collect();
                            }
                        })
                        .thread(move |start, _| {
                            start.wait();
                            for _ in 0..(objects / 100 + 1) {
                                let _v: Vec<_> =
                                    (0..100).map(|_| Box::new(Foo::default())).collect();
                            }
                        })
                        .run();
                    total += elapsed;
                }
                total
            })
        });
        let slab = Arc::new(sharded_slab::Slab::new());
        g.bench_with_input(
            BenchmarkId::new("sharded_slab", objects),
            objects,
            |b, &objects| {
                b.iter_custom(|iters| {
                    let mut total = Duration::from_secs(0);
                    for _ in 0..iters {
                        let bench = MultiThreadedBench::new(slab.clone(), pool.clone());
                        let elapsed = bench
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.insert(Foo::default()).unwrap())
                                        .collect();
                                    for i in v {
                                        slab.remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.insert(Foo::default()).unwrap())
                                        .collect();
                                    for i in v {
                                        slab.remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.insert(Foo::default()).unwrap())
                                        .collect();
                                    for i in v {
                                        slab.remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.insert(Foo::default()).unwrap())
                                        .collect();
                                    for i in v {
                                        slab.remove(i);
                                    }
                                }
                            })
                            .run();
                        total += elapsed;
                    }
                    total
                })
            },
        );
        g.bench_with_input(
            BenchmarkId::new("slab_biglock", objects),
            objects,
            |b, &objects| {
                b.iter_custom(|iters| {
                    let mut total = Duration::from_secs(0);
                    for _ in 0..iters {
                        let bench = MultiThreadedBench::new(
                            Arc::new(RwLock::new(slab::Slab::new())),
                            pool.clone(),
                        );
                        let elapsed = bench
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.write().insert(Foo::default()))
                                        .collect();
                                    for i in v {
                                        slab.write().remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.write().insert(Foo::default()))
                                        .collect();
                                    for i in v {
                                        slab.write().remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.write().insert(Foo::default()))
                                        .collect();
                                    for i in v {
                                        slab.write().remove(i);
                                    }
                                }
                            })
                            .thread(move |start, slab| {
                                start.wait();
                                for _ in 0..(objects / 100 + 1) {
                                    let v: Vec<_> = (0..100)
                                        .map(|_| slab.write().insert(Foo::default()))
                                        .collect();
                                    for i in v {
                                        slab.write().remove(i);
                                    }
                                }
                            })
                            .run();
                        total += elapsed;
                    }
                    total
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, insert_remove_local);
criterion_main!(benches);
