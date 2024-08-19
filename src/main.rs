mod const_multithreaded_ring_buffer;
mod multithreaded_ring_buffer;
mod mtrb_test;

use multithreaded_ring_buffer::MultiThreadedRingBuffer;
use const_multithreaded_ring_buffer::ConstMultiThreadedRingBuffer;


async fn dequeue_items(mtrb: &MultiThreadedRingBuffer<usize>) -> usize{
    let mut counter: usize = 0;
    loop {
        println!("Dequeued item!");
        let item: Option<usize> = mtrb.dequeue().await;
        match item {
            Some(item) => {
                counter += 1;
            }
            None => {
                break;
            }
        }
    }
    println!("Finished Working!");
    counter
}

#[tokio::main]
async fn main() {
    const MAX_ITEMS: usize = 100;
    const MAX_THREADS: usize = 5;
    let mtrb: MultiThreadedRingBuffer<usize> = MultiThreadedRingBuffer::new(MAX_ITEMS);
    let mut threads: Vec<tokio::task::JoinHandle<usize>> =
                Vec::with_capacity(MAX_THREADS.try_into().unwrap());

    for _ in 0..MAX_THREADS {
        let mtrb_clone = mtrb.clone();
        let handler: tokio::task::JoinHandle<usize> = tokio::spawn(async move { 
            dequeue_items(&mtrb_clone).await
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

    println!("Items taken from thread: {}", items_taken);

    println!("{:?}", items_taken == 200)

}
