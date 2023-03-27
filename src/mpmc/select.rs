// 当前的数据(在阻塞操作期间被初始化)会被read和write消耗

// 每个域包含一个与指定channel flavor关联的数据

// 三种缓冲区，ArrayToken是数组队列有界缓冲区，ListToken是链队列无界缓冲区，ZeroToken无缓冲区
#[derive(Debug, Default)]
pub struct Token {
    pub(crate) array: super::array::ArrayToken,
    pub(crate) list: super::list::ListToken,
    pub(crate) zero: super::zero::ZeroToken,
}

// 代表与一个指定的线程在指定的channel上相关联的操作的id
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation(usize);

impl Operation {
    // 从一个可变的引用创建一个Operation标识
    // 这个函数的本质是将引用变为一个数字，该引用指向一个特定的线程和操作的变量
    // 在阻塞过程中，这个引用是一直有效的
    #[inline]
    pub fn hook<T>(r: &mut T) -> Operation {
        let val = r as *mut T as usize;
        assert!(val > 2);
        Operation(val)
    }
}

// 当前阻塞操作的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selected {
    // 等待一个操作 0
    Waiting,
    // 尝试阻塞已经终止的线程 1
    Aborted,
    // 一个变成ready的操作因为channel断开了连接(r,s) 2
    Disconnected,
    // 一个操作变成ready因为一个消息可以被发送或接收 x
    Operation(Operation),
}

impl From<usize> for Selected {
    #[inline]
    fn from(val: usize) -> Self {
        match val {
            0 => Selected::Waiting,
            1 => Selected::Aborted,
            2 => Selected::Disconnected,
            oper => Selected::Operation(Operation(oper)),
        }
    }
}

impl Into<usize> for Selected {
    #[inline]
    fn into(self) -> usize {
        match self {
            Selected::Waiting => 0,
            Selected::Aborted => 1,
            Selected::Disconnected => 2,
            Selected::Operation(Operation(val)) => val,
        }
    }
}
