use std::{
    cell::UnsafeCell,
    fmt,
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::TrySendError,
        Mutex,
    },
    time::Instant,
};

use super::{
    context::Context, errors::*, select::{Token,Operation,Selected}, utils::Backoff, waker::Waker,
};

// 指向Packet的一个指针
pub(crate) struct ZeroToken(*mut ());

impl Default for ZeroToken {
    fn default() -> Self {
        Self(std::ptr::null_mut())
    }
}

impl fmt::Debug for ZeroToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&(self.0 as usize), f)
    }
}

// 从sender到recv传递信息的一个slot
struct Packet<T> {
    // 这个结构体是分配在stack上的
    on_stack: bool,
    // 这个packet是否准备好读或写
    ready: AtomicBool,
    // msg
    msg: UnsafeCell<Option<T>>,
}

impl<T> Packet<T> {
    fn empty_on_stack() -> Packet<T> {
        Packet {
            on_stack: true,
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(None),
        }
    }
    fn message_on_stack(msg: T) -> Packet<T> {
        Packet {
            on_stack: true,
            ready: AtomicBool::new(false),
            msg: UnsafeCell::new(Some(msg)),
        }
    }
    // 等待直到packet变得ready
    fn wait_ready(&self) {
        let backoff = Backoff::new();
        while !self.ready.load(Ordering::Acquire) {
            backoff.spin_heavy();
        }
    }
}

// channel实体
struct Inner {
    // 等待配对接收方的sender
    senders: Waker,
    // 等待配置发送方的recv
    receivers: Waker,
    // 当channel断开连接时为true
    is_disconnected: bool,
}
// 注意通道访问的互斥性 Channel的泛型代表sender和recv的消息类型
pub(crate) struct Channel<T> {
    inner: Mutex<Inner>,
    // 删除通道意味删除一个T
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                senders: Waker::new(),
                receivers: Waker::new(),
                is_disconnected: false,
            }),
            _marker: PhantomData,
        }
    }

    // 向一个packet写入一个msg
    pub(crate) unsafe fn write(&self, token: &mut Token, msg: T) -> Result<(), T> {
        if token.zero.0.is_null() {
            return Err(msg);
        }

        let packet = &*(token.zero.0 as *const Packet<T>);
        packet.msg.get().write(Some(msg));
        packet.ready.store(true, Ordering::Release);
        Ok(())
    }

    // 从packet读取msg
    pub(crate) unsafe fn read(&self, token: &mut Token) -> Result<T, ()> {
        if token.zero.0.is_null() {
            return Err(());
        }
        let packet = &*(token.zero.0 as *const Packet<T>);
        if packet.on_stack {
            let msg = packet.msg.get().replace(None).unwrap();
            packet.ready.store(true, Ordering::Release);
            Ok(msg)
        } else {
            // 等待 packet被分配在堆上
            packet.wait_ready();
            let msg = packet.msg.get().replace(None).unwrap();
            // 删除避免内存泄漏
            drop(Box::from_raw(token.zero.0 as *mut Packet<T>));
            Ok(msg)
        }
    }

    // 尝试将msg写入channel
    pub(crate) fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let token = &mut Token::default();
        let mut inner = self.inner.lock().unwrap();
        if let Some(operation) = inner.receivers.try_select() {
            token.zero.0 = operation.packet;
            drop(inner);
            unsafe {
                self.write(token, msg).ok().unwrap();
            }
            Ok(())
        } else if inner.is_disconnected {
            Err(TrySendError::Disconnected(msg))
        } else {
            Err(TrySendError::Full(msg))
        }
    }

    pub(crate) fn send(
        &self,
        msg: T,
        deadline: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        let token = &mut Token::default();
        let mut inner = self.inner.lock().unwrap();

        // If there's a waiting receiver, pair up with it.
        if let Some(operation) = inner.receivers.try_select() {
            token.zero.0 = operation.packet;
            drop(inner);
            unsafe {
                self.write(token, msg).ok().unwrap();
            }
            return Ok(());
        }

        if inner.is_disconnected {
            return Err(SendTimeoutError::Disconnected(msg));
        }

        Context::with(|cx| {
            // Prepare for blocking until a receiver wakes us up.
            let oper = Operation::hook(token);
            let mut packet = Packet::<T>::message_on_stack(msg);
            inner
                .senders
                .register_with_packet(oper, &mut packet as *mut Packet<T> as *mut (), cx);
            inner.receivers.notify();
            drop(inner);

            // Block the current thread.
            let sel = cx.wait_until(deadline);

            match sel {
                Selected::Waiting => unreachable!(),
                Selected::Aborted => {
                    self.inner.lock().unwrap().senders.unregister(oper).unwrap();
                    let msg = unsafe { packet.msg.get().replace(None).unwrap() };
                    Err(SendTimeoutError::Timeout(msg))
                }
                Selected::Disconnected => {
                    self.inner.lock().unwrap().senders.unregister(oper).unwrap();
                    let msg = unsafe { packet.msg.get().replace(None).unwrap() };
                    Err(SendTimeoutError::Disconnected(msg))
                }
                Selected::Operation(_) => {
                    // Wait until the message is read, then drop the packet.
                    packet.wait_ready();
                    Ok(())
                }
            }
        })
    }

    pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
        let token = &mut Token::default();
        let mut inner = self.inner.lock().unwrap();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            token.zero.0 = operation.packet;
            drop(inner);
            unsafe { self.read(token).map_err(|_| TryRecvError::Disconnected) }
        } else if inner.is_disconnected {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Receives a message from the channel.
    pub(crate) fn recv(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let token = &mut Token::default();
        let mut inner = self.inner.lock().unwrap();

        // If there's a waiting sender, pair up with it.
        if let Some(operation) = inner.senders.try_select() {
            token.zero.0 = operation.packet;
            drop(inner);
            unsafe {
                return self.read(token).map_err(|_| RecvTimeoutError::Disconnected);
            }
        }

        if inner.is_disconnected {
            return Err(RecvTimeoutError::Disconnected);
        }

        Context::with(|cx| {
            // Prepare for blocking until a sender wakes us up.
            let oper = Operation::hook(token);
            let mut packet = Packet::<T>::empty_on_stack();
            inner.receivers.register_with_packet(
                oper,
                &mut packet as *mut Packet<T> as *mut (),
                cx,
            );
            inner.senders.notify();
            drop(inner);

            // Block the current thread.
            let sel = cx.wait_until(deadline);

            match sel {
                Selected::Waiting => unreachable!(),
                Selected::Aborted => {
                    self.inner
                        .lock()
                        .unwrap()
                        .receivers
                        .unregister(oper)
                        .unwrap();
                    Err(RecvTimeoutError::Timeout)
                }
                Selected::Disconnected => {
                    self.inner
                        .lock()
                        .unwrap()
                        .receivers
                        .unregister(oper)
                        .unwrap();
                    Err(RecvTimeoutError::Disconnected)
                }
                Selected::Operation(_) => {
                    // Wait until the message is provided, then read it.
                    packet.wait_ready();
                    unsafe { Ok(packet.msg.get().replace(None).unwrap()) }
                }
            }
        })
    }

    /// Disconnects the channel and wakes up all blocked senders and receivers.
    ///
    /// Returns `true` if this call disconnected the channel.
    pub(crate) fn disconnect(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();

        if !inner.is_disconnected {
            inner.is_disconnected = true;
            inner.senders.disconnect();
            inner.receivers.disconnect();
            true
        } else {
            false
        }
    }
    pub(crate) fn len(&self) -> usize {
        0
    }
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn capacity(&self) -> Option<usize> {
        Some(0)
    }
    pub(crate) fn is_empty(&self) -> bool {
        true
    }
    pub(crate) fn is_full(&self) -> bool {
        true
    }
}
