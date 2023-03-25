use std::{
    ops,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

/*
 * Receiver和Sender非常像Arc的实现，其内部的Counter中的chan才是真正的数据存在的地方
 * 使用时创建的Sender和Receiver只是一个Counter的包装，他们共享一个*mut Counter指针
 * 新建一个Sender/Receiver时，内部counter计数会+1，离开作用域要释放内存，内部counter
 * 计数会-1，直到减到0时才会真正的释放Counter
 */

// 内部的计数器
#[allow(dead_code)]
struct Counter<C> {
    // 与channel相关联的senders和receivers的数量
    senders: AtomicUsize,
    receivers: AtomicUsize,
    // 如果最后一个sender或receiver取消了channel的分配，这个值就为true
    destroy: AtomicBool,
    // 内部的Channel
    chan: C,
}

#[allow(dead_code)]
pub(crate) fn new<C>(chan: C) -> (Sender<C>, Receiver<C>) {
    let counter = Box::into_raw(Box::new(Counter {
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
        destroy: AtomicBool::new(false),
        chan,
    }));
    let sender = Sender { counter };
    let recv = Receiver { counter };
    (sender, recv)
}

pub(crate) struct Sender<C> {
    counter: *mut Counter<C>,
}

#[allow(dead_code)]
impl<C> Sender<C> {
    fn counter(&self) -> &Counter<C> {
        unsafe { &*self.counter }
    }
    pub(crate) fn acquire(&self) -> Sender<C> {
        let count = self.counter().senders.fetch_add(1, Ordering::Relaxed);
        if count > isize::MAX as usize {
            std::process::abort();
        }
        Sender {
            counter: self.counter,
        }
    }
    pub(crate) unsafe fn release<F: FnOnce(&C) -> bool>(&self, disconnect: F) {
        if self.counter().senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            disconnect(&self.counter().chan);
            if self.counter().destroy.swap(true, Ordering::AcqRel) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C> ops::Deref for Sender<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.counter().chan
    }
}

impl<C> PartialEq for Sender<C> {
    fn eq(&self, other: &Self) -> bool {
        self.counter == other.counter
    }
}

pub(crate) struct Receiver<C> {
    counter: *mut Counter<C>,
}

#[allow(dead_code)]
impl<C> Receiver<C> {
    fn counter(&self) -> &Counter<C> {
        unsafe { &*self.counter }
    }
    pub(crate) fn acquire(&self) -> Receiver<C> {
        let count = self.counter().receivers.fetch_add(1, Ordering::Relaxed);
        if count > isize::MAX as usize {
            std::process::abort();
        }

        Receiver {
            counter: self.counter,
        }
    }
    pub(crate) unsafe fn release<F: FnOnce(&C) -> bool>(&self, disconnect: F) {
        if self.counter().receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            disconnect(&self.counter().chan);

            if self.counter().destroy.swap(true, Ordering::AcqRel) {
                drop(Box::from_raw(self.counter));
            }
        }
    }
}

impl<C> ops::Deref for Receiver<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.counter().chan
    }
}

impl<C> PartialEq for Receiver<C> {
    fn eq(&self, other: &Receiver<C>) -> bool {
        self.counter == other.counter
    }
}
