use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use generic_ringbuffers::{
    ConstMultiThreadedRingBuffer, MultiThreadedRingBuffer, ShardedMultiThreadedRingBuffer,
};
use std::{
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::Barrier as AsyncBarrier;

const MAX_SHARDS: usize = 1000;
const MAX_THREADS: usize = 8;
const CAPACITY: usize = 1000;

fn benchmark_const_buffer<const C: usize>() {
    let cmtrb: Arc<ConstMultiThreadedRingBuffer<usize, C>> =
        Arc::new(ConstMultiThreadedRingBuffer::new());

    // barrier used to make sure all threads are operating at the same time
    let barrier = Arc::new(Barrier::new(MAX_THREADS * 2));

    let mut deq_threads = Vec::with_capacity(MAX_THREADS);
    let mut enq_threads = Vec::with_capacity(MAX_THREADS);

    // spawn deq threads
    for _ in 0..MAX_THREADS {
        let cmtrb = Arc::clone(&cmtrb);
        let barrier = Arc::clone(&barrier);
        let handler: thread::JoinHandle<usize> = thread::spawn(move || {
            barrier.wait();
            let mut counter: usize = 0;
            for _i in 0..C {
                let item: Option<usize> = cmtrb.dequeue();
                match item {
                    Some(_) => counter += 1,
                    None => break,
                }
            }
            counter
        });
        deq_threads.push(handler);
    }

    // spawn enq threads
    for _ in 0..MAX_THREADS {
        let cmtrb = Arc::clone(&cmtrb);
        let barrier = Arc::clone(&barrier);
        let handler: thread::JoinHandle<()> = thread::spawn(move || {
            barrier.wait();
            for _i in 0..C {
                cmtrb.enqueue(20);
            }
        });
        enq_threads.push(handler);
    }

    // Wait for enqueuers
    for enq in enq_threads {
        enq.join().unwrap();
    }

    // Wait for dequerers
    for deq in deq_threads {
        deq.join().unwrap();
    }
}

async fn benchmark_regular_buffer(capacity: usize) {
    let max_items: usize = capacity;

    let mtrb: Arc<MultiThreadedRingBuffer<usize>> =
        Arc::new(MultiThreadedRingBuffer::new(max_items));

    // barrier used to make sure all threads are operating at the same time
    let barrier = Arc::new(AsyncBarrier::new(MAX_THREADS * 2));

    let mut deq_threads = Vec::with_capacity(MAX_THREADS);
    let mut enq_threads = Vec::with_capacity(MAX_THREADS);

    // spawn deq threads
    for _ in 0..MAX_THREADS {
        let mtrb = Arc::clone(&mtrb);
        let barrier = Arc::clone(&barrier);
        let handler: tokio::task::JoinHandle<usize> = tokio::spawn(async move {
            barrier.wait().await;
            let mut counter: usize = 0;
            for _i in 0..max_items {
                let item: Option<usize> = mtrb.dequeue().await;
                match item {
                    Some(_) => counter += 1,
                    None => break,
                }
            }
            counter
        });
        deq_threads.push(handler);
    }

    // spawn enq threads
    for _ in 0..MAX_THREADS {
        let mtrb = Arc::clone(&mtrb);
        let barrier = Arc::clone(&barrier);
        let handler: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            barrier.wait().await;
            for _i in 0..max_items {
                mtrb.enqueue(20).await;
            }
        });
        enq_threads.push(handler);
    }

    // Wait for enqueuers
    for enq in enq_threads {
        enq.await.unwrap();
    }

    // Wait for dequerers
    for deq in deq_threads {
        deq.await.unwrap();
    }
}

async fn benchmark_sharded_buffer(capacity: usize) {
    let max_items: usize = capacity;

    let smtrb: Arc<ShardedMultiThreadedRingBuffer<usize>> =
        Arc::new(ShardedMultiThreadedRingBuffer::new(max_items, MAX_SHARDS));

    // barrier used to make sure all threads are operating at the same time
    let barrier = Arc::new(AsyncBarrier::new(MAX_THREADS * 2));

    let mut deq_threads = Vec::with_capacity(MAX_THREADS);
    let mut enq_threads = Vec::with_capacity(MAX_THREADS);

    // spawn deq threads
    for _ in 0..MAX_THREADS {
        let smtrb = Arc::clone(&smtrb);
        let barrier = Arc::clone(&barrier);
        let handler: tokio::task::JoinHandle<usize> = tokio::spawn(async move {
            barrier.wait().await;
            let mut counter: usize = 0;
            for _i in 0..max_items {
                let item = smtrb.dequeue().await;
                match item {
                    Some(_) => counter += 1,
                    None => break,
                }
            }
            counter
        });
        deq_threads.push(handler);
    }

    // spawn enq threads
    for _ in 0..MAX_THREADS {
        let smtrb = Arc::clone(&smtrb);
        let barrier = Arc::clone(&barrier);
        let handler: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            barrier.wait().await;
            for _i in 0..max_items {
                smtrb.enqueue(20).await;
            }
        });
        enq_threads.push(handler);
    }

    // Wait for enqueuers
    for enq in enq_threads {
        enq.await.unwrap();
    }

    // Wait for dequeuers
    for deq in deq_threads {
        deq.await.unwrap();
    }
}

// Benchmark regular ring buffer and sharded ring buffer code here!
// Uses tokio runtime to test async code
// Sources where I learned about cargo benchmarking:
// https://www.youtube.com/watch?app=desktop&v=w4pCcW8uSNs
// https://bheisler.github.io/criterion.rs/book/getting_started.html
// https://bheisler.github.io/criterion.rs/book/user_guide/benchmarking_async.html
fn rb_benchmark(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // .max_blocking_threads(MAX_THREADS * 2)
        .enable_all()
        .worker_threads(MAX_THREADS * 2)
        .build()
        .unwrap();

    c.bench_with_input(
        BenchmarkId::new("const_buffer", CAPACITY),
        &CAPACITY,
        |b, &_s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.iter_custom(move |iters| {
                let mut total = Duration::ZERO;
                for _i in 0..iters {
                    let start = Instant::now();
                    benchmark_const_buffer::<CAPACITY>();
                    let end = Instant::now();
                    total += end - start;
                }

                total
            });
        },
    );

    c.bench_with_input(
        BenchmarkId::new("regular_buffer", CAPACITY),
        &CAPACITY,
        |b, &s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.to_async(&runtime).iter_custom(|iters| async move {
                let mut total = Duration::ZERO;
                for _i in 0..iters {
                    let start = Instant::now();
                    benchmark_regular_buffer(s).await;
                    let end = Instant::now();
                    total += end - start;
                }

                total
            });
        },
    );

    c.bench_with_input(
        BenchmarkId::new("regular_buffer", CAPACITY),
        &CAPACITY,
        |b, &s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.to_async(&runtime).iter_custom(|iters| async move {
                let mut total = Duration::ZERO;
                for _i in 0..iters {
                    let start = Instant::now();
                    benchmark_regular_buffer(s).await;
                    let end = Instant::now();
                    total += end - start;
                }

                total
            });
        },
    );

    c.bench_with_input(
        BenchmarkId::new("sharded_buffer", CAPACITY),
        &CAPACITY,
        |b, &s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.to_async(&runtime).iter_custom(|iters| async move {
                let mut total = Duration::ZERO;
                for _i in 0..iters {
                    let start = Instant::now();
                    benchmark_sharded_buffer(s).await;
                    let end = Instant::now();
                    total += end - start;
                }

                total
            });
        },
    );
}

criterion_group!(benches, rb_benchmark);
criterion_main!(benches);
