use std::{
    cmp,
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use thread_local::ThreadLocal;
use tokio::sync::Mutex;
use tokio::task::yield_now;

#[derive(Debug)]
enum Acquire {
    Enqueue,
    Dequeue,
}

/// A sharded ring (circular) buffer struct that can only be used in a *multi-threaded environment*,
/// using a [BoxedSlice] under the hood.
/// See the [Wikipedia article](https://en.wikipedia.org/wiki/Circular_buffer) for more info.
#[derive(Debug)]
pub struct ShardedMultiThreadedRingBuffer<T> {
    capacity: usize,
    shards: usize,
    max_capacity_per_shard: usize,
    num_jobs: AtomicUsize,
    // Each thread owns a local variable of the index it's looking
    // at within shard_jobs
    shard_ind: ThreadLocal<AtomicUsize>,
    // Used to determine which shard a thread should work on:
    // An atomic bool denoting if the shard is taken or not
    // An atomic usize val denoting if job is at capacity or not
    shard_jobs: Box<[(AtomicBool, AtomicUsize)]>,
    // Multiple InnerRingBuffer structure based on num of shards
    inner_rb: Box<[Mutex<InnerRingBuffer<T>>]>,
}

// An inner ring buffer to contain the items, enqueue, and dequeue index for ShardedMultiThreadedRingBuffer struct
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

impl<T: Debug> ShardedMultiThreadedRingBuffer<T> {
    /// Instantiates the ShardedMultiThreadedRingBuffer.
    ///
    /// Time Complexity: O(s) where s is the number of shards
    ///
    /// Space Complexity: O(N)
    pub fn new(capacity: usize, shards: usize) -> Self {
        Self {
            capacity: (capacity as f64 / shards as f64).ceil() as usize * shards,
            shards,
            // max_capacity_per_shard: cmp::max((capacity as f64 / shards as f64).ceil() as usize, 1),
            max_capacity_per_shard: cmp::max((capacity + shards - 1) / shards, 1),
            num_jobs: AtomicUsize::new(0),
            shard_ind: ThreadLocal::new(),
            shard_jobs: {
                let mut vec = Vec::with_capacity(shards);
                for _i in 0..shards {
                    vec.push((AtomicBool::new(false), AtomicUsize::new(0)));
                }
                vec.into_boxed_slice()
            },
            inner_rb: {
                let mut vec = Vec::with_capacity(shards);
                for _i in 0..shards {
                    vec.push(Mutex::new(InnerRingBuffer::new(cmp::max(
                        // (capacity as f64 / shards as f64).ceil() as usize,
                        (capacity + shards - 1) / shards,
                        1,
                    ))));
                }
                vec.into_boxed_slice()
            },
        }
    }

    /// Helper function for a thread to acquire a specific shard within
    /// self.shard_jobs for enqueuing or dequeuing purposes. It iterates
    /// in a ring buffer like manner to give each shard equal weight. tokio
    /// yield_now() function is used so that this function isn't fully occupying
    /// the CPU at all times.
    ///
    /// The time complexity of this depends on number of enquerer and
    /// dequerer threads there are; ideally, you would have similar number
    /// of enquerer and dequerer threads to use this properly.
    ///
    /// Space Complexity: O(1)
    async fn acquire_shard(&self, acquire: Acquire) -> usize {
        let cell = self.shard_ind.get_or(|| AtomicUsize::new(0));
        let mut current = cell.load(Ordering::Relaxed);

        loop {
            if match acquire {
                Acquire::Enqueue => {
                    self.shard_jobs[current].1.load(Ordering::Acquire) < self.max_capacity_per_shard
                }
                Acquire::Dequeue => self.shard_jobs[current].1.load(Ordering::Acquire) > 0,
            } && self.shard_jobs[current]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let next = (current + 1) % self.shards;
                cell.store(next, Ordering::Relaxed);
                break;
            }

            current = (current + 1) % self.shards;
            yield_now().await;
        }
        current
    }

    /// Helper function to add an Option item to the RingBuffer
    /// This is necessary so that the ring buffer can be poisoned with None values
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is)
    ///
    /// Space Complexity: O(1)
    async fn enqueue_item(&self, item: Option<T>) {
        let current = self.acquire_shard(Acquire::Enqueue).await;

        // Lock the inner ring buffer shard, enqueue the item, update the enqueue index
        let mut inner = self.inner_rb[current].lock().await;
        let enqueue_index = inner.enqueue_index;
        inner.items[enqueue_index] = item;
        inner.enqueue_index = (inner.enqueue_index + 1) % self.max_capacity_per_shard;

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        self.num_jobs.fetch_add(1, Ordering::Release);
        self.shard_jobs[current].1.fetch_add(1, Ordering::Release);
        self.shard_jobs[current].0.store(false, Ordering::Release);
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
        let current = self.acquire_shard(Acquire::Dequeue).await;

        // Lock the inner ring buffer shard, dequeue the item, update the dequeue index
        let mut inner = self.inner_rb[current].lock().await;
        let dequeue_index = inner.dequeue_index;
        let item = inner.items[dequeue_index].take();
        inner.dequeue_index = (inner.dequeue_index + 1) % self.max_capacity_per_shard;

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        self.num_jobs.fetch_sub(1, Ordering::Release);
        self.shard_jobs[current].1.fetch_sub(1, Ordering::Release);
        self.shard_jobs[current].0.store(false, Ordering::Release);

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

    // /// If the RingBuffer is [poisoned][Self::poison] or is at capacity,
    // /// this method will allow the RingBuffer
    // /// to be used again and resets it to an empty state.
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn clear_poison(&mut self) {
    //     let mut num_jobs = self.num_jobs.lock().await;
    //     // let mut inner = self.inner_rb.lock().await;
    //     if *num_jobs == self.capacity {
    //         *inner = InnerRingBuffer::new(self.capacity);
    //         *num_jobs = 0;
    //     } else {
    //         println!("Ring buffer is not poisoned or it is empty");
    //     }
    // }

    // /// Clears the MultiThreadedRingBuffer back to an empty state.
    // ///
    // /// To clear the RingBuffer *only* when it is *poisoned*, see [Self::clear_poison].
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space complexity: O(1)
    // pub async fn clear(&self) {
    //     *self.num_jobs.lock().await = 0;
    //     *self.inner_rb.lock().await = InnerRingBuffer::new(self.capacity);
    // }

    // /// Checks whether the MultiThreadedRingBuffer is empty or not
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn is_empty(&self) -> bool {
    //     return *self.num_jobs.lock().await == 0;
    // }

    // /// Checks whether the MultiThreadedRingBuffer is full or not
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn is_full(&self) -> bool {
    //     return *self.num_jobs.lock().await == self.capacity;
    // }

    // /// Checks the next enqueue index within the MultiThreadedRingBuffer
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn next_enqueue_index(&self) -> usize {
    //     let _ = self.num_jobs.lock().await;
    //     let inner = self.inner_rb.lock().await;
    //     return inner.enqueue_index;
    // }

    // /// Checks the next dequeue index within the MultiThreadedRingBuffer
    // ///
    // /// Time Complexity: O(1)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn next_dequeue_index(&self) -> usize {
    //     let _ = self.num_jobs.lock().await;
    //     let inner = self.inner_rb.lock().await;
    //     return inner.dequeue_index;
    // }

    // /// Returns a clone of the item within the MultiThreadedRingBuffer
    // ///
    // /// The T object inside the ring buffer *must* implement the Clone trait
    // ///
    // /// Time Complexity: O(T_t)
    // ///
    // /// Space Complexity: O(T_s)
    // ///
    // /// Where O(T_t) and O(T_s) is the time and space complexity required
    // /// to clone the internals of the T object itself
    // pub async fn get(&self, index: usize) -> Option<T>
    // where T: Clone
    // {
    //     let _ = self.num_jobs.lock().await;
    //     let inner = self.inner_rb.lock().await;
    //     return inner.items[index].clone();
    // }

    // /// Returns a clone of the MultiThreadedRingBuffer in its current state
    // ///
    // /// The T object inside the ring buffer *must* implement the Clone trait
    // ///
    // /// Time Complexity: O(N * O(T_t))
    // ///
    // /// Space Complexity: O(N * O(T_s))
    // ///
    // /// Where O(T_t) and O(T_s) is the time and space complexity required
    // /// to clone the internals of the T object itself
    // pub async fn rb_items(&self) -> Box<[Option<T>]>
    // where T: Clone
    // {
    //     let _ = self.num_jobs.lock().await;
    //     let inner = self.inner_rb.lock().await;
    //     return inner.items.clone();
    // }

    // /// Print out the content inside the MultitThreadedRingBuffer
    // ///
    // /// Time Complexity: O(N)
    // ///
    // /// Space Complexity: O(1)
    // pub async fn print_buffer(&self)
    // where T: Debug
    // {
    //     let _ = self.num_jobs.lock().await;
    //     let inner = self.inner_rb.lock().await;
    //     print!("[");
    //     for item in &inner.items {
    //         print!("{:?}, ", item);
    //     }
    //     print!("]");
    // }
}
