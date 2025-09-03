//! PID 实现
//!
//! 分配 PID 时，相应的内核栈也被确定。

use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE};
use crate::mm::{MapPermission, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use alloc::vec::Vec;
use lazy_static::*;

pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl RecycleAllocator {
    pub fn new() -> Self {
        RecycleAllocator {
            current: 0,
            recycled: Vec::new(),
        }
    }
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            self.current += 1;
            self.current - 1
        }
    }
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.iter().any(|i| *i == id),
            "id {} has been deallocated!",
            id
        );
        self.recycled.push(id);
    }
}


//@什么时候用 UPSafeCell？包裹了之后可以干什么？
// 1. 当需要可以修改的全局变量时。2. 可以运行时检查借用。
lazy_static! {
    static ref PID_ALLOCATOR: UPSafeCell<RecycleAllocator> =
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
    static ref KSTACK_ALLOCATOR: UPSafeCell<RecycleAllocator> = 
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
}

/// PID 结构体
pub struct PidHandle(pub usize);

impl Drop for PidHandle {
    fn drop(&mut self) {
        PID_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

/// 分配新 PID 。  
/// 全局分配进程标识符的接口。
pub fn pid_alloc() -> PidHandle {
    PidHandle(PID_ALLOCATOR.exclusive_access().alloc())
}


/// 返回 app_id 对应的内核栈范围 (bottom, top) 
/// 
/// 内核用户栈从高地址向低地址分配，每个栈之间留出一个页 PAGE_SIZE 作为保护页，为了防止栈溢出时
/// 覆盖相邻栈。
pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // 每个程序内核栈占用空间 KERNEL_STACK_SIZE + PAGE_SIZE
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    // 实际分配的空间 KERNEL_STACK_SIZE
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}


/// 用户使用的内核栈结构，内部存储的是 PID；
/// 通过 get_top 方法获得地址。
pub struct KernelStack(pub usize);

/// 分配一个新的内核栈
pub fn kstack_alloc() -> KernelStack {
    let kstack_id = KSTACK_ALLOCATOR.exclusive_access().alloc();
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    KERNEL_SPACE.exclusive_access().insert_framed_area(
        kstack_bottom.into(), 
        kstack_top.into(), 
        MapPermission::R | MapPermission::W,
    );
    KernelStack(kstack_id)
}

//? 内核除了 boot_stack 之外还有自己的栈吗？
impl Drop for KernelStack {
    /// 释放应用的内核栈
    fn drop(&mut self) {
        let (kernel_stack_bottom, _) = kernel_stack_position(self.0);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        KERNEL_SPACE
            .exclusive_access()
            .remove_area_with_start_vpn(kernel_stack_bottom_va.into());
        KSTACK_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

impl KernelStack {
    /// 向内核栈压入一个 T 类型的数据，返回该数据的原始指针
    #[allow(unused)]
    pub fn push_on_top<T>(&self, value: T) -> *mut T 
    where T: Sized,  //@ Sized 为编译时已知长度的类型。
    {
        let kernel_stack_top = self.get_top();
        let ptr_mut = (kernel_stack_top - core::mem::size_of::<T>()) as *mut T;
        unsafe {
            *ptr_mut = value;
        }
        ptr_mut
    }

    /// 获得栈顶地址
    pub fn get_top(&self) -> usize {
        let (_, kernel_stack_top) = kernel_stack_position(self.0);
        kernel_stack_top
    }
}