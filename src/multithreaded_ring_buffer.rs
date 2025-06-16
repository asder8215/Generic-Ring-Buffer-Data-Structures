use std::fmt::Debug;
use tokio::sync::{Mutex, Notify};

/// A ring (circular) buffer struct that can only be used in a *multi-threaded environment*,
/// using a [BoxedSlice] under the hood.
/// See the [Wikipedia article](https://en.wikipedia.org/wiki/Circular_buffer) for more info.
#[derive(Debug)]
pub struct MultiThreadedRingBuffer<T> {
    notify: Notify,
    num_jobs: Mutex<usize>,
    capacity: usize,
    inner_rb: Mutex<InnerRingBuffer<T>>,
}

// An inner ring buffer to contain the items, enqueue, and dequeue index for MultiThreadedRingBuffer struct
#[derive(Debug, Default, Clone)]
struct InnerRingBuffer<T> {
    items: Box<[Option<T>]>,
    enqueue_index: usize,
    dequeue_index: usize,
}

/// Implements the InnerRingBuffer functions
impl<T: Debug> InnerRingBuffer<T> {
    /// Instantiates the InnerRingBuffer
    fn new(capacity: usize) -> Self {
        InnerRingBuffer {
            items: {
                let mut vec = Vec::with_capacity(capacity);
                for _i in 0..capacity {
                    vec.push(None);
                }
                vec.into_boxed_slice()
            },
            enqueue_index: 0,
            dequeue_index: 0,
        }
    }
}
impl<T: Debug> MultiThreadedRingBuffer<T> {
    /// Instantiates the MultiThreadedRingBuffer.
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(N)
    pub fn new(capacity: usize) -> Self {
        Self {
            notify: Notify::new(),
            capacity,
            num_jobs: Mutex::new(0),
            inner_rb: Mutex::new(InnerRingBuffer::new(capacity)),
        }
    }

    /// Helper function to add an Option item to the RingBuffer
    /// This is necessary so that the ring buffer can be poisoned with None values
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is)
    ///
    /// Space Complexity: O(1)
    async fn enqueue_item(&self, item: Option<T>) {
        // Locks to read how many jobs are in the ring buffer
        let mut num_jobs = self.num_jobs.lock().await;

        // If ring buffer is at capacity, block until an item is dequeued off the ring buffer
        while *num_jobs == self.capacity {
            let notify = self.notify.notified();

            drop(num_jobs);

            notify.await;

            num_jobs = self.num_jobs.lock().await;
        }

        // Locks to read the current enqueue index & capacity in the ring buffer and write it to the
        // items of the ring buffer at that specific enqueue index
        let mut inner = self.inner_rb.lock().await;
        let enqueue_index = inner.enqueue_index;
        inner.items[enqueue_index] = item;

        *num_jobs += 1;

        // This enables the enqueue index to remain within the bounds of the
        // array
        inner.enqueue_index = (inner.enqueue_index + 1) % self.capacity;

        // Notifies so that Dequerer knows there is a job available
        self.notify.notify_one();
    }

    /// Adds an item of type T to the RingBuffer, *blocking* the thread until there is space to add the item.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    /// Space complexity: O(1)
    pub async fn enqueue(&self, item: T) {
        self.enqueue_item(Some(item)).await;
    }

    /// Retrieves an item of type T from the RingBuffer if an item exists in the buffer.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    ///
    /// Space Complexity: O(1)
    pub async fn dequeue(&self) -> Option<T> {
        // Locks to read how many jobs are in the ring buffer
        let mut num_jobs = self.num_jobs.lock().await;

        // If ring buffer is empty, block until an item is enqueued on the ring buffer
        while *num_jobs == 0 {
            let notify = self.notify.notified();

            drop(num_jobs);

            notify.await;

            num_jobs = self.num_jobs.lock().await;
        }

        // Locks to read the current dequeue index & capacity in the ring buffer and takes the
        // item of the ring buffer at that specific enqueue index (replaces it with None
        // in exchange)
        let mut inner = self.inner_rb.lock().await;
        let dequeue_index = inner.dequeue_index;
        let item = inner.items[dequeue_index].take();
        *num_jobs -= 1;

        // This enables the dequeue index to remain within the bounds of the
        // array
        inner.dequeue_index = (inner.dequeue_index + 1) % self.capacity;

        // Notifies that a job can be enqueued
        self.notify.notify_one();

        // Returns dequeued item
        item
    }

    /// Poisons the RingBuffer, preventing any more items from being **enqueued**.
    ///
    /// Time Complexity: O(N) if not blocked (arbitrary time if it is)
    ///
    /// Space Complexity: O(1)
    pub async fn poison(&self) {
        for _ in 0..self.capacity {
            self.enqueue_item(None).await;
        }
    }

    /// If the RingBuffer is [poisoned][Self::poison] or is at capacity,
    /// this method will allow the RingBuffer
    /// to be used again and resets it to an empty state.
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn clear_poison(&mut self) {
        let mut num_jobs = self.num_jobs.lock().await;
        let mut inner = self.inner_rb.lock().await;
        if *num_jobs == self.capacity {
            *inner = InnerRingBuffer::new(self.capacity);
            *num_jobs = 0;
        } else {
            println!("Ring buffer is not poisoned or it is empty");
        }
    }

    /// Clears the MultiThreadedRingBuffer back to an empty state.
    ///
    /// To clear the RingBuffer *only* when it is *poisoned*, see [Self::clear_poison].
    ///
    /// Time Complexity: O(1)
    ///
    /// Space complexity: O(1)
    pub async fn clear(&self) {
        *self.num_jobs.lock().await = 0;
        *self.inner_rb.lock().await = InnerRingBuffer::new(self.capacity);
    }

    /// Checks whether the MultiThreadedRingBuffer is empty or not
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn is_empty(&self) -> bool {
        return *self.num_jobs.lock().await == 0;
    }

    /// Checks whether the MultiThreadedRingBuffer is full or not
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn is_full(&self) -> bool {
        return *self.num_jobs.lock().await == self.capacity;
    }

    /// Checks the next enqueue index within the MultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_enqueue_index(&self) -> usize {
        let _ = self.num_jobs.lock().await;
        let inner = self.inner_rb.lock().await;
        inner.enqueue_index
    }

    /// Checks the next dequeue index within the MultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_dequeue_index(&self) -> usize {
        let _ = self.num_jobs.lock().await;
        let inner = self.inner_rb.lock().await;
        inner.dequeue_index
    }

    /// Returns a clone of the item within the MultiThreadedRingBuffer
    ///
    /// The T object inside the ring buffer *must* implement the Clone trait
    ///
    /// Time Complexity: O(T_t)
    ///
    /// Space Complexity: O(T_s)
    ///
    /// Where O(T_t) and O(T_s) is the time and space complexity required
    /// to clone the internals of the T object itself
    pub async fn get(&self, index: usize) -> Option<T>
    where
        T: Clone,
    {
        let _ = self.num_jobs.lock().await;
        let inner = self.inner_rb.lock().await;
        inner.items[index].clone()
    }

    /// Returns a clone of the MultiThreadedRingBuffer in its current state
    ///
    /// The T object inside the ring buffer *must* implement the Clone trait
    ///
    /// Time Complexity: O(N * O(T_t))
    ///
    /// Space Complexity: O(N * O(T_s))
    ///
    /// Where O(T_t) and O(T_s) is the time and space complexity required
    /// to clone the internals of the T object itself
    pub async fn rb_items(&self) -> Box<[Option<T>]>
    where
        T: Clone,
    {
        let _ = self.num_jobs.lock().await;
        let inner = self.inner_rb.lock().await;
        inner.items.clone()
    }

    /// Print out the content inside the MultiThreadedRingBuffer
    ///
    /// Time Complexity: O(N)
    ///
    /// Space Complexity: O(1)
    pub async fn print_buffer(&self)
    where
        T: Debug,
    {
        let _ = self.num_jobs.lock().await;
        let inner = self.inner_rb.lock().await;
        print!("[");
        for item in &inner.items {
            print!("{:?}, ", item);
        }
        print!("]");
    }
}
