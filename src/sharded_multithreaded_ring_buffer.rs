use rand::Rng;
use std::{
    cell::RefCell,
    cmp,
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
    usize,
};
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, PartialEq, Eq)]
enum Acquire {
    Enqueue,
    Dequeue,
}

#[derive(Debug)]
pub enum EnqStatus {
    Poisoned,
    Success,
}

// Each thread will own its own shard index and utilize cache
// effectively to find an unoccupied shard
thread_local! {
    static SHARD_INDEX: std::cell::RefCell<Option<usize>> = RefCell::new(None);
}

/// A sharded ring (circular) buffer struct that can only be used in a *multi-threaded environment*,
/// using a [BoxedSlice] of InnerRingBuffers under the hood.
/// See the [Wikipedia article](https://en.wikipedia.org/wiki/Circular_buffer) for more info.
#[derive(Debug)]
pub struct ShardedMultiThreadedRingBuffer<T> {
    capacity: usize,
    shards: usize,
    max_capacity_per_shard: usize,
    num_jobs: AtomicUsize,
    // global lock used for printing or cloning parts
    // of the data structure
    global_lock: Arc<RwLock<()>>,
    // Used to determine which shard a thread should work on:
    // An atomic bool denoting if the shard is taken or not
    // An atomic usize val denoting if job is at capacity or not
    shard_jobs: Box<[(AtomicBool, AtomicUsize)]>,
    // Multiple InnerRingBuffer structure based on num of shards
    inner_rb: Box<[Mutex<InnerRingBuffer<T>>]>,
    // Poisoned state of the buffer (important for dequeurer threads)
    poisoned: AtomicBool,
}

// An inner ring buffer to contain the items, enqueue, and dequeue index for ShardedMultiThreadedRingBuffer struct
#[derive(Debug, Default, Clone)]
struct InnerRingBuffer<T> {
    items: Box<[Option<T>]>,
    enqueue_index: usize,
    dequeue_index: usize,
}

/// Implements the InnerRingBuffer functions
impl<T> InnerRingBuffer<T> {
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
    /// Note: The capacity of this buffer will always be rounded up to
    /// the next positive integer that is divisible by the provided shards.
    /// The provided shard value can only be a *positive* integer.
    ///
    /// Time Complexity: O(s) where s is the number of shards
    ///
    /// Space Complexity: O(s * c_s) where s is the number of shards and c_s
    /// is the capacity per shard (space usage also depends on T)
    pub fn new(capacity: usize, shards: usize) -> Self {
        Self {
            capacity: (capacity as f64 / shards as f64).ceil() as usize * shards,
            shards: cmp::max(shards, 1),
            max_capacity_per_shard: cmp::max((capacity + shards - 1) / shards, 1),
            num_jobs: AtomicUsize::new(0),
            global_lock: Arc::new(RwLock::default()),
            // shard_ind: ThreadLocal::new(),
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
                        (capacity + shards - 1) / shards,
                        1,
                    ))));
                }
                vec.into_boxed_slice()
            },
            poisoned: AtomicBool::new(false),
        }
    }

    /// Helper function for a thread to acquire a specific shard within
    /// self.shard_jobs for enqueuing or dequeuing purposes. It iterates
    /// in a ring buffer like manner to give each shard equal weight. Yielding
    /// is done through exponential backoff (capped at 20 ms) so that this function
    /// isn't fully occupying the CPU at all times.
    ///
    /// The time complexity of this depends on number of enquerer and
    /// dequerer threads there are; ideally, you would have similar number
    /// of enquerer and dequerer threads to use this properly.
    ///
    /// Space Complexity: O(1)
    async fn acquire_shard(&self, acquire: Acquire) -> Option<usize> {
        // Threads start off with a random shard_ind value before going
        // around a circle in the ring buffer (will likely change this to thread local)
        let mut current = SHARD_INDEX.with(|cell| {
            let mut cell_val = cell.borrow_mut();
            let current = match *cell_val {
                Some(val) => (val + 1) % self.shards, // look at the next shard
                None => rand::rng().random_range(0..self.shards), // init rand shard for thread to look at
            };
            *cell_val = Some(current);
            current
        });

        let mut spins = 0;
        let mut attempt: i32 = 0;

        loop {
            if self.shard_jobs[current]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                if match acquire {
                    Acquire::Enqueue => {
                        let shard_full = self.is_shard_full(current);
                        !shard_full
                    }
                    Acquire::Dequeue => {
                        let poisoned = self.poisoned.load(Ordering::Acquire);
                        let empty = self.is_empty();
                        let shard_empty = self.is_shard_empty(current);

                        if poisoned && empty {
                            self.shard_jobs[current].0.store(false, Ordering::Release);
                            return None;
                        }

                        !shard_empty
                    }
                } {
                    break;
                } else {
                    self.shard_jobs[current].0.store(false, Ordering::Release);
                }
            }

            current = (current + 1) % self.shards;

            spins += 1;
            // yield only once the enquerer or dequerer thread has went one round through
            // the shard_job buffer
            if spins % self.shards == 0 {
                // yielding is done through exponential backoff + random jitter
                // max wait of 20ms; jitter allows the threads to wake up at
                // different ms timings
                let backoff_ms = (1u64 << attempt.min(5)).min(20);
                let jitter = rand::rng().random_range(0..=backoff_ms);
                tokio::time::sleep(Duration::from_millis(jitter)).await;
                attempt = attempt.saturating_add(1); // Avoid overflow
            }
        }
        Some(current)
    }

    /// Helper function to add an Option item to the RingBuffer
    /// This is necessary so that the ring buffer can be poisoned with None values
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is)
    ///
    /// Space Complexity: O(1)
    async fn enqueue_item(&self, item: Option<T>) -> EnqStatus {
        // Checks the poison status of the buffer and will return *only*
        // if the threads are finished with dequeuing/enqueuing
        let current = match self.acquire_shard(Acquire::Enqueue).await {
            Some(cur) => cur,
            None => return EnqStatus::Poisoned,
        };

        // Lock the inner ring buffer shard, enqueue the item, update the enqueue index
        let mut inner = self.inner_rb[current].lock().await;
        let enqueue_index = inner.enqueue_index;
        inner.items[enqueue_index] = item;
        inner.enqueue_index = (inner.enqueue_index + 1) % self.max_capacity_per_shard;

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        // AcqRel ordering is used because the val of the num_jobs is acquired, updated,
        // and released at once
        self.num_jobs.fetch_add(1, Ordering::AcqRel);
        self.shard_jobs[current].1.fetch_add(1, Ordering::AcqRel);
        self.shard_jobs[current].0.store(false, Ordering::Release);

        return EnqStatus::Success;
    }

    /// Adds an item of type T to the RingBuffer, *blocking* the thread until there is space to add the item.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    /// Space complexity: O(1)
    pub async fn enqueue(&self, item: T) -> EnqStatus {
        // read lock for access into reading the ShardedMultithreadedRingBuffer structure
        let _read_guard = self.global_lock.read().await;
        return self.enqueue_item(Some(item)).await;
    }

    /// Retrieves an item of type T from the RingBuffer if an item exists in the buffer.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    ///
    /// Space Complexity: O(1)
    pub async fn dequeue(&self) -> Option<T> {
        // read lock for access into reading the ShardedMultithreadedRingBuffer structure
        let _read_guard = self.global_lock.read().await;

        // Locks to read how many jobs are in the ring buffer
        let current = match self.acquire_shard(Acquire::Dequeue).await {
            Some(cur) => cur,
            None => return None,
        };

        // Lock the inner ring buffer shard, dequeue the item, update the dequeue index
        let mut inner = self.inner_rb[current].lock().await;
        let dequeue_index = inner.dequeue_index;
        let item = inner.items[dequeue_index].take();
        inner.dequeue_index = (inner.dequeue_index + 1) % self.max_capacity_per_shard;

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        // AcqRel ordering is used because the val of the num_jobs is acquired, updated,
        // and released at once
        self.num_jobs.fetch_sub(1, Ordering::AcqRel);
        self.shard_jobs[current].1.fetch_sub(1, Ordering::AcqRel);
        self.shard_jobs[current].0.store(false, Ordering::Release);

        item
    }

    /// Poisons the RingBuffer, preventing any more items from being **dequeued**.
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn poison(&self) {
        let _g_write = self.global_lock.write().await;
        self.poisoned.store(true, Ordering::Release);
    }

    /// If the RingBuffer is [poisoned][Self::poison],
    /// this method will allow the RingBuffer to be used again
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn clear_poison(&self) {
        let _g_write = self.global_lock.write().await;
        if self.poisoned.load(Ordering::Acquire) {
            self.poisoned.store(false, Ordering::Release);
        } else {
            println!("Ring buffer is not poisoned or it is empty");
        }
    }

    /// Clears the ShardedMultiThreadedRingBuffer back to an empty state.
    ///
    /// To clear the RingBuffer *only* when it is *poisoned*, see [Self::clear_poison].
    ///
    /// Time Complexity: O(s)
    ///
    /// Space complexity: O(1)
    pub async fn clear(&self) {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        self.num_jobs.store(0, Ordering::Release);
        for shard in 0..self.shards {
            *self.inner_rb[shard].lock().await = InnerRingBuffer::new(self.max_capacity_per_shard);
            self.shard_jobs[shard].0.store(false, Ordering::Release);
            self.shard_jobs[shard].1.store(0, Ordering::Release);
        }
    }

    /// Checks whether the ShardedMultiThreadedRingBuffer is empty or not
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub fn is_empty(&self) -> bool {
        return self.num_jobs.load(Ordering::Acquire) == 0;
    }

    pub fn is_shard_empty(&self, shard_ind: usize) -> bool {
        return self.shard_jobs[shard_ind].1.load(Ordering::Acquire) == 0;
    }

    /// Checks whether the ShardedMultiThreadedRingBuffer is full or not
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub fn is_full(&self) -> bool {
        return self.num_jobs.load(Ordering::Acquire) == self.capacity;
    }

    pub fn is_shard_full(&self, shard_ind: usize) -> bool {
        return self.shard_jobs[shard_ind].1.load(Ordering::Acquire) == self.max_capacity_per_shard;
    }

    /// Checks the next enqueue index within the ShardedMultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_enqueue_index_for_shard(&self, shard_ind: usize) -> Option<usize> {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }
        let inner = self.inner_rb[shard_ind].lock().await;
        return Some(inner.enqueue_index);
    }

    /// Checks the next dequeue index within the ShardedMultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_dequeue_index_for_shard(&self, shard_ind: usize) -> Option<usize> {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }
        let inner = self.inner_rb[shard_ind].lock().await;
        return Some(inner.dequeue_index);
    }

    /// Returns a clone of the item within the ShardedMultiThreadedRingBuffer
    ///
    /// The T object inside the ring buffer *must* implement the Clone trait
    ///
    /// Time Complexity: O(T_t)
    ///
    /// Space Complexity: O(T_s)
    ///
    /// Where O(T_t) and O(T_s) is the time
    /// and space complexity required to clone the internals of the T object
    /// itself
    pub async fn get_item_in_shard(&self, item_index: usize, shard_ind: usize) -> Option<T>
    where
        T: Clone,
    {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        if item_index >= self.max_capacity_per_shard {
            println!("Invalid item index");
            return None;
        }

        let inner = self.inner_rb[shard_ind].lock().await;
        return inner.items[item_index].clone();
    }

    /// Returns a clone of a specific InnerRingBuffer shard in its current state
    ///
    /// The T object inside the ring buffer *must* implement the Clone trait
    ///
    /// Time Complexity: O(c_s * O(T_t))
    ///
    /// Space Complexity: O(c_s * O(T_s))
    ///
    /// Where c_s is the capacity in a shard O(T_t) and O(T_s) is the time and
    /// space complexity required to clone the internals of the T object itself
    pub async fn rb_items_at_shard(&self, shard_ind: usize) -> Option<Box<[Option<T>]>>
    where
        T: Clone,
    {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        let inner = self.inner_rb[shard_ind].lock().await;
        return Some(inner.items.clone());
    }

    /// Returns a clone of the ShardedMultithreadedRingBuffer in its current state
    ///
    /// The T object inside the ring buffer *must* implement the Clone trait
    ///
    /// Time Complexity: O(s * c_s * O(T_t))
    ///
    /// Space Complexity: O(s * c_s * O(T_s))
    ///
    /// Where s is the number of shards, c_s is the capacity in a shard,
    /// and O(T_t) and O(T_s) is the time and space complexity required
    /// to clone the internals of the T object itself
    pub async fn rb_items(&self) -> Box<[Box<[Option<T>]>]>
    where
        T: Clone,
    {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        let mut vec = Vec::new();

        for i in 0..self.shards {
            vec.push(self.inner_rb[i].lock().await.items.clone());
        }

        return vec.into_boxed_slice();
    }

    /// Print out the content inside the ShardedMultiThreadedRingBuffer
    ///
    /// Time Complexity: O(N)
    ///
    /// Space Complexity: O(1)
    pub async fn print_buffer(&self)
    where
        T: Debug,
    {
        // write lock to prevent access into modifying the ShardedMultithreadedRingBuffer
        // structure
        let _g_write = self.global_lock.write().await;

        for i in 0..self.shards {
            let inner = self.inner_rb[i].lock().await;
            print!("Shard {i}: ");
            print!("[");
            for item in &inner.items {
                print!("{:?}, ", item);
            }
            print!("]");
            println!();
        }
    }
}
