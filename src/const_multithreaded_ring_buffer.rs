use std::fmt::Debug;
use std::sync::{Condvar, Mutex};

/// A ring (circular) buffer struct that can only be used in a *multi-threaded environment*,
/// using a stack-allocated [Array][https://doc.rust-lang.org/std/primitive.array.html] under the hood.
/// See the [Wikipedia article](https://en.wikipedia.org/wiki/Circular_buffer) for more info.
#[derive(Debug)]
pub struct ConstMultiThreadedRingBuffer<T, const CAPACITY: usize> {
    num_jobs: (Mutex<usize>, Condvar),
    inner_rb: Mutex<InnerRingBuffer<T, CAPACITY>>,
}

/// An inner ring buffer to contain the items, enqueue, and dequeue index for ConstMultiThreadedRingBuffer struct
#[derive(Debug)]
struct InnerRingBuffer<T, const CAPACITY: usize> {
    items: [Option<T>; CAPACITY],
    enqueue_index: usize,
    dequeue_index: usize,
}

/// Implements the InnerRingBuffer functions
impl<T, const CAPACITY: usize> InnerRingBuffer<T, CAPACITY> {
    /// Instantiates the InnerRingBuffer
    const fn new() -> Self {
        InnerRingBuffer {
            // How to initialize a generic array of options with None (without needing to iterate hence making it O(1) init)
            // https://stackoverflow.com/questions/28656387/initialize-a-large-fixed-size-array-with-non-copy-types
            items: [const { None }; CAPACITY],
            enqueue_index: 0,
            dequeue_index: 0,
        }
    }
}
impl<T, const CAPACITY: usize> ConstMultiThreadedRingBuffer<T, CAPACITY> {
    /// Instantiates the ConstMultiThreadedRingBuffer.
    ///
    /// Time Complexity: O(1), Space complexity: O(N)
    pub const fn new() -> Self {
        ConstMultiThreadedRingBuffer {
            num_jobs: (Mutex::new(0), Condvar::new()),
            inner_rb: Mutex::new(InnerRingBuffer::new()),
        }
    }

    /// Helper function to add an Option item to the RingBuffer
    /// This is necessary so that the ring buffer can be poisoned with None values
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    /// Space complexity: O(1)
    async fn enqueue_item(&self, item: Option<T>) {
        // Locks to read how many jobs are in the ring buffer
        let (num_jobs, cvar) = &self.num_jobs;
        let mut num_jobs = num_jobs.lock().unwrap();

        // If ring buffer is at capacity, block until an item is dequeued off the ring buffer
        while *num_jobs == CAPACITY {
            num_jobs = cvar.wait(num_jobs).unwrap();
        }

        // Locks to read the current enqueue index in the ring buffer and write it to the
        // items of the ring buffer at that specific enqueue index
        let mut inner = self.inner_rb.lock().unwrap();
        let enqueue_index = inner.enqueue_index;
        inner.items[enqueue_index] = item;
        *num_jobs += 1;

        // This enables the enqueue index to remain within the bounds of the
        // array
        inner.enqueue_index = (inner.enqueue_index + 1) % CAPACITY;

        // Notifies a CondVar to inform that there is a job available
        cvar.notify_one();
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
    /// Space complexity: O(1)
    pub async fn dequeue(&self) -> Option<T> {
        // Locks to read how many jobs are in the ring buffer
        let (num_jobs, cvar) = &self.num_jobs;
        let mut num_jobs = num_jobs.lock().unwrap();

        // If ring buffer is empty, block until an item is enqueued on the ring buffer
        while *num_jobs == 0 {
            num_jobs = cvar.wait(num_jobs).unwrap();
        }

        // Locks to read the current dequeue index in the ring buffer and takes the
        // item of the ring buffer at that specific enqueue index (replaces it with None
        // in exchange)
        let mut inner = self.inner_rb.lock().unwrap();
        let dequeue_index = inner.dequeue_index;
        let item = inner.items[dequeue_index].take();
        *num_jobs -= 1;

        // This enables the dequeue index to remain within the bounds of the
        // array
        inner.dequeue_index = (inner.dequeue_index + 1) % CAPACITY;

        // Notifies a CondVar to inform that a job can be enqueued
        cvar.notify_one();

        // Returns dequeued item
        item
    }

    /// Poisons the RingBuffer, preventing any more items from being **enqueued**.
    ///
    /// Time Complexity: O(N) if not blocked (arbitrary time if it is),
    /// Space complexity: O(1)
    pub async fn poison(&self) {
        for _ in 0..CAPACITY {
            self.enqueue_item(None).await;
        }
    }

    /// If the RingBuffer is [poisoned][Self::poison] or is at capacity,
    /// this method will allow the RingBuffer
    /// to be used again and resets it to an empty state.
    ///
    /// Time Complexity: O(1), Space complexity: O(1)
    pub fn clear_poison(&self) {
        let mut num_jobs = self.num_jobs.0.lock().unwrap();
        if *num_jobs == CAPACITY {
            *self.inner_rb.lock().unwrap() = InnerRingBuffer::new();
            *num_jobs = 0;
        } else {
            println!("Ring buffer is not poisoned or it is empty");
        }
    }

    /// Clears the MultiThreadedRingBuffer back to an empty state.
    /// 
    /// To clear the RingBuffer *only* when it is *poisoned*, see [Self::clear_poison].
    ///
    /// Time Complexity: O(1), Space complexity: O(1)
    pub async fn clear(&self) {
        let mut num_jobs = self.num_jobs.0.lock().unwrap();
        *num_jobs = 0;
        *self.inner_rb.lock().unwrap() = InnerRingBuffer::new();
    }
}

impl<T, const CAPACITY: usize> Default for ConstMultiThreadedRingBuffer<T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
