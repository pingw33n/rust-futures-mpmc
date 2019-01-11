use futures::task::{self, Task};
use std::collections::VecDeque;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;
use std::sync::atomic::{self, AtomicUsize, Ordering};
use std::thread;

pub struct TaskSet {
    tasks: Mutex<VecDeque<Task>>,
    len: AtomicUsize,
    notify_stamp: AtomicUsize,
}

impl TaskSet {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
            notify_stamp: AtomicUsize::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len.load(Ordering::SeqCst) == 0
    }

    pub fn notify_stamp(&self) -> usize {
        self.notify_stamp.load(Ordering::SeqCst)
    }

    pub fn is_not_notified(&self, stamp: usize) -> bool {
        self.notify_stamp() == stamp
    }

    pub fn notify_one(&self) {
        self.notify_stamp.fetch_add(1, Ordering::SeqCst);
        if !self.is_empty() {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(task) = tasks.pop_front() {
                task.notify();
                self.len.store(tasks.len(), Ordering::SeqCst);
            }
        }
    }

    pub fn notify_all(&self) {
        self.notify_stamp.fetch_add(1, Ordering::SeqCst);
        if !self.is_empty() {
            let mut tasks = self.tasks.lock().unwrap();
            if !tasks.is_empty() {
                for task in tasks.drain(..) {
                    task.notify();
                }
                self.len.store(0, Ordering::SeqCst);
            }
        }
    }

    #[must_use]
    pub fn try_register(&self, notify_stamp: usize) -> bool {
        if !self.is_not_notified(notify_stamp) {
            return false;
        }
        let mut tasks = self.tasks.lock().unwrap();
        self.len.store(tasks.len() + 1, Ordering::SeqCst);
        if self.is_not_notified(notify_stamp) {
            if tasks.iter().all(|t| !t.will_notify_current()) {
                tasks.push_back(task::current());
            } else {
                self.len.store(tasks.len(), Ordering::SeqCst);
            }
            true
        } else {
            self.len.store(tasks.len(), Ordering::SeqCst);
            false
        }
    }
}

#[derive(Clone, Default)]
#[repr(align(64))]
pub struct CachePadded<T> {
    inner: T,
}

unsafe impl<T: Send> Send for CachePadded<T> {}
unsafe impl<T: Sync> Sync for CachePadded<T> {}

impl<T> CachePadded<T> {
    /// Pads a value to the length of a cache line.
    pub fn new(t: T) -> CachePadded<T> {
        CachePadded::<T> { inner: t }
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: fmt::Debug> fmt::Debug for CachePadded<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner: &T = &*self;
        write!(f, "CachePadded {{ {:?} }}", inner)
    }
}

impl<T> From<T> for CachePadded<T> {
    fn from(t: T) -> Self {
        CachePadded::new(t)
    }
}

pub struct Spinner(usize);

impl Spinner {
    #[inline]
    pub fn new() -> Self {
        Spinner(0)
    }

    #[inline]
    pub fn spin(&mut self) -> bool {
        if self.0 <= 6 {
            for _ in 0..1 << self.0 {
                atomic::spin_loop_hint();
            }
            self.0 += 1;
            true
        } else if self.0 <= 10 {
            thread::yield_now();
            self.0 += 1;
            true
        } else {
            thread::yield_now();
            false
        }
    }
}