// 线程本地的channel上下文
use std::{
    cell::Cell,
    ptr,
    sync::{
        atomic::{AtomicPtr, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, Thread},
    time::Instant,
};

use super::select::Selected;
use super::waker::current_thread_id;
//线程本地上下文
#[derive(Debug, Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

//代表Context
#[derive(Debug)]
struct Inner {
    // Selected操作
    select: AtomicUsize,
    // 这是一个Slot，其他线程可能将packet指针存入其中
    packet: AtomicPtr<()>,
    thread: Thread,
    thread_id: usize,
}

impl Context {
    // 在闭包中创建一个新的上下文
    pub fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&Context) -> R,
    {
        thread_local! {
            static CONTEXT:Cell<Option<Context>>= Cell::new(Some(Context::new()));
        }

        let mut f = Some(f);
        let mut f = |cx: &Context| -> R {
            let f = f.take().unwrap();
            f(cx)
        };

        CONTEXT
            .try_with(|cell| match cell.take() {
                Some(cx) => {
                    cx.reset();
                    let res = f(&cx);
                    cell.set(Some(cx));
                    res
                }
                None => f(&Context::new()),
            })
            .unwrap_or_else(|_| f(&Context::new()))
    }

    #[cold]
    fn new() -> Context {
        Context {
            inner: Arc::new(Inner {
                select: AtomicUsize::new(Selected::Waiting.into()),
                packet: AtomicPtr::new(ptr::null_mut()),
                thread: thread::current(),
                thread_id: current_thread_id(),
            }),
        }
    }

    // 重置select和packet
    #[inline]
    fn reset(&self) {
        self.inner
            .select
            .store(Selected::Waiting.into(), Ordering::Release);
        self.inner.packet.store(ptr::null_mut(), Ordering::Release);
    }

    // 尝试一个select操作 (CAS)
    // 当失败时，之前的已经selected操作被返回
    pub fn try_select(&self, select: Selected) -> Result<(), Selected> {
        self.inner
            .select
            .compare_exchange(
                Selected::Waiting.into(),
                select.into(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|e| e.into())
    }

    // 存入一个packet
    // 这个方法必须被try_select方法成功并且提供一个packet后被调用
    #[inline]
    pub fn store_packet(&self, packet: *mut ()) {
        self.inner.packet.store(packet, Ordering::Release);
    }

    // 等待直到一个操作被select并返回它
    // 如果到了deadline，Selected::Aborted会被select
    #[inline]
    pub fn wait_until(&self, deadline: Option<Instant>) -> Selected {
        loop{
            let sel=Selected::from(self.inner.select.load(Ordering::Acquire));
            // 如果当前Selected操作不是Waiting就返回
            if sel != Selected::Waiting{
                return sel;
            }
            // deadline和无限park
            if let Some(end)=deadline{
                let now=Instant::now();
                if now<end{
                    std::thread::park_timeout(end-now);
                }else{
                    return match self.try_select(Selected::Aborted) {
                        Ok(()) => Selected::Aborted,
                        Err(s) => s,
                    };
                }
            }else{
                std::thread::park();
            }
        }
    }

    #[inline]
    pub fn unpark(&self) {
        self.inner.thread.unpark();
    }

    #[inline]
    pub fn thread_id(&self) -> usize {
        self.inner.thread_id
    }
}
