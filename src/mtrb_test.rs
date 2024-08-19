
#[cfg(test)]
mod tests {
    use crate::multithreaded_ring_buffer::MultiThreadedRingBuffer;

    async fn dequeue_items(mtrb: &MultiThreadedRingBuffer<usize>) -> usize{
        let mut counter: usize = 0;
        loop {
            let item: Option<usize> = mtrb.dequeue().await;
            println!("Dequeued items!");
            match item {
                Some(item) => {
                    counter += 1;
                }
                None => {
                    break;
                }
            }
        }
        counter
    }

    #[tokio::test]
    async fn test_counter() {
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

        // assert_eq!(200, items_taken);
    }
}