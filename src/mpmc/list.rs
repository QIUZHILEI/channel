use std::{cell::UnsafeCell, mem::MaybeUninit, sync::atomic::{AtomicUsize, AtomicPtr, Ordering, self}, marker::PhantomData, time::Instant, ptr};

use super::{utils::CachePadded, context::*, utils::*, waker::SyncWaker, select::*, errors::*};

/*
 * 这三个位表示slot的状态：
 * 如果msg已经被写入slot，WRITE状态被设置
 * 如果msg已经从slot被读出来，READ状态被设置
 * 如果block被销毁，DESTROY状态被设置
 */
const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;

//
const LAP: usize = 32;
// 一个msg能持有的最大Block
const BLOCK_CAP: usize = LAP - 1;
// 用于右移操作，代表为元数据的低位保留多少位
const SHIFT: usize = 1;
/*
 * MARK_BIT有两种不同的目的：
 * 如果被设置在head，说明这个块不是最后一个
 * 如果被设置在tail，表示channel已经断开
 */
const MARK_BIT: usize = 1;

// 一个Block块中的Slot
struct Slot<T> {
    // msg
    msg: UnsafeCell<MaybeUninit<T>>,
    // slot状态(WRITE/READ/DESTROY)
    state: AtomicUsize,
}

impl<T> Slot<T> {
    // 自旋直到msg被写入slot
    fn wait_write(&self) {
        let backoff=Backoff::new();
        while self.state.load(Ordering::Acquire)&WRITE==0 {
            backoff.spin_heavy();
        }
    }
}

/*
 * block是list的一个节点
 * 每个block中的信息msg被组合成一个slots:[Slot[T],BLOCK_CAP]数组,这种连续的分配不仅可以
 * 内存分配的效率，还可以发挥缓存的性能
 */
struct Block<T> {
    next: AtomicPtr<Block<T>>,
    slots: [Slot<T>; BLOCK_CAP],
}


impl<T> Block<T> {
    fn new() -> Block<T> {
        // 这里使用MaybeUninit是安全的
        // Block::next 可以零初始化
        // Block::slots 数组可以零初始化
        // Slot::msg 内部是UnsafeCell持有MaybeUninit域，可以零初始化
        // Slot::state AtomicUsize可以零初始化
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    /// 等待next指针域被设置
    fn wait_next(&self) -> *mut Block<T> {
        let backoff = Backoff::new();
        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            backoff.spin_heavy();
        }
    }

    //将Slot中的state位设置为DESTROY，最终销毁这个Block
    unsafe fn destroy(this: *mut Block<T>, start: usize) {
        // i 从start到(BLOCK_CAP-1)，不包括BLOCK-1位置的Slot，因为该slot已经开始销毁该块
        for i in start..BLOCK_CAP - 1 {
            let slot = (*this).slots.get_unchecked(i);

            // 如果还有线程使用这个slot，就标记为DESTROY
            if slot.state.load(Ordering::Acquire) & READ == 0
                && slot.state.fetch_or(DESTROY, Ordering::AcqRel) & READ == 0
            {
                // 如果一个线程正在使用这个slot，那个线程将会继续销毁该块，因此我们应该返回
                return;
            }
        }

        // 没有线程使用这个块就可以安全的销毁
        drop(Box::from_raw(this));
    }
}

// 在channel中的一个消息实体所在的位置
#[derive(Debug)]
struct Position<T> {
    // 在channel中的索引(主要记录head和tail的位置)
    index: AtomicUsize,
    // block承载msg实体
    block: AtomicPtr<Block<T>>,
}

// list flavor channel，记录msg在哪一个Block中，Block中的slots数组中的偏移量是多少
#[derive(Debug)]
pub(crate) struct ListToken {
    // slot的block，这里代表的是Block指针的指针
    block: *const u8,
    // 在block中的偏移量，offset的计算方式导致它的取值范围为[0,32)
    offset: usize,
}

impl Default for ListToken {
    #[inline]
    fn default() -> Self {
        ListToken { block: ptr::null(), offset: 0 }
    }
}

// 无界Channel：每个发送到这个channel的msg都被赋予一个索引(usize)
pub(crate) struct Channel<T> {
    // channel 头
    head: CachePadded<Position<T>>,
    // channel 尾，tail的索引为2，4，6，8..偶数
    tail: CachePadded<Position<T>>,
    // 当channel为空或者没有被断开时，Receivers会阻塞，这个SyncWaker就记录阻塞
    receivers: SyncWaker,
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    pub(crate) fn new() -> Self {
        Channel {
            head: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            receivers: SyncWaker::new(),
            _marker: PhantomData,
        }
    }

    /*
     * 向channel发送msg前，要调整Block中的slot，如果一个Block
     * 中有可以使用的空间则只需调整tail索引，如果没有可用空间，则需新建block
     */
    fn start_send(&self, token: &mut Token) -> bool {
        let backoff = Backoff::new();
        let mut tail = self.tail.index.load(Ordering::Acquire);
        let mut block = self.tail.block.load(Ordering::Acquire);
        let mut next_block = None;

        loop {
            // tail的index是奇数时这个条件为真
            if tail & MARK_BIT != 0 {
                token.list.block = ptr::null();
                return true;
            }

            // 计算进入block的index的偏移量
            let offset = (tail >> SHIFT) % LAP;

            // 如果offset==Block_CAP(31)时，这个block已经满了，也证明下一个Block正在被创建，要等待下一个Block被创建
            if offset == BLOCK_CAP {
                backoff.spin_heavy();
                tail = self.tail.index.load(Ordering::Acquire);
                block = self.tail.block.load(Ordering::Acquire);
                continue;
            }

            // 如果我们需要(offset=30)新创建一个Block就提前分配它，以便使其他线程的等待时间尽可能的短
            if offset + 1 == BLOCK_CAP && next_block.is_none() {
                next_block = Some(Box::new(Block::<T>::new()));
            }

            // 如果是第一个被发送至channel的msg，需要新创建一个block
            if block.is_null() {
                let new = Box::into_raw(Box::new(Block::<T>::new()));
                if self
                    .tail
                    .block
                    .compare_exchange(block, new, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    self.head.block.store(new, Ordering::Release);
                    block = new;
                } else {
                    next_block = unsafe { Some(Box::from_raw(new)) };
                    tail = self.tail.index.load(Ordering::Acquire);
                    block = self.tail.block.load(Ordering::Acquire);
                    continue;
                }
            }
            // index都是偶数
            let new_tail = tail + (1 << SHIFT);

            // 更新tail的index
            match self.tail.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // recheck：如果此时offset为30，应该扩容
                    if offset + 1 == BLOCK_CAP {
                        let next_block = Box::into_raw(next_block.unwrap());
                        self.tail.block.store(next_block, Ordering::Release);
                        self.tail.index.fetch_add(1 << SHIFT, Ordering::Release);
                        (*block).next.store(next_block, Ordering::Release);
                    }
                    token.list.block = block as *const u8;
                    token.list.offset = offset;
                    return true;
                },
                Err(_) => {
                    // TODO(这里为什么要自旋)
                    backoff.spin_light();
                    tail = self.tail.index.load(Ordering::Acquire);
                    block = self.tail.block.load(Ordering::Acquire);
                }
            }
        }
    }

    // 将msg写入channel
    pub(crate) unsafe fn write(&self, token: &mut Token, msg: T) -> Result<(), T> {
        // 如果list中没有slot那么代表channel已经disconnected
        if token.list.block.is_null() {
            return Err(msg);
        }

        // 写入slot
        let block = token.list.block as *mut Block<T>;
        let offset = token.list.offset;
        let slot = (*block).slots.get_unchecked(offset);
        slot.msg.get().write(MaybeUninit::new(msg));
        // slot中的msg已经被写入
        slot.state.fetch_or(WRITE, Ordering::Release);
        // 唤醒一个等待的receiver
        self.receivers.notify();
        Ok(())
    }
    // 尝试发送一个msg到channel
    pub(crate) fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.send(msg, None).map_err(|err| match err {
            SendTimeoutError::Disconnected(msg) => TrySendError::Disconnected(msg),
            SendTimeoutError::Timeout(_) => unreachable!(),
        })
    }

    // 发送一个msg到channel
    pub(crate) fn send(
        &self,
        msg: T,
        _deadline: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        let token = &mut Token::default();
        assert!(self.start_send(token));
        unsafe { self.write(token, msg).map_err(SendTimeoutError::Disconnected) }
    }

    // 尝试为接收信息保留一个slot. 这里会选择性的更新head block index，如果head和tail不在一个Block中，head会被设置为奇数
    fn start_recv(&self, token: &mut Token) -> bool {
        let backoff = Backoff::new();
        let mut head = self.head.index.load(Ordering::Acquire);
        let mut block = self.head.block.load(Ordering::Acquire);

        loop {
            // 头部偏移也需要计算
            let offset = (head >> SHIFT) % LAP;

            // 如果offset为BLOCK_CAP说明到达这个block的末尾，应该等待其他
            if offset == BLOCK_CAP {
                backoff.spin_heavy();
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }

            let mut new_head = head + (1 << SHIFT);

            if new_head & MARK_BIT == 0 {
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Relaxed);

                // 头尾索引相同代表channel为空没有msg
                if head >> SHIFT == tail >> SHIFT {
                    // 如果channel已经断开
                    if tail & MARK_BIT != 0 {
                        // 然后将block置空，并返回一个错误
                        token.list.block = ptr::null();
                        return true;
                    } else {
                        // 否则就是接收操作未就绪
                        return false;
                    }
                }

                // 如果head和tail不在同一个Block中就将head设置为奇数(MARK_BIT)
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= MARK_BIT;
                }
            }

            // 只有第一个msg进入channel时block会为null，这时需要等待write msg into channel
            if block.is_null() {
                backoff.spin_heavy();
                head = self.head.index.load(Ordering::Acquire);
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }

            // 更新head index
            match self.head.index.compare_exchange_weak(
                head,
                new_head,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, move to the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next = (*block).wait_next();
                        let mut next_index = (new_head & !MARK_BIT).wrapping_add(1 << SHIFT);
                        if !(*next).next.load(Ordering::Relaxed).is_null() {
                            next_index |= MARK_BIT;
                        }

                        self.head.block.store(next, Ordering::Release);
                        self.head.index.store(next_index, Ordering::Release);
                    }

                    token.list.block = block as *const u8;
                    token.list.offset = offset;
                    return true;
                },
                Err(_) => {
                    backoff.spin_light();
                    head = self.head.index.load(Ordering::Acquire);
                    block = self.head.block.load(Ordering::Acquire);
                }
            }
        }
    }
    /// Reads a message from the channel.
    pub(crate) unsafe fn read(&self, token: &mut Token) -> Result<T, ()> {
        if token.list.block.is_null() {
            // 如果channel已经关闭就返回Err
            return Err(());
        }

        // Read the message.
        let block = token.list.block as *mut Block<T>;
        let offset = token.list.offset;
        let slot = (*block).slots.get_unchecked(offset);
        slot.wait_write();
        let msg = slot.msg.get().read().assume_init();

        // Destroy the block if we've reached the end, or if another thread wanted to destroy but
        // couldn't because we were busy reading from the slot.
        if offset + 1 == BLOCK_CAP {
            Block::destroy(block, 0);
        } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
            Block::destroy(block, offset + 1);
        }

        Ok(msg)
    }

    // 尝试接收一个msg(non-blocking)
    pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
        let token = &mut Token::default();

        if self.start_recv(token) {
            unsafe { self.read(token).map_err(|_| TryRecvError::Disconnected) }
        } else {
            Err(TryRecvError::Empty)
        }
    }

    // 接收一个msg
    pub(crate) fn recv(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let token = &mut Token::default();
        loop {
            if self.start_recv(token) {
                unsafe {
                    return self.read(token).map_err(|_| RecvTimeoutError::Disconnected);
                }
            }

            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return Err(RecvTimeoutError::Timeout);
                }
            }

            // Prepare for blocking until a sender wakes us up.
            Context::with(|cx| {
                let oper = Operation::hook(token);
                self.receivers.register(oper, cx);

                // Has the channel become ready just now?
                if !self.is_empty() || self.is_disconnected() {
                    let _ = cx.try_select(Selected::Aborted);
                }

                // Block the current thread.
                let sel = cx.wait_until(deadline);

                match sel {
                    Selected::Waiting => unreachable!(),
                    Selected::Aborted | Selected::Disconnected => {
                        self.receivers.unregister(oper).unwrap();
                        // If the channel was disconnected, we still have to check for remaining
                        // messages.
                    }
                    Selected::Operation(_) => {}
                }
            });
        }
    }

    /// Returns the current number of messages inside the channel.
    pub(crate) fn len(&self) -> usize {
        loop {
            // Load the tail index, then load the head index.
            let mut tail = self.tail.index.load(Ordering::SeqCst);
            let mut head = self.head.index.load(Ordering::SeqCst);

            // If the tail index didn't change, we've got consistent indices to work with.
            if self.tail.index.load(Ordering::SeqCst) == tail {
                // Erase the lower bits.
                tail &= !((1 << SHIFT) - 1);
                head &= !((1 << SHIFT) - 1);

                // Fix up indices if they fall onto block ends.
                if (tail >> SHIFT) & (LAP - 1) == LAP - 1 {
                    tail = tail.wrapping_add(1 << SHIFT);
                }
                if (head >> SHIFT) & (LAP - 1) == LAP - 1 {
                    head = head.wrapping_add(1 << SHIFT);
                }

                // Rotate indices so that head falls into the first block.
                let lap = (head >> SHIFT) / LAP;
                tail = tail.wrapping_sub((lap * LAP) << SHIFT);
                head = head.wrapping_sub((lap * LAP) << SHIFT);

                // Remove the lower bits.
                tail >>= SHIFT;
                head >>= SHIFT;

                // Return the difference minus the number of blocks between tail and head.
                return tail - head - tail / LAP;
            }
        }
    }

    pub(crate) fn capacity(&self) -> Option<usize> {
        None
    }

    /*
     * 断开senders并唤醒所有的receivers
     * 如果操作成功就返回true
     * 首先的操作就是将tail的索引置为奇数
     * 如果原来tail的索引就是奇数，说明已经disconnected，那么这个调用就会没有效果返回false
     */
    pub(crate) fn disconnect_senders(&self) -> bool {
        let tail = self.tail.index.fetch_or(MARK_BIT, Ordering::SeqCst);
        if tail & MARK_BIT == 0 {
            self.receivers.disconnect();
            true
        } else {
            false
        }
    }

    /*
     * 断开receivers，如果原来tail的索引就是奇数，说明已经disconnected
     * 此时应该丢弃所有消息并释放内存
    */
    pub(crate) fn disconnect_receivers(&self) -> bool {
        let tail = self.tail.index.fetch_or(MARK_BIT, Ordering::SeqCst);

        if tail & MARK_BIT == 0 {
            // If receivers are dropped first, discard all messages to free
            // memory eagerly.
            self.discard_all_messages();
            true
        } else {
            false
        }
    }

    // 丢弃所有的msg，这个方法应该仅在所有的receivers被删除时才调用
    fn discard_all_messages(&self) {
        let backoff = Backoff::new();
        let mut tail = self.tail.index.load(Ordering::Acquire);
        loop {
            let offset = (tail >> SHIFT) % LAP;
            if offset != BLOCK_CAP {
                break;
            }

            // New updates to tail will be rejected by MARK_BIT and aborted unless it's
            // at boundary. We need to wait for the updates take affect otherwise there
            // can be memory leaks.
            backoff.spin_heavy();
            tail = self.tail.index.load(Ordering::Acquire);
        }

        let mut head = self.head.index.load(Ordering::Acquire);
        let mut block = self.head.block.load(Ordering::Acquire);

        unsafe {
            // Drop all messages between head and tail and deallocate the heap-allocated blocks.
            while head >> SHIFT != tail >> SHIFT {
                let offset = (head >> SHIFT) % LAP;

                if offset < BLOCK_CAP {
                    // Drop the message in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.wait_write();
                    let p = &mut *slot.msg.get();
                    p.as_mut_ptr().drop_in_place();
                } else {
                    (*block).wait_next();
                    // Deallocate the block and move to the next one.
                    let next = (*block).next.load(Ordering::Acquire);
                    drop(Box::from_raw(block));
                    block = next;
                }

                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
        head &= !MARK_BIT;
        self.head.block.store(ptr::null_mut(), Ordering::Release);
        self.head.index.store(head, Ordering::Release);
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.tail.index.load(Ordering::SeqCst) & MARK_BIT != 0
    }

    pub(crate) fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }

    pub(crate) fn is_full(&self) -> bool {
        false
    }
}


impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        let mut head = self.head.index.load(Ordering::Relaxed);
        let mut tail = self.tail.index.load(Ordering::Relaxed);
        let mut block = self.head.block.load(Ordering::Relaxed);

        // Erase the lower bits.
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all messages between head and tail and deallocate the heap-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;

                if offset < BLOCK_CAP {
                    // Drop the message in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    let p = &mut *slot.msg.get();
                    p.as_mut_ptr().drop_in_place();
                } else {
                    // Deallocate the block and move to the next one.
                    let next = (*block).next.load(Ordering::Relaxed);
                    drop(Box::from_raw(block));
                    block = next;
                }

                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}
