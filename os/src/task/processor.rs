//! `Processor` 的实现及控制流调度
//!
//! 本模块是 CPU 调度与执行的核心。它负责：
//! 1. 维护 CPU 的当前运行状态，例如当前正在执行哪个任务。
//! 2. 实现一个核心调度循环，不断从任务管理器中获取任务并投入运行。
//! 3. 执行不同任务之间的上下文切换，即控制流的转移。


use super::__switch;
use super::{fetch_task, TaskStatus};
use super::{TaskContext, TaskControlBlock};
use crate::sync::UPSafeCell;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use lazy_static::*;

/// 管理任务在 CPU 上的状态。
pub struct Processor {
    /// 当前正在此 CPU 核心上执行的任务。
    current: Option<Arc<TaskControlBlock>>,

    /// CPU 核心的“空闲”任务上下文。
    ///
    /// 这并非一个真正的任务，而是代表了调度器 `run_tasks` 循环本身的执行上下文。
    /// 当需要从调度器切换到一个新任务时，CPU 状态从 `idle_task_cx` 切换出去。
    /// 当一个任务主动让出或执行完毕后，CPU 状态会切换回 `idle_task_cx`，
    /// 从而回到 `run_tasks` 循环中，继续寻找下一个要执行的任务。
    idle_task_cx: TaskContext,
}

impl Processor {
    /// 创建一个空的 `Processor` 实例。
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }

    /// 获取 `idle_task_cx` 的可变裸指针。之后用于保存上下文到这个地址。
    ///
    /// 返回裸指针是为了将其传递给底层的汇编函数 `__switch`，
    /// 该函数需要直接操作内存地址来进行上下文保存和恢复。
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        //@ _  下划线表示编译器自动推断类型，这里是 TaskContext
        &mut self.idle_task_cx as *mut _  
    }

    /// 取出当前任务，并将 `current` 字段置为 `None`。
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }

    /// 克隆一份当前任务的 `Arc` 引用。
    ///
    /// 这是一个非破坏性的只读操作，用于需要获取当前任务信息但又不影响其状态的场景。
    /// 调用者会得到一个新的 `Arc` 指针，增加任务的引用计数。
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
}

lazy_static!{
    // 在单核环境下，我们仅创建单个 Processor 的全局实例 PROCESSOR 
    pub static ref PROCESSOR: UPSafeCell<Processor> = unsafe { UPSafeCell::new(Processor::new()) };
}

/// 内核的主调度循环。
///
/// 该函数是一个无限循环，是操作系统的心脏。它不断地从任务管理器中获取就绪任务，
/// 然后通过 `__switch` 将 CPU 控制权交给该任务。
pub fn run_tasks() {
    loop {
        let mut processor = PROCESSOR.exclusive_access();

        // 1. 尝试从任务管理器获取一个就绪任务 task
        if let Some(task) = fetch_task() {
            // 2. 得到一个内存地址，用于稍后保存 run_tasks 循环的当前的上下文。
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();

            // 3. 获取即将运行的任务 task 内部的 task_cx 字段的内存地址。
            //  这个字段里存储着该任务上一次被暂停时，CPU 的所有寄存器状态。
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;

            // 4. 更新状态并释放锁。
            task_inner.task_status = TaskStatus::Running;
            drop(task_inner);
            processor.current = Some(task);
            drop(processor);

            // 5a. 保存当前位置（就是 __switch 之后）上下文到 idle_task_cx_ptr
            // 5b. 恢复目标任务上下文到 next_task_cx_ptr
            // 随后执行流跳转到了目标任务的代码中，从它上一次被中断的地方继续执行。之后跳回来
            // 到这里会继续执行 run_tasks 循环中，寻找并跳转下一个要执行的任务。
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
            //& <---idle_task_cx 就位于内核的这个位置
        }
        else {
            // 忙等待
            warn!("no tasks available in run_tasks");
        }
    }
}

/// 取走当前正在运行的任务。
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

/// 获取当前正在运行任务的一份克隆引用。
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().current()
}

/// 获取当前任务的地址空间 otken
pub fn current_user_token() -> usize {
    let task = current_task().unwrap();
    task.get_user_token()
}

/// 获取当前任务的 Trap 上下文的可变引用。
/// 
/// Trap 处理程序通过此函数来读取或修改用户任务被中断时保存的寄存器状态，
/// 例如，获取系统调用参数或设置返回值。
pub fn current_trap_cx() -> &'static mut TrapContext {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_trap_cx()
}

/// 让执行流返回 run_tasks。
/// 
/// 任务调度函数，由当前任务主动调用以让出 CPU。
/// 当一个任务需要暂停（例如，等待 I/O）或时间片用完时，会调用此函数。
/// CPU 控制权会从当前任务切换回调度器（`idle_task_cx`）。
pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let mut processor = PROCESSOR.exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);

    //@ 如何切换的？切换到哪里？
    // __switch 在这里的行为：
    // a. 保存：将当前任务的 CPU 寄存器状态保存到 switched_task_cx_ptr。
    // b. 恢复：将之前保存在 idle_task_cx_ptr 里的 run_tasks 的寄存器状态加载回 CPU。
    // 
    // schedule 调用 switch ，执行到 schedule 的最后一行状态保存在 
    // switched_task_cx_ptr，下一次转到该任务，就会从 schedule 这里返回，返回到当初调用
    // schedule 那个地方的下一条指令。 schedule 被 suspend_current_and_run_next、
    // exit_current_and_run_next 调用，它俩又由系统调用函数调用，系统调用退出时会进入
    // trap_return，所以会由 trap_return 返回用户程序中断处继续执行。
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}
