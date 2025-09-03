//! 任务管理实现
//! 
//! 包含所有与任务管理相关的实现，例如启动或切换任务。
//! TaskManager 的一个全局实例管理操作系统所有的任务。 
//! 要小心注意 __switch 附近的控制流。 __switch 实际上是汇编函数。
//! 
//! - **内部可变性 `UPSafeCell`**:
//!   `TaskManager` 内部包含一个 `UPSafeCell<TaskManagerInner>`。
//!   这是一种实现“内部可变性”的常见模式。在 Rust 中，全局静态变量默认是不可变的。
//!   但任务管理器在运行时需要频繁修改其内部状态（如当前运行的任务、任务列表等）。
//!   `UPSafeCell` 允许我们在拥有一个不可变引用（`&TaskManager`）的情况下，
//!   安全地获取其内部数据的可变访问权限。这在单核（Uni-Processor）环境下是安全的，
//!   因为它能保证在任何时刻只有一个地方在修改数据，从而避免了数据竞争。
//!
//! - **任务切换 `__switch`**:
//!   任务切换的核心逻辑由汇编函数 `__switch` (位于 `switch.S` 文件) 实现。
//!   这是一个非常关键且技巧性很强的部分。它负责：
//!     1. **保存当前任务的上下文**: 将当前任务的所有通用寄存器、栈指针等 CPU 状态保存到
//!        当前任务的 `TaskContext` 结构体中。
//!     2. **恢复下一个任务的上下文**: 从下一个待运行任务的 `TaskContext` 结构体中加载
//!        其之前保存的 CPU 状态到寄存器中。
//!     3. **切换控制流**: 当 `__switch` 函数返回时，它已经不在原来的调用栈上了，
//!        而是返回到了下一;个任务上次被中断的地方。这就是任务切换的“魔术”所在。
//!   **注意**: 理解围绕 `__switch` 的代码时要特别小心，因为它的执行流并非传统的函数调用和返回。

mod context;
mod id;
mod manager;
mod processor;
mod switch;

//@ 什么玩意儿？干嘛的？
// Clippy 是 rust 提供的一个静态代码分析工具。
// module_inception 是一个检查项，当你的目录结构和模块声明出现 a/mod.rs 文件中包含 mod a; 
// 这样的情况时，Clippy 会发出这个警告，因为它认为这是一种“模块套娃”式的结构，可能会引起混淆。
// 在这个本项目中存在一个 task 目录，其中包含一个 mod.rs 文件，而这个 mod.rs 里又声明了 mod
// task;（去加载同目录下的 task.rs 文件）。
//
// #[allow(...)]: 这是一个属性（attribute），用来告诉编译器或工具（这里是 Clippy）
// “我知道这里有一个警告，但这是我故意这么做的，请忽略它”。
//? 为什么要写一句 mod task?
#[allow(clippy::module_inception)] 
#[allow(rustdoc::private_intra_doc_links)] //?
mod task;

use crate::fs::{open_file, OpenFlags};
use alloc::sync::Arc;
pub use context::TaskContext;
use lazy_static::*;
pub use manager::{fetch_task, TaskManager};
use switch::__switch;
pub use task::{TaskControlBlock, TaskStatus};

pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle};
pub use manager::add_task;
pub use processor::{
    current_task, current_trap_cx, current_user_token, run_tasks, schedule, take_current_task,
    Processor,
};

/// 暂停当前任务并执行下一个任务。
/// 当前任务自愿放弃 CPU 使用权。
pub fn suspend_current_and_run_next() {
     // 1. 从 CPU 执行单元中取走当前任务的所有权。
    let task = take_current_task().unwrap();

    // 2. 将其状态从 `Running` 修改为 `Ready`。
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);

    // 3. 将任务重新放回就绪队列的末尾，等待下一次被调度。
    add_task(task);
    // 4. 调用调度器，切换到调度循环。
    schedule(task_cx_ptr);
}

/// pid of usertests app in make run TEST=1
pub const IDLE_PID: usize = 0;

/// 结束当前运行的任务并运行下一个
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().unwrap();
    let pid = task.getpid();

    // 特殊处理：如果退出的是空闲进程，表示所有应用都已执行完毕，引发 panic。
    if pid == IDLE_PID {
        println!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code,
        );
        panic!("All applications completed!");
    }

    // 锁定 TCB
    let mut inner = task.inner_exclusive_access();
    inner.task_status = TaskStatus::Zombie;
    inner.exit_code = exit_code;

    //?
    // 如果一个进程先于它的子进程结束，那么该进程就无法call 系统调用回收子进程；
    // 为了处理这种情况，我们在进程结束时，把它的所有子进程作为用户初始进程（initproc）的子进程，
    // 同时设置这些子进程的父进程为用户初始进程
    {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        // 遍历当前进程的子进程
        for child in inner.children.iter() {
            // 更新子进程的父进程为 init 进程
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            // 将子进程全部加入 init 进程子进程列表！
            initproc_inner.children.push(child.clone());
        }
    }

    // 4. 清理自身资源。
    inner.children.clear();
    // 先回收用户地址空间的所有数据页帧。虽然父进程删除子进程的 TaskControlBlock 时资源
    // 都会连锁回收，但到这个时间点还有一段时间，不回收会使得用户数据物理页帧闲置，利用率不高。
    inner.memory_set.recycle_data_pages(); 
    // 释放文件描述符
    inner.fd_table.clear();
    drop(inner);
    drop(task);

    // 5. 执行调度，切换到下一个任务。
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    /// 全局唯一的初始进程 `INITPROC`。
    pub static ref INITPROC: Arc<TaskControlBlock> = Arc::new({
        let inode = open_file("ch6b_initproc", OpenFlags::RDONLY).unwrap();
        let v = inode.read_all();
        TaskControlBlock::new(v.as_slice())
    });
}

/// 将初始进程 `INITPROC` 加入到任务队列中。
pub fn add_initproc() {
    add_task(INITPROC.clone());
}