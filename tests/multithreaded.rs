use std::sync::Arc;
use generic_ringbuffers::MultiThreadedRingBuffer;

#[tokio::test]
async fn test_counter() {
    const MAX_ITEMS: usize = 100;
    const MAX_THREADS: usize = 5;
    let mtrb = Arc::new(MultiThreadedRingBuffer::new(MAX_ITEMS));
    let mut threads = Vec::with_capacity(MAX_THREADS.try_into().unwrap());

    for _ in 0..MAX_THREADS {
        let mtrb = Arc::clone(&mtrb);
        let handler: tokio::task::JoinHandle<usize> = tokio::spawn(async move { 
            let mut counter: usize = 0;
            loop {
                let item: Option<usize> = mtrb.dequeue().await;
                println!("Dequeued items!");
                match item {
                    Some(_) => counter += 1,
                    None => break,
                }
            }
            counter
        });
        threads.push(handler);
    }

    for _ in 0..2*MAX_ITEMS {
        println!("Enqueued item!");
        mtrb.enqueue(20).await;
    }

    mtrb.poison().await;

    let mut items_taken: usize = 0;
    while let Some(curr_thread) = threads.pop() {
        items_taken += curr_thread.await.unwrap();
    }

    assert_eq!(200, items_taken);
}