// Many tests were "stolen" from crossbeam-channel.
extern crate futures_mpmc;
extern crate futures;

use futures::lazy;
use futures::prelude::*;
use std::mem;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use futures_mpmc::array::*;

trait AssertTraits: Send {}
impl AssertTraits for Sender<i32> {}
impl AssertTraits for Receiver<i32> {}

#[test]
fn send_recv() {
    let (tx, rx) = array::<i32>(16);
    let mut rx = rx.wait();

    tx.send(1).wait().unwrap();

    let x = rx.next();

    assert_eq!(x.unwrap(), Ok(1));
}

#[test]
fn send_shared_recv() {
    let (tx1, rx1) = array::<i32>(16);
    let tx2 = tx1.clone();
    let rx2 = rx1.clone();

    let mut rx1 = rx1.wait();
    let mut rx2 = rx2.wait();

    tx1.send(1).wait().unwrap();
    assert_eq!(rx1.next().unwrap(), Ok(1));

    tx2.send(2).wait().unwrap();
    assert_eq!(rx2.next().unwrap(), Ok(2));
}

#[test]
fn send_recv_threads() {
    let (tx, rx) = array::<i32>(16);
    let mut rx = rx.wait();

    thread::spawn(move|| {
        tx.send(1).wait().unwrap();
    }).join().unwrap();

    assert_eq!(rx.next().unwrap(), Ok(1));
}

#[derive(Debug)]
struct DropChecked<T> {
    v: T,
    dropped: AtomicBool,
    on_drop: Arc<AtomicUsize>,
}

impl<T> DropChecked<T> {
    pub fn with_on_drop(v: T, on_drop: Arc<AtomicUsize>) -> Self {
        Self {
            v,
            dropped: AtomicBool::new(false),
            on_drop,
        }
    }

    pub fn new(v: T) -> Self {
        Self::with_on_drop(v, Arc::new(AtomicUsize::new(0)))
    }
}

impl<T: PartialEq> PartialEq for DropChecked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.v == other.v
    }
}

impl<T: PartialEq> Eq for DropChecked<T> {}

impl<T> Drop for DropChecked<T> {
    fn drop(&mut self) {
        assert!(!self.dropped.compare_and_swap(false, true, Ordering::SeqCst));
        self.on_drop.fetch_add(1, Ordering::SeqCst);
    }
}

impl<T> From<T> for DropChecked<T> {
    fn from(t: T) -> Self {
        Self::new(t)
    }
}

#[test]
fn drops() {
    let (tx, rx) = array(10);
    tx.try_send(DropChecked::new(1)).unwrap();

    let item2 = Arc::new(AtomicUsize::new(0));
    tx.try_send(DropChecked::with_on_drop(2, item2.clone())).unwrap();

    let item3 = Arc::new(AtomicUsize::new(0));
    tx.try_send(DropChecked::with_on_drop(3, item3.clone())).unwrap();

    mem::drop(tx);

    assert_eq!(rx.try_receive(), Some(Some(1.into())));
    assert_eq!(item2.load(Ordering::SeqCst), 0);
    assert_eq!(item3.load(Ordering::SeqCst), 0);
    mem::drop(rx);

    assert_eq!(item2.load(Ordering::SeqCst), 1);
    assert_eq!(item3.load(Ordering::SeqCst), 1);
}

#[test]
fn no_rx_try_send_error() {
    let (tx, rx) = array(10);
    mem::drop(rx);

    assert!(tx.try_send(123).is_err());
}

#[test]
fn stress_try_send_receive() {
    #[derive(Debug, Eq, PartialEq)]
    struct NonClone<T>(T);

    let (tx, rx) = array(2);
    const AMT: usize = 20000;
    const NTHREADS: usize = 4;

    let sent = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(AtomicUsize::new(0));

    let mut threads = Vec::new();
    for _ in 0..NTHREADS {
        let tx = tx.clone();
        let sent = sent.clone();
        threads.push(thread::spawn(move || {
            while sent.load(Ordering::Relaxed) < AMT {
                while let Err(e) = tx.try_send(NonClone(123)) {
                    let v = e.into_inner();
                    assert_eq!(v, NonClone(123));
                }
                sent.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    mem::drop(tx);

    for _ in 0..NTHREADS {
        let rx = rx.clone();
        let received = received.clone();
        threads.push(thread::spawn(move || {
            loop {
                match rx.try_receive() {
                    Some(Some(v)) => {
                        assert_eq!(v, NonClone(123));
                        received.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(None) => break,
                    None => {}
                }
            }
        }));
    }

    for t in threads {
        t.join().unwrap();
    }

    let sent = sent.load(Ordering::Relaxed);
    let received = received.load(Ordering::Relaxed);
    assert_eq!(sent, received);
    assert!(sent >= AMT);
}

#[test]
fn stress_shared_array_hard() {
    const AMT: usize = 10000;
    const NTHREADS: usize = 4;
    let (tx, rx) = array::<i32>(2);

    let received = Arc::new(AtomicUsize::new(0));

    let mut threads = Vec::new();
    for _ in 0..NTHREADS {
        let mut rx = rx.clone().wait();
        let received = received.clone();

        threads.push(thread::spawn(move|| {
            while let Some(v) = rx.next() {
                assert_eq!(v, Ok(1));
                received.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for _ in 0..NTHREADS {
        let mut tx = tx.clone();

        thread::spawn(move|| {
            for _ in 0..AMT {
                tx = tx.send(1).wait().unwrap();
            }
        });
    }

    drop(rx);
    drop(tx);

    for t in threads {
        t.join().ok().unwrap();
    }

    assert_eq!(received.load(Ordering::Relaxed), AMT * NTHREADS);
}

#[test]
fn stress_receiver_multi_task_array_hard() {
    const AMT: usize = 10000;
    const NTHREADS: u32 = 2;

    let (mut tx, rx) = array::<usize>(2);
    let rx = Arc::new(Mutex::new(Some(rx)));
    let n = Arc::new(AtomicUsize::new(0));

    let mut th = vec![];

    for _ in 0..NTHREADS {
        let rx = rx.clone();
        let n = n.clone();

        let t = thread::spawn(move || {
            let mut i = 0;

            loop {
                i += 1;
                let mut lock = rx.lock().ok().unwrap();

                match lock.take() {
                    Some(mut rx) => {
                        if i % 5 == 0 {
                            let (item, rest) = rx.into_future().wait().ok().unwrap();

                            if item.is_none() {
                                break;
                            }

                            n.fetch_add(1, Ordering::Relaxed);
                            *lock = Some(rest);
                        } else {
                            // Just poll
                            let n = n.clone();
                            let r = lazy(move || {
                                let r = match rx.poll().unwrap() {
                                    Async::Ready(Some(_)) => {
                                        n.fetch_add(1, Ordering::Relaxed);
                                        *lock = Some(rx);
                                        false
                                    }
                                    Async::Ready(None) => {
                                        true
                                    }
                                    Async::NotReady => {
                                        *lock = Some(rx);
                                        false
                                    }
                                };

                                Ok::<bool, ()>(r)
                            }).wait().unwrap();

                            if r {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
        });

        th.push(t);
    }

    for i in 0..AMT {
        tx = tx.send(i).wait().unwrap();
    }

    drop(tx);

    for t in th {
        t.join().unwrap();
    }

    assert_eq!(AMT, n.load(Ordering::Relaxed));
}

/// Stress test that receiver properly receives all the messages
/// after sender dropped.
#[test]
fn stress_drop_sender() {
    fn list() -> impl Stream<Item=i32, Error=u32> {
        let (tx, rx) = array(3);
        tx.send(Ok(1))
            .and_then(move |tx| tx.send(Ok(2)))
            .and_then(move |tx| tx.send(Ok(3)))
            .wait()
            .unwrap();
        rx.then(|r| r.unwrap())
    }

    for _ in 0..10000 {
        assert_eq!(list().wait().collect::<Result<Vec<_>, _>>(),
            Ok(vec![1, 2, 3]));
    }
}

#[test]
fn zero_size_struct() {
    struct ZeroSize;
    let (tx, rx) = array::<ZeroSize>(10);
    tx.send(ZeroSize)
        .and_then(|tx| tx.send(ZeroSize))
        .wait()
        .unwrap();
    let mut rx = rx.wait();
    assert!(rx.next().unwrap().is_ok());
    assert!(rx.next().unwrap().is_ok());
}