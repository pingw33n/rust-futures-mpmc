// This is implementation of
// http://www.1024cores.net/home/lock-free-algorithms/queues/bounded-mpmc-queue
use futures::prelude::*;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use {SendError, TrySendError, TrySendErrorKind};
use util::*;

pub fn array<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel::new(capacity));
    (Sender(channel.clone()), Receiver(channel))
}

pub struct Sender<T>(Arc<Channel<T>>);

impl<T> Sender<T> {
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.0.try_send(item)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        debug_assert!(self.0.sender_count.load(Ordering::SeqCst) < usize::max_value());
        self.0.sender_count.fetch_add(1, Ordering::SeqCst);
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        debug_assert!(self.0.sender_count.load(Ordering::SeqCst) > 0);
        if self.0.sender_count.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Last sender was dropped, notify receivers.
            self.0.waiting_receivers.notify_all();
        }
    }
}

impl<T> Sink for Sender<T> {
    type SinkItem = T;
    type SinkError = SendError<T>;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.0.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

pub struct Receiver<T>(Arc<Channel<T>>);

impl<T> Receiver<T> {
    pub fn try_receive(&self) -> Option<Option<T>> {
        self.0.try_receive()
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        debug_assert!(self.0.receiver_count.load(Ordering::SeqCst) < usize::max_value());
        self.0.receiver_count.fetch_add(1, Ordering::SeqCst);
        Receiver(self.0.clone())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        debug_assert!(self.0.receiver_count.load(Ordering::SeqCst) > 0);
        if self.0.receiver_count.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Last receiver was dropped, notify senders.
            self.0.waiting_senders.notify_all();
        }
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll()
    }
}

struct Channel<T> {
    buf: *mut Slot<T>,
    buf_len_mask: usize,
    send_pos: CachePadded<AtomicUsize>,
    receive_pos: CachePadded<AtomicUsize>,
    waiting_senders: TaskSet,
    waiting_receivers: TaskSet,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Channel<T> {
    pub fn new(buf_len: usize) -> Self {
        assert!(buf_len >= 2 && buf_len <= isize::max_value() as usize);
        let buf_len = buf_len.next_power_of_two();
        let buf_len_mask = buf_len - 1;
        let buf: *mut Slot<T> = {
            let mut vec = Vec::with_capacity(buf_len);
            let buf = vec.as_mut_ptr();
            mem::forget(vec);
            buf
        };
        for i in 0..buf_len {
            unsafe {
                ptr::write(&mut (*buf.add(i)).seq, AtomicUsize::new(i));
            }
        }
        Self {
            buf,
            buf_len_mask,
            send_pos: AtomicUsize::new(0).into(),
            receive_pos: AtomicUsize::new(0).into(),
            waiting_senders: TaskSet::new(),
            waiting_receivers: TaskSet::new(),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf_len_mask + 1
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.try_send1(item, false)
    }

    pub fn try_receive(&self) -> Option<Option<T>> {
        self.try_receive1(false)
    }

    pub fn start_send(&self, item: T) -> StartSend<T, SendError<T>> {
        match self.try_send1(item, true) {
            Ok(()) => Ok(AsyncSink::Ready),
            Err(TrySendError(TrySendErrorKind::Full(v))) => Ok(AsyncSink::NotReady(v)),
            Err(TrySendError(TrySendErrorKind::Disconnected(v))) => Err(SendError(v)),
        }
    }

    pub fn poll(&self) -> Poll<Option<T>, ()> {
        Ok(match self.try_receive1(true) {
            Some(v) => Async::Ready(v),
            None => Async::NotReady,
        })
    }

    #[inline]
    fn try_send1(&self, mut item: T, block: bool) -> Result<(), TrySendError<T>> {
        loop {
            if !self.has_receivers() {
                break Err(TrySendError(TrySendErrorKind::Disconnected(item)));
            }
            let notify_stamp = if block {
                self.waiting_senders.notify_stamp()
            } else {
                0
            };
            match self.try_send0(item) {
                Ok(_) => {
                    if block {
                        self.waiting_receivers.notify_one();
                    }
                    break Ok(());
                }
                Err(e) => {
                    let v = e.into_inner();
                    if !block || self.waiting_senders.try_register(notify_stamp) {
                        break Err(TrySendError(TrySendErrorKind::Full(v)));
                    }
                    item = v;
                    // Senders have just been notified, spin.
                }
            }
        }
    }

    fn try_send0(&self, item: T) -> Result<(), TrySendError<T>> {
        let mut spinner = Spinner::new();
        let mut pos = self.send_pos.load(Ordering::Relaxed);
        let slot = loop {
            let (slot, seq) = unsafe {
                let slot = self.buf.add(pos & self.buf_len_mask);
                let seq = (*slot).seq.load(Ordering::Acquire);
                (slot, seq)
            };
            let diff = seq as isize - pos as isize;
            if diff == 0 {
                if self.send_pos.compare_exchange_weak(pos, pos.wrapping_add(1),
                        Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                    break slot;
                }
            } else if diff < 0 && !spinner.spin() {
                return Err(TrySendError(TrySendErrorKind::Full(item)));
            } else {
                pos = self.send_pos.load(Ordering::Relaxed);
            };
        };
        unsafe {
            ptr::write(&mut (*slot).item, item);
            (*slot).seq.store(pos.wrapping_add(1), Ordering::Release);
        }
        Ok(())
    }

    #[inline]
    fn try_receive1(&self, block: bool) -> Option<Option<T>> {
        loop {
            let notify_stamp = if block {
                self.waiting_receivers.notify_stamp()
            } else {
                0
            };
            match self.try_receive0() {
                Some(v) => {
                    if block {
                        self.waiting_senders.notify_one();
                    }
                    break Some(Some(v));
                }
                None => {
                    if !self.has_senders() {
                        break Some(None);
                    }
                    if !block || self.waiting_receivers.try_register(notify_stamp) {
                        break None;
                    }
                    // Receivers have just been notified, spin.
                }
            }
        }
    }

    fn try_receive0(&self) -> Option<T> {
        let mut spinner = Spinner::new();
        let mut pos = self.receive_pos.load(Ordering::Relaxed);
        let slot = loop {
            let (slot, seq) = unsafe {
                let slot = self.buf.add(pos & self.buf_len_mask);
                let seq = (*slot).seq.load(Ordering::Acquire);
                (slot, seq)
            };
            let diff = seq as isize - pos.wrapping_add(1) as isize;
            if diff == 0 {
                if self.receive_pos.compare_exchange_weak(pos, pos.wrapping_add(1),
                        Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                    break slot;
                }
            } else if diff < 0 && !spinner.spin() {
                return None;
            } else {
                pos = self.receive_pos.load(Ordering::Relaxed);
            };
        };
        unsafe {
            let item = ptr::read(& (*slot).item);
            (*slot).seq.store(pos.wrapping_add(self.buf_len_mask).wrapping_add(1),
                Ordering::Release);
            Some(item)
        }
    }

    pub fn has_receivers(&self) -> bool {
        self.receiver_count.load(Ordering::SeqCst) != 0
    }

    pub fn has_senders(&self) -> bool {
        self.sender_count.load(Ordering::SeqCst) != 0
    }
}

unsafe impl<T: Send> Send for Channel<T> {}
unsafe impl<T: Sync> Sync for Channel<T> {}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        // Drain the buffer.
        while let Some(Some(_)) = self.try_receive() {}
        unsafe {
            let _ = Vec::from_raw_parts(self.buf, 0, self.capacity());
        }
    }
}

struct Slot<T> {
    seq: AtomicUsize,
    item: T,
}