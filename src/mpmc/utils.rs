use std::{cell::Cell, ops::{Deref, DerefMut}};

// 这是对Channel内部值的一个抽象，它做了缓存行填充的优化
// list和array channel都会用到这个缓存行优化

// 将一个值填充并对齐到一个缓存行的长度 注意x86_64位架构，目前缓存行对齐128字节

//Cache块 
#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]
#[cfg_attr(
    any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "powerpc64",),
    repr(align(128))
)]
pub struct CachePadded<T>{
    value:T
}

impl<T> CachePadded<T>{
    //填充并对齐一个值到一个缓存块的长度
    pub fn new(value:T)->CachePadded<T>{
        CachePadded::<T> { value }
    }
}

impl<T> Deref for CachePadded<T>{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T>{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

const SPIN_LIMIT: u32 = 6;

// 在自选环路中执行二次退避
pub struct Backoff{
    step:Cell<u32>
}

impl Backoff {

    pub fn new()->Self{
        Backoff { step: Cell::new(0) }
    }

    pub fn spin_light(&self){
        let step=self.step.get().min(SPIN_LIMIT);
        for _ in 0..step.pow(2){
            std::hint::spin_loop();
        }
        self.step.set(self.step.get()+1);
    }

    pub fn spin_heavy(&self){
        if self.step.get()<=SPIN_LIMIT{
            for _ in 0..self.step.get().pow(2){
                std::hint::spin_loop();
            }
        }else{
            std::thread::yield_now();
        }
        self.step.set(self.step.get()+1);
    }

    pub fn is_completed(&self)->bool{
        self.step.get() > SPIN_LIMIT
    }

}