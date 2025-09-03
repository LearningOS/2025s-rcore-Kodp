//! 内存管理实现
//! 
//! RV64 SV39 分页虚拟内存系统。所有内存管理的单元，比如页分配器，页表，内存映射等，
//! 都在这里实现。
//! 
//! 每个任务或进程都有一个 memory_set 管理其虚拟内存。
pub mod address;
mod frame_allocator;
mod heap_allocator;
pub mod memory_set;
pub mod page_table;


//? 系统学习一下 rust 工程管理 
//? mod.rs 是干嘛的？ pub use 这些是为了暴露库吗？ mod 对其下所有子的权限是什么？
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum, VPNRange};
pub use frame_allocator::{frame_alloc, frame_dealloc, FrameTracker};
pub use memory_set::remap_test;
pub use memory_set::{ kernel_token, MapPermission, MemorySet, KERNEL_SPACE};
pub use page_table::{
    translated_byte_buffer, translated_ref, translated_refmut, translated_str, 
    PageTable, PageTableEntry, UserBuffer, UserBufferIterator, PTEFlags
};

/// 初始化内存模块。
pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    KERNEL_SPACE.exclusive_access().activate();  // 激活虚拟地址空间
}
