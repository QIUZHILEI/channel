use std::fmt;

// send_timeout方法可能会返回一个Error，就是这个Error
// 错误会被包含在信息中，因此它能够被恢复
#[allow(dead_code)]
#[derive(PartialEq, Eq,Clone, Copy)]
pub enum SendTimeoutError<T> {
    // 这个错误是指：信息不能被发送，因为channel满了并且操作超时
    // 如果这是个zero channel，表示没有接收者，也会出现这个错误
    Timeout(T),
    // channel已经关闭因此信息不能被发送
    Disconnected(T)
}

impl<T> fmt::Debug for SendTimeoutError<T>{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "SendTimeoutError(..)".fmt(f)
    }
}

impl<T> fmt::Display for SendTimeoutError<T>{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SendTimeoutError::Timeout(..) => "timed out waiting on send operation".fmt(f),
            SendTimeoutError::Disconnected(..)=>"sending on a disconnected channel".fmt(f)
        }
    }
}

// 在Sender、SyncSender的send方法可能会出现这个错误
// SendError仅在接收端在Disconnected时才会发生
// 错误会包裹原始信息，这个信息可以被获取以便于恢复
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct SendError<T>(pub T);

// 在Receiver的recv方法中可能会产生这个错误
// RecvError，当recv接受msg时，sender传送msg到一半而channel(include sync_channel)关闭了就会产生这个错误
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct RecvError;

// 这个枚举类型中的错误可能会发生在channel和sync_channel
#[allow(dead_code)]
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

// 接收超时
#[allow(dead_code)]
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

// try_send错误
#[allow(dead_code)]
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}