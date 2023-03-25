use std::mem::MaybeUninit;

pub mod mpmc;


fn test(){
    let mut x = MaybeUninit::<&i32>::uninit();
}