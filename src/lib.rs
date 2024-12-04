use std::{
    cell::Cell,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Once, OnceLock,
    },
    task::{Poll, Waker},
};

pub struct OneshotChan<T> {
    inner: Cell<Option<T>>,
    waker: OnceLock<Waker>,
    tx: Once,
    sender_rc: AtomicUsize,
}

unsafe impl<T: Send + Sync> Sync for OneshotChan<T> {}
unsafe impl<T: Send> Send for OneshotChan<T> {}

pub fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(OneshotChan::new());
    (
        Sender {
            chan: Arc::clone(&chan),
        },
        Receiver {
            chan: Arc::clone(&chan),
        },
    )
}

pub struct Sender<T> {
    chan: Arc<OneshotChan<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.sender_rc.fetch_add(1, Ordering::Release);
        Self {
            chan: Arc::clone(&self.chan),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // 最后一个sender被drop
        if self.chan.sender_rc.fetch_sub(1, Ordering::AcqRel) == 1 {
            // 唤醒receiver
            if let Some(waker) = self.chan.waker.get() {
                waker.wake_by_ref();
            }
        }
    }
}

impl<T> Sender<T> {
    fn send(&self, value: T) -> Result<(), T> {
        self.chan.send(value)
    }
}

pub struct Receiver<T> {
    chan: Arc<OneshotChan<T>>,
}

impl<T> Receiver<T> {
    fn recv(&self) -> ReceiveFuture<T> {
        ReceiveFuture {
            chan: self.chan.clone(),
        }
    }
}

impl<T> OneshotChan<T> {
    fn new() -> Self {
        Self {
            inner: Cell::new(None),
            waker: OnceLock::new(),
            tx: Once::new(),
            sender_rc: AtomicUsize::new(1),
        }
    }

    fn send(&self, value: T) -> Result<(), T> {
        let mut data = Some(value);
        self.tx.call_once(|| {
            self.inner.set(data.take());
        });
        match data {
            // 已经调用过send了，之后的调用出错
            Some(v) => Err(v),
            // 第一次调用send会成功
            None => {
                if let Some(waker) = self.waker.get() {
                    waker.wake_by_ref();
                }
                Ok(())
            }
        }
    }
}

pub struct ReceiveFuture<T> {
    chan: Arc<OneshotChan<T>>,
}

impl<T> Future for ReceiveFuture<T> {
    type Output = Option<T>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.chan.waker.get_or_init(|| cx.waker().clone());

        if self.chan.tx.is_completed() {
            Poll::Ready(self.chan.inner.take())
        } else if self.chan.sender_rc.load(Ordering::Acquire) == 0 {
            // 没有sender了
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_basic_send_recv() {
        let (tx, rx) = oneshot();
        tx.send(42).unwrap();
        assert_eq!(rx.recv().await, Some(42));
    }

    #[tokio::test]
    async fn test_multiple_sends_fail() {
        let (tx, rx) = oneshot();
        assert!(tx.send(1).is_ok());
        assert!(tx.send(2).is_err());
        assert_eq!(rx.recv().await, Some(1));
    }

    #[tokio::test]
    async fn test_sender_drop_before_send() {
        let (tx, rx) = oneshot::<i32>();
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn test_receiver_drop_before_receive() {
        let (tx, _rx) = oneshot();
        assert!(tx.send(1).is_ok());
    }

    #[tokio::test]
    async fn test_concurrent_send_receive() {
        for _ in 0..1000 {
            let (tx, rx) = oneshot();

            let tx_handle = tokio::spawn(async move {
                std::thread::sleep(Duration::from_micros(1));
                tx.send(42)
            });

            let rx_handle = tokio::spawn(async move { rx.recv().await });

            let (send_result, receive_result) = tokio::join!(tx_handle, rx_handle);
            assert!(send_result.unwrap().is_ok());
            assert_eq!(receive_result.unwrap(), Some(42));
        }
    }
}
