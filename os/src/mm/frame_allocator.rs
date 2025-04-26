//! Implementation of [`FrameAllocator`] which
//! controls all the frames in the operating system.

use super::{PhysAddr, PhysPageNum};
use crate::config::MEMORY_END;
use crate::sync::UPSafeCell;
use alloc::vec::Vec;
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;

/// tracker for physical page frame allocation and deallocation
/// 将 PhysPageNum 包装到 FrameTracker 内。
/// 目的是，我们可以对 FrameTracker 实现 drop，来释放对应的物理页（RAII思想）！
/// 当FrameTracker离开作用域或被显式drop，其物理页会被释放。
pub struct FrameTracker {
    /// physical page number
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    /// Create a new FrameTracker
    /// 由于这个物理页帧之前可能被分配并使用过，在这里我们将这个物理页帧上的所有字节清零。
    pub fn new(ppn: PhysPageNum) -> Self {
        // page cleaning
        let bytes_array = ppn.get_bytes_array();
        for i in bytes_array {
            *i = 0;
        }
        Self { ppn }
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PPN={:#x}", self.ppn.0))
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.ppn);
    }
}

trait FrameAllocator {
    fn new() -> Self;
    fn alloc(&mut self) -> Option<PhysPageNum>;  // 分配一页
    fn dealloc(&mut self, ppn: PhysPageNum);     // 
}
/// an implementation for frame allocator
pub struct StackFrameAllocator {
    current: usize,  // 起始页号
    end: usize,      // 结束页号
    recycled: Vec<usize>,
}

impl StackFrameAllocator {
    pub fn init(&mut self, l: PhysPageNum, r: PhysPageNum) {
        self.current = l.0;
        self.end = r.0;
        // trace!("last {} Physical Frames.", self.end - self.current);
    }
}
impl FrameAllocator for StackFrameAllocator {
    fn new() -> Self {
        Self {
            current: 0,
            end: 0,
            recycled: Vec::new(),
        }
    }
    /// 分配页帧
    /// - 当分配页时，首先检查 `recycled` 栈有没有页，如果有则使用；
    /// - 否则，尝试分配 `current` 位置的页，并将 `current` 加 1。
    ///   此时尝试分配如果有 `current==end`， 则整个内存中都没有空页了，分配失败。
    fn alloc(&mut self) -> Option<PhysPageNum> {
        // 1. 优先从回收站 (recycled 栈) 中获取页帧
        if let Some(ppn) = self.recycled.pop() {
            Some(ppn.into())  // 将回收的页号包装成 PhysPageNum 返回
            // Vec::pop() 的行为类似栈的“弹出”操作，后进入的先被弹出（LIFO - Last-In, First-Out）。
            // 这是这个分配器被称为“栈”分配器的原因之一。
        } else 
        // 2. 如果回收站是空的
        if self.current == self.end {
            // 2a. 如果连续范围也分配完了 (current 指针到达 end)
            None  // 没有可用的页帧，返回 None
        } else {
            // 2b. 从连续范围中分配下一个页帧
            self.current += 1;  // current 指针向前移动一位 //! current始终递增，不会减小
            Some((self.current - 1).into())  // 返回 current 移动前指向的页号
        }
    }
    /// 回收页帧
    /// 首先检查一个页是否可以被回收。可回收的页要么不在 `recycled` 里，要么页号小于 `current`。
    ///     小于 `current` 表示不是从 `recycled` 里分配的页。
    /// 如果可以回收，放入 `recycled`。
    fn dealloc(&mut self, ppn: PhysPageNum) {
        let ppn = ppn.0;  // 获取物理页号的原始 usize 值
        // validity check (有效性检查) - 防止双重释放或释放未分配的页帧
        if ppn >= self.current 
        || self.recycled
        // 检查要释放页号是否已经在 recycled 向量中，如果存在，说明这个页帧已经被回收过一次了。
            .iter()
            .any(|&v| v == ppn) { 
            panic!("Frame ppn={:#x} has not been allocated!", ppn);
        }
        // recycle
        self.recycled.push(ppn);  // 使用 push() 方法将页号添加到 recycled 向量的末尾
    }
}

// 这里类型别名提供了一层抽象。
// 如果在未来的开发中决定使用另一种页帧分配器实现（比如基于链表或其他算法的分配器），只需要修改
// 这一行 (type FrameAllocatorImpl = AnotherAllocator;)，
type FrameAllocatorImpl = StackFrameAllocator;

// 这是 Rust 中的一个常用 crate，用于创建需要在运行时首次访问时才初始化的静态变量。
// 普通的 static 变量必须在编译时确定其值，而分配器需要在 OS 启动后才能知道可用的内存范围，
// 所以需要运行时初始化。   
lazy_static! {
    /// frame allocator instance through lazy_static!
    /// ref 关键字表示这是一个静态引用。
    pub static ref FRAME_ALLOCATOR: UPSafeCell<FrameAllocatorImpl> =
        unsafe { UPSafeCell::new(FrameAllocatorImpl::new()) };
    // UPSafeCell<T> 是一种允许内部可变性的 Cell 类型。它绕过了 Rust 常规的编译时借用检查，
    // 使得即使通过一个不可变的引用 (&UPSafeCell) 也能获取内部值的可变引用 (&mut T)。
    // UP 通常代表 "Uni-Processor" 或 "Unprotected"，暗示这种 Cell 在单处理器环境或外部
    // 同步措施（如自旋锁）保护下是“安全”的，但在多线程/多核环境中直接使用 
    // .exclusive_access() 是不安全的，因为它没有提供内在的并发控制。
}
/// initiate the frame allocator using `ekernel` and `MEMORY_END`
pub fn init_frame_allocator() {
    extern "C" {
    // extern "C" 块让 Rust 知道这个符号由外部（通常是汇编或链接器）提供，
    // 并且遵循 C 语言的调用约定（尽管这里我们只关心它的地址）。
        fn ekernel();
    }
    // 调用物理地址 PhysAddr 的 floor/ceil 方法，下/上取整获得可用的物理页号区间。
    FRAME_ALLOCATOR.exclusive_access().init(
        PhysAddr::from(ekernel as usize).ceil(),
        PhysAddr::from(MEMORY_END).floor(),
    );
}

/// Allocate a physical page frame in FrameTracker style
pub fn frame_alloc() -> Option<FrameTracker> {
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc()
        .map(FrameTracker::new)
}

/// Deallocate a physical page frame with a given ppn
pub fn frame_dealloc(ppn: PhysPageNum) {
    FRAME_ALLOCATOR.exclusive_access().dealloc(ppn);
}

#[allow(unused)]
/// a simple test for frame allocator
pub fn frame_allocator_test() {
    let mut v: Vec<FrameTracker> = Vec::new();
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    v.clear();  // 由于 FrameTracker 的 RAII 设计，这会自动调用 frame_dealloc 回收这 5 个页帧。
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    drop(v);
    println!("frame_allocator_test passed!");
}
