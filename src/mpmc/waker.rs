use std::sync::{atomic::{AtomicBool,Ordering}, Mutex};
use super::{
    context::Context,
    select::{Operation, Selected},
};

// 线程的select可能会造成阻塞，Entry代表一个线程阻塞在一个指定的channel上的操作
pub(crate) struct Entry {
    // 这个操作
    pub(crate) oper: Operation,
    // 可选的packet
    pub(crate) packet: *mut (),
    // 与线程所做的操作相关的Context
    pub(crate) cx: Context,
}

// 这个数据结构被线程用来记录阻塞操作，并在操作准备就绪后被唤醒
pub(crate) struct Waker {
    // 一系列的select 操作
    selectors: Vec<Entry>,
    // 等待ready的operation list
    observers: Vec<Entry>,
}

impl Waker {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            selectors: Vec::new(),
            observers: Vec::new(),
        }
    }

    //注册一个select操作
    #[inline]
    pub(crate) fn register(&mut self, oper: Operation, cx: &Context) {
        self.register_with_packet(oper, std::ptr::null_mut(), cx);
    }

    // 注册一个select操作和一个packet
    #[inline]
    pub(crate) fn register_with_packet(&mut self, oper: Operation, packet: *mut (), cx: &Context) {
        self.selectors.push(Entry {
            oper,
            packet,
            cx: cx.clone(),
        });
    }

    //取消一个select操作
    #[inline]
    pub(crate) fn unregister(&mut self, oper: Operation) -> Option<Entry> {
        if let Some((i, _)) = self
            .selectors
            .iter()
            .enumerate()
            .find(|&(_, entry)| entry.oper == oper)
        {
            let entry = self.selectors.remove(i);
            Some(entry)
        } else {
            None
        }
    }

    // 尝试寻找其他线程的entry，select这个操作，并唤醒它
    #[inline]
    pub(crate) fn try_select(&mut self) -> Option<Entry> {
        self.selectors
            .iter()
            .position(|selector| {
                selector.cx.thread_id() != current_thread_id()
                    && selector
                        .cx
                        .try_select(Selected::Operation(selector.oper))
                        .is_ok()
                    && {
                        selector.cx.store_packet(selector.packet);
                        selector.cx.unpark();
                        true
                    }
            })
            .map(|pos| self.selectors.remove(pos))
    }

    // 通知所有等待准备的操作
    #[inline]
    pub(crate) fn notify(&mut self) {
        // 移除所有的监视者并unpark他们
        for entry in self.observers.drain(..){
            if entry.cx.try_select(Selected::Operation(entry.oper)).is_ok(){
                entry.cx.unpark();
            }
        }
    }

    // 通知所有注册的操作，channel被断开了
    #[inline]
    pub(crate) fn disconnect(&mut self) {
        for entry in self.selectors.iter() {
            if entry.cx.try_select(Selected::Disconnected).is_ok() {
                // 唤醒所有线程.
                // 这里我们不会移除从队列中移除entry。被register的线程需要从waker中自己调用unregister，
                // 如果有必要它们也可能会恢复packet值，并销毁他们
                entry.cx.unpark();
            }
        }

        self.notify();
    }
}

impl Drop for Waker {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(self.selectors.len(), 0);
        debug_assert_eq!(self.observers.len(), 0);
    }
}

// 一个可以被线程之间共享而不会被阻塞的Waker
pub(crate) struct SyncWaker {
    inner: Mutex<Waker>,
    is_empty: AtomicBool,
}

impl SyncWaker {
    #[inline]
    pub(crate) fn new() -> Self {
        SyncWaker {
            inner: Mutex::new(Waker::new()),
            is_empty: AtomicBool::new(true),
        }
    }
    #[inline]
    pub(crate) fn register(&self, oper: Operation, cx: &Context) {
        let mut inner = self.inner.lock().unwrap();
        inner.register(oper, cx);
        self.is_empty
            .store(inner.selectors.is_empty() && inner.observers.is_empty(), Ordering::SeqCst);
    }
    #[inline]
    pub(crate) fn unregister(&self, oper: Operation) -> Option<Entry> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.unregister(oper);
        self.is_empty
            .store(inner.selectors.is_empty() && inner.observers.is_empty(), Ordering::SeqCst);
        entry
    }
    #[inline]
    pub(crate) fn notify(&self) {
        if !self.is_empty.load(Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            if !self.is_empty.load(Ordering::SeqCst) {
                inner.try_select();
                inner.notify();
                self.is_empty.store(
                    inner.selectors.is_empty() && inner.observers.is_empty(),
                    Ordering::SeqCst,
                );
            }
        }
    }
    #[inline]
    pub(crate) fn disconnect(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.disconnect();
        self.is_empty
            .store(inner.selectors.is_empty() && inner.observers.is_empty(), Ordering::SeqCst);
    }
}

impl Drop for SyncWaker {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(self.is_empty.load(std::sync::atomic::Ordering::SeqCst));
    }
}

#[inline]
pub fn current_thread_id() -> usize {
    // `u8` is not drop so this variable will be available during thread destruction,
    // whereas `thread::current()` would not be
    thread_local! { static DUMMY: u8 = 0 }
    
    DUMMY.with(|x| (x as *const u8) as usize)
}
