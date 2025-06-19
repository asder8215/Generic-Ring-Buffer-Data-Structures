use std::{
    cell::{Cell, RefCell, UnsafeCell}, cmp, fmt::Debug, sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
    }, time::Duration, usize
};
use rand::{Rng};

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
pub struct LFShardedMultiThreadedRingBuffer<T> {
    capacity: usize,
    shards: usize,
    max_capacity_per_shard: usize,
    num_jobs: AtomicUsize,
    // Used to determine which shard a thread should work on:
    // An atomic bool denoting if the shard is taken or not
    // A cell usize val denoting if job is at capacity or not
    shard_jobs: Box<[(AtomicBool, Cell<usize>)]>,
    // Multiple InnerRingBuffer structure based on num of shards
    inner_rb: Box<[InnerRingBuffer<T>]>,
    // Poisoned state of the buffer (important for dequeurer threads)
    poisoned: AtomicBool,
    //  This is a global 'locking' flag for clearing, cloning, and printing
    global_flag: AtomicBool,
}

// An inner ring buffer to contain the items, enqueue, and dequeue index for ShardedMultiThreadedRingBuffer struct
#[derive(Debug, Default)]
struct InnerRingBuffer<T> {
    items: Box<[UnsafeCell<Option<T>>]>,
    enqueue_index: Cell<usize>,
    dequeue_index: Cell<usize>,
}

/// Implements the InnerRingBuffer functions
impl<T> InnerRingBuffer<T> {
    /// Instantiates the InnerRingBuffer
    fn new(capacity: usize) -> Self {
        InnerRingBuffer {
            items: {
                let mut vec = Vec::with_capacity(capacity);
                for _i in 0..capacity {
                    vec.push(UnsafeCell::new(None));
                }
                vec.into_boxed_slice()
            },
            enqueue_index: Cell::new(0),
            dequeue_index: Cell::new(0),
        }
    }
}

impl<T: Debug> LFShardedMultiThreadedRingBuffer<T> {
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
            shard_jobs: {
                let mut vec = Vec::with_capacity(shards);
                for _i in 0..shards {
                    vec.push((AtomicBool::new(false), Cell::new(0)));

                }
                vec.into_boxed_slice()
            },
            inner_rb: {
                let mut vec = Vec::with_capacity(shards);
                for _i in 0..shards {
                    vec.push(InnerRingBuffer::new(cmp::max(
                        (capacity + shards - 1) / shards,
                        1,
                    )));
                }
                vec.into_boxed_slice()
            },
            poisoned: AtomicBool::new(false),
            global_flag: AtomicBool::new(false),

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
            if !self.global_flag.load(Ordering::Acquire) {
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

        // Grab inner ring buffer shard, enqueue the item, update the enqueue index
        let inner = &self.inner_rb[current];
        let enqueue_index = inner.enqueue_index.get();
        let item_cell = inner.items[enqueue_index].get();
        unsafe {
            *item_cell = item;
        }
        inner.enqueue_index.set((enqueue_index + 1) % self.max_capacity_per_shard);

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        // AcqRel ordering is used because the val of the num_jobs is acquired, updated,
        // and released at once
        self.num_jobs.fetch_add(1, Ordering::AcqRel);
        self.shard_jobs[current].1.set(self.shard_jobs[current].1.get() + 1);
        self.shard_jobs[current].0.store(false, Ordering::Release);

        return EnqStatus::Success;
    }

    /// Adds an item of type T to the RingBuffer, *blocking* the thread until there is space to add the item.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    /// Space complexity: O(1)
    pub async fn enqueue(&self, item: T) -> EnqStatus {
        return self.enqueue_item(Some(item)).await;
    }

    /// Retrieves an item of type T from the RingBuffer if an item exists in the buffer.
    ///
    /// Time Complexity: O(1) if not blocked (arbitrary time if it is),
    ///
    /// Space Complexity: O(1)
    pub async fn dequeue(&self) -> Option<T> {
        // Locks to read how many jobs are in the ring buffer
        let current = match self.acquire_shard(Acquire::Dequeue).await {
            Some(cur) => cur,
            None => return None,
        };

        // Grab the inner ring buffer shard, dequeue the item, update the dequeue index
        let inner = &self.inner_rb[current];
        let dequeue_index = inner.dequeue_index.get();
        let item = unsafe { (*inner.items[dequeue_index].get()).take() };
        inner.dequeue_index.set((dequeue_index + 1) % self.max_capacity_per_shard);

        // Update the num jobs there are in total, the number of jobs inside
        // each shard, and release the occupation status of this shard atomically
        // AcqRel ordering is used because the val of the num_jobs is acquired, updated,
        // and released at once
        self.num_jobs.fetch_sub(1, Ordering::AcqRel);
        self.shard_jobs[current].1.set(self.shard_jobs[current].1.get() - 1);
        self.shard_jobs[current].0.store(false, Ordering::Release);

        item
    }

    /// Poisons the RingBuffer, preventing any more items from being **dequeued**.
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);

    }

    /// If the RingBuffer is [poisoned][Self::poison],
    /// this method will allow the RingBuffer to be used again
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn clear_poison(&self) {
        // check poisoned flag and clear it if it's on
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
        // spin to acquire the global flag for clearing
        while !self.global_flag
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
        }

        // acquire each shard
        for shard in &self.shard_jobs {
            while !shard
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
            }
        }

        // reset each shard's inner ring buffer and release the shard
        self.num_jobs.store(0, Ordering::Release);
        for shard in 0..self.shards {
            for i in 0..self.max_capacity_per_shard { 
                unsafe {
                    *self.inner_rb[shard].items[i].get() = None;
                }
            }
            self.inner_rb[shard].enqueue_index.set(0);
            self.inner_rb[shard].dequeue_index.set(0);
            self.shard_jobs[shard].1.set(0);
            self.shard_jobs[shard].0.store(false, Ordering::Release);
        }

        // release the clear flag
        self.global_flag.store(false, Ordering::Release);

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
        return self.shard_jobs[shard_ind].1.get() == 0;
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
        return self.shard_jobs[shard_ind].1.get() == self.max_capacity_per_shard;
    }

    /// Checks the next enqueue index within the ShardedMultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_enqueue_index_for_shard(&self, shard_ind: usize) -> Option<usize> {
        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        // spin trying to grab the shard
        while !self.shard_jobs[shard_ind]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
            tokio::task::yield_now().await;
        }

        // grab enq val
        let inner = &self.inner_rb[shard_ind];
        let enq_ind = inner.enqueue_index.get();
        
        // release shard
        self.shard_jobs[shard_ind].0.store(false, Ordering::Release);

        return Some(enq_ind);
    }

    /// Checks the next dequeue index within the ShardedMultiThreadedRingBuffer
    ///
    /// Time Complexity: O(1)
    ///
    /// Space Complexity: O(1)
    pub async fn next_dequeue_index_for_shard(&self, shard_ind: usize) -> Option<usize> {
        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        // spin trying to grab the shard
        while !self.shard_jobs[shard_ind]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
            tokio::task::yield_now().await;
        }

        // grab deq ind val
        let inner = &self.inner_rb[shard_ind];
        let deq_ind = inner.dequeue_index.get();
        
        // release shard
        self.shard_jobs[shard_ind].0.store(false, Ordering::Release);
        return Some(deq_ind);
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
        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        if item_index >= self.max_capacity_per_shard {
            println!("Invalid item index");
            return None;
        }

        // spin trying to grab the shard
        while !self.shard_jobs[shard_ind]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
            tokio::task::yield_now().await;
        }

        // clone item in shard
        let inner = &self.inner_rb[shard_ind];
        let item = unsafe { (*inner.items[item_index].get()).clone() };

        // release shard
        self.shard_jobs[shard_ind].0.store(false, Ordering::Release);

        return item;
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
        if shard_ind >= self.shards {
            println!("Invalid shard index");
            return None;
        }

        // spin trying to grab the shard
        while !self.shard_jobs[shard_ind]
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
            tokio::task::yield_now().await;
        }

        // clone items in shard
        let inner = &self.inner_rb[shard_ind];
        let items = unsafe { 
            let mut vec = Vec::new();
            for item in &inner.items {
                vec.push((*item.get()).clone());
            }
            vec.into_boxed_slice()
        };

        // release shard
        self.shard_jobs[shard_ind].0.store(false, Ordering::Release);

        return Some(items);
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
        // spin to acquire the global flag for cloning
        while !self.global_flag
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
        }

        // acquire shard
        for shard in &self.shard_jobs {
            while !shard
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
            }
        }

        let mut vec = Vec::new();
        let inner = &self.inner_rb;

        // clone items in each shard
        for shard in inner {
            let items = unsafe { 
                let mut shard_vec = Vec::new();
                for item in &shard.items {
                    shard_vec.push((*item.get()).clone());
                }
                shard_vec.into_boxed_slice()
            };
            vec.push(items);
        }

        // release shard
        for shard in &self.shard_jobs {
            shard.0.store(false, Ordering::Release);
        }

        // release clone flag
        self.global_flag.store(false, Ordering::Release);

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
        // spin to acquire the global flag for printing
        while !self.global_flag
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
        }

        // sping to acquire shard
        for shard in &self.shard_jobs {
            while !shard
                .0
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok() {
                tokio::task::yield_now().await;
            }
        }

        // Print items out and release shard
        for shard in 0..self.shards {
            let inner = &self.inner_rb[shard];
            print!("Shard {shard}: ");
            print!("[");
            for item in &inner.items {
                unsafe {
                    print!("{:?}, ", *item.get());
                }
            }
            print!("]");
            println!();
            self.shard_jobs[shard].0.store(false, Ordering::Release);
        }

        // Release print flag
        self.global_flag.store(false, Ordering::Release);
    }
}

// The InnerRingBuffer should definitely be lock free
unsafe impl<T: Send> Sync for InnerRingBuffer<T> {}
unsafe impl<T: Send> Sync for LFShardedMultiThreadedRingBuffer<T> {}
