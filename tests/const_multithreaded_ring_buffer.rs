use generic_ringbuffers::ConstMultiThreadedRingBuffer;
use std::sync::Arc;
use std::thread;

#[test]
fn test_counter() {
    const MAX_ITEMS: usize = 100;
    const MAX_THREADS: usize = 5;
    let mtrb: Arc<ConstMultiThreadedRingBuffer<usize, 100>> =
        Arc::new(ConstMultiThreadedRingBuffer::new());
    let mut threads = Vec::with_capacity(MAX_THREADS.try_into().unwrap());

    for _ in 0..MAX_THREADS {
        let mtrb = Arc::clone(&mtrb);
        let handler: thread::JoinHandle<usize> = thread::spawn(move || {
            let mut counter: usize = 0;
            loop {
                let item: Option<usize> = mtrb.dequeue();
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

    for _ in 0..2 * MAX_ITEMS {
        println!("Enqueued item!");
        mtrb.enqueue(20);
    }

    mtrb.poison();

    let mut items_taken: usize = 0;
    while let Some(curr_thread) = threads.pop() {
        items_taken += curr_thread.join().expect("Could not join threads");
    }

    assert_eq!(2 * MAX_ITEMS, items_taken);
}
