//! 任务管理器 `TaskManager` 的实现
//!
//! 该模块实现了一个简单的先进先出（FIFO）任务调度器 `TaskManager`。
//! 它维护一个全局唯一的就绪队列，所有待调度的任务都会被放入这个队列中。

use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;

/// 任务管理器，管理任务调度。
pub struct TaskManager {
    /// 任务控制块 `TaskControlBlock` 被 `Arc`（原子引用计数）包裹，
    /// 允许多个上下文（如任务管理器、执行器）安全地共享任务的所有权
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }

    /// 将一个任务加入队尾
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }

    /// 从队头取出一个任务
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        // self.ready_queue.pop_front()
        
        let mut min_stride = isize::MAX;
        let mut res_idx: Option<usize> = None;
        for (idx, task) in self.ready_queue.iter().enumerate() {
            let stride = task.inner_exclusive_access().stride;
            if stride < min_stride {
                min_stride = stride;
                res_idx = Some(idx);
            }
        }

        if let Some(idx) = res_idx {
            self.ready_queue.remove(idx)
        }
        else {
            None
        }
    }
}

lazy_static! {
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// 一个全局接口，用于向任务管理器中添加新任务。
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// 一个全局接口，用于从任务管理器中获取下一个要运行的任务。
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
