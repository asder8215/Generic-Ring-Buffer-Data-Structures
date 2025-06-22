use generic_ringbuffers::LFShardedMultiThreadedRingBuffer;
use std::sync::Arc;
use tokio::sync::Barrier;

#[tokio::test]
async fn test_counter() {
    const MAX_ITEMS: usize = 100;
    const MAX_SHARDS: usize = 10;
    const MAX_THREADS: usize = 5;
    let mtrb: Arc<LFShardedMultiThreadedRingBuffer<usize>> =
        Arc::new(LFShardedMultiThreadedRingBuffer::new(MAX_ITEMS, MAX_SHARDS));
    let mut threads = Vec::with_capacity(MAX_THREADS.try_into().unwrap());

    for _ in 0..MAX_THREADS {
        let mtrb = Arc::clone(&mtrb);
        let handler: tokio::task::JoinHandle<usize> = tokio::spawn(async move {
            let mut counter: usize = 0;
            loop {
                let item: Option<usize> = mtrb.dequeue().await;
                match item {
                    Some(_) => counter += 1,
                    None => break,
                }
            }
            counter
        });
        threads.push(handler);
    }

    for _ in 0..2 * MAX_ITEMS {
        mtrb.enqueue(20).await;
    }

    for _ in 0..MAX_THREADS {
        mtrb.poison_deq().await;
    }

    let mut items_taken: usize = 0;
    while let Some(curr_thread) = threads.pop() {
        items_taken += curr_thread.await.unwrap();
    }

    assert_eq!(200, items_taken);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn benchmark_sharded_buffer() {
    let max_items: usize = 1000000;
    const MAX_SHARDS: usize = 10;
    const MAX_THREADS: usize = 8;

    let smtrb: Arc<LFShardedMultiThreadedRingBuffer<usize>> =
        Arc::new(LFShardedMultiThreadedRingBuffer::new(max_items, MAX_SHARDS));
    let barrier = Arc::new(Barrier::new(MAX_THREADS * 2));

    let mut deq_threads = Vec::with_capacity(MAX_THREADS);
    let mut enq_threads = Vec::with_capacity(MAX_THREADS);

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

    let mut items_taken: usize = 0;
    while let Some(curr_thread) = deq_threads.pop() {
        items_taken += curr_thread.await.unwrap();
    }
    assert_eq!(max_items * MAX_THREADS, items_taken);
}

// #[test]
// fn run_benchmark_test() {
//     let rt = tokio::runtime::Builder::new_multi_thread()
//         .worker_threads(16)
//         .enable_all()
//         .build()
//         .unwrap();

//     rt.block_on(async {
//         benchmark_sharded_buffer().await;
//     })
// }
