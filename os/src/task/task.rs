//! Types related to task management

//@ Super? 为什么
// super 是指当前模块的父模块，也就是 task/mod.rs。让我们方便取当前目录下的其他子模块，TaskContext。
use super::TaskContext;
use super::{kstack_alloc, pid_alloc, KernelStack, PidHandle};
use crate::config::TRAP_CONTEXT_BASE;
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{MemorySet, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use crate::trap::{trap_handler, TrapContext};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

/// 进程控制块（任务控制块），包含一个进程的全部信息。
pub struct TaskControlBlock {
    /// 进程索引 PID，不可变
    pub pid: PidHandle,

    /// 内核栈结构（内部存的是 usize，值为 PID）
    pub kernel_stack: KernelStack,

    /// 可变对象
    inner: UPSafeCell<TaskControlBlockInner>,
}


impl TaskControlBlock {
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn get_user_token(&self) -> usize {
        let inner = self.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    /// 优先级
    pub priority: isize,

    /// 已经“走”了多远。
    pub stride: isize,


    /// Trap 上下文所在的物理页号
    pub trap_cx_ppn: PhysPageNum,

    ///? 从 ELF 加载的应用大小，也作为用户栈的初始顶部？
    /// 应用数据可用内存空间的上限
    pub base_size: usize,

    /// 任务上下文（暂停任务的状态）
    /// 当任务被切换出去时，CPU 的通用寄存器（如 `ra`, `sp`, `s0-s11`）需要被保存起来。
    /// `TaskContext` 就是用来存放这些寄存器值的结构体。当任务被切换回来时，
    /// `__switch` 函数会从这里恢复寄存器的值，使得任务可以从上次中断的地方无缝地继续执行。
    pub task_cx: TaskContext,

    /// 任务执行状态
    /// 调度器根据这个状态来决定下一步该执行哪个任务。
    pub task_status: TaskStatus,

    /// 任务的地址空间
    pub memory_set: MemorySet,

    /// 父进程
    /// Weak 指针不会影响父进程 TaskControlBlock 的引用计数。
    pub parent: Option<Weak<TaskControlBlock>>,

    /// 所有子进程
    /// Arc （Atomically Referenced Counter）会影响子进程 TaskControlBlock 的引用
    /// 计数，当引用计数不为 0 时，资源不会释放。
    pub children: Vec<Arc<TaskControlBlock>>,

    ///? It is set when active exit or execution error occurs
    pub exit_code: i32,

    ///& 打开文件表，索引就是目标文件的文件描述符
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,

    /// 堆起始地址
    pub heap_bottom: usize,

    /// 程序中断点（Program break），未来用于实现 `sbrk`` 系统调用用于管理堆内存大小。
    pub program_brk: usize,
}

impl TaskControlBlockInner {
    /// 获取 Trap 上下文的可变引用。
    /// Trap 上下文存储在用户地址空间的一个固定页面上，通过物理页号直接访问。
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    /// 获取当前任务地址空间的 token，即页表根目录的物理地址。
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    
    /// 获取任务执行状态
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }

    /// 获取任务是否结束
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }

    /// 分配一个文件描述符（id）
    pub fn alloc_fd(&mut self) -> usize {
        // 有空的则用空的
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }
}

impl TaskControlBlock {
    /// 根据 ELF 数据创建进程，设置父子关系。
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Arc<Self> {
        // 据新的 ELF 数据获取新地址空间、新用户栈顶地址、新程序入口点
        let mut parent_inner = self.inner_exclusive_access();
        let (memory_set, user_sp, entry_point) =
            MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();

        // 创建 TCB
        let tcb = Arc::new(TaskControlBlock{
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner{
                    priority: 16,
                    stride: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    trap_cx_ppn,
                    base_size: user_sp,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)), // 弱引用父进程
                    children: Vec::new(),
                    exit_code: 0,
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                })
            },
        });
        parent_inner.children.push(tcb.clone());  // 父进程添加子进程
        
        // 设置 TrapContext
        // - 之前 from_elf 不含 TrapContext 部分的设置，但 from_existed_user 由于
        //   程序之前设置了 TrapContext，于是会被拷贝过来。
        let trap_cx = tcb.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point, 
            user_sp, 
            KERNEL_SPACE.exclusive_access().token(), 
            kernel_stack_top, 
            trap_handler as usize,
        );
        tcb
    }
    /// 从 ELF 文件创建一个新的进程
    /// 
    /// 这是进程诞生的地方，完成了从一个静态的程序二进制文件（ELF）
    /// 到一个可以在内存中运行的进程的所有初始化工作。
    ///? 具体是怎么工作的？每一步干了什么、值为什么这么设置？这个函数很重要。
    pub fn new(elf_data: &[u8]) -> Self {
        // 1. 根据 ELF 创建用户地址空间、获取进程用户栈顶地址、获取程序入口点地址。
        let (memory_set, user_sp, entry_point) = 
            MemorySet::from_elf(elf_data);

        // 2. 在新地址空间中找到预先映射好的 TrapContext 页面的物理页号
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();

        // 3. 为新进程分配进程 ID、内核栈
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();

        // 4. 将进程的 PID、内核栈、地址空间、初始状态等所有信息都聚合到一个数据结构中，方便
        //  内核后续的管理和调度
        let task_control_block = Self {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    priority: 16,
                    stride: 0,
                    trap_cx_ppn,
                    base_size: user_sp,
                    // 设置 task_cx:
                    // 巧妙的一步。一个新进程从未执行过，所以它没有一个“之前”的上下文可以恢
                    // 复。我们在这里为它伪造一个初始的任务上下文 (TaskContext)。这个上下
                    // 文被特殊设置为：当调度器第一次切换到这个任务时，CPU 的返回地址
                    // （ra 寄存器）会指向 trap_return 函数，栈指针（sp）指向内核栈顶。
                    // 这样，任务第一次运行就会直接“返回”到 trap_return，而 trap_return
                    // 的功能正是从内核态返回到用户态，从而启动整个程序。
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                })
            }
        };
        // 程序开始执行会进入 trap_return，trap_return 会执行到 __restore, __restore
        // 会从 TrapContext 地址处恢复程序，包括程序执行位置，对于要启动的程序就是程序入口点。
        // 我们需要向 TrapContext 地址写入程序的上下文信息。
        let trap_cx = 
            task_control_block.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,   // 入口点
            user_sp,       // 用户栈地址
            KERNEL_SPACE.exclusive_access().token(), 
            kernel_stack_top, 
            trap_handler as usize
        );
        task_control_block
    }

    ///? 重要函数，理解全部细节
    /// 载入一个新的 ELF 程序，替换当前任务的地址空间和执行流
    pub fn exec(&self, elf_data: &[u8]) {
        // 1. 根据新的 ELF 数据获取新地址空间、新用户栈顶地址、新程序入口点
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        
        // 2. 锁定当前 TCB，切换状态
        let mut inner = self.inner_exclusive_access();
        inner.memory_set = memory_set;  // 整个替换掉 MemorySet
        inner.trap_cx_ppn = trap_cx_ppn;  // 替换 TrapContext 物理页号
        //? ch6 取消了 inner.base_size = user_sp;
        // 3. 重新初始化 Trap 上下文，指向新程序的入口点
        let trap_cx = TrapContext::app_init_context(
            entry_point,   // 设置新入口点
            user_sp,       // 设置新栈
            KERNEL_SPACE.exclusive_access().token(), 
            self.kernel_stack.get_top(), 
            trap_handler as usize,
        );
        *inner.get_trap_cx() = trap_cx;
    }

    ///? 重要函数，理解全部细节
    /// 创建一个当前任务的子任务。
    /// 
    /// fork 会复制父任务的绝大部分数据，但会为子任务分配新 PID 和栈。
    pub fn fork(self: &Arc<TaskControlBlock>) -> Arc<TaskControlBlock> {
        let mut parent_inner = self.inner_exclusive_access();
        // 复制父进程地址空间
        let memory_set = MemorySet::from_existed_user(&parent_inner.memory_set);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();

        // 复制父进程的打开文件表         
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent_inner.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }

        // 既要将父进程的弱引用计数放到子进程的进程控制块中，
        // 又要将子进程插入到父进程的孩子向量 children 中。
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    priority: 16,
                    stride: 0,
                    trap_cx_ppn,
                    base_size: parent_inner.base_size,
                    //& 注意这里是 TaskContext 不是 TrapContext
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),  //? 父进程弱引用？
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    heap_bottom: parent_inner.heap_bottom,
                    program_brk: parent_inner.program_brk,
                })
            },
        });
        parent_inner.children.push(task_control_block.clone());

        //@ 干什么的？
        // 获取子进程 Trap 上下文的可变引用，并手动修改其中的内核栈顶指针字段，将其设置为子
        // 进程自己的新内核栈顶地址。
        // 在做这个修改之前，子进程 TrapContext 里还是父进程的内核栈栈顶地址（因为我们逐字
        // 逐句复制了父进程的地址空间，包含跳板页也就包含 TrapContext 内容）。
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top; 
        
        task_control_block
    }   

    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    pub fn change_program_brk(&self, size: i32) -> Option<usize> {
        let mut inner = self.inner_exclusive_access();
        let heap_bottom = inner.heap_bottom;
        let old_break = inner.program_brk;
        let new_brk = inner.program_brk as isize + size as isize;
        if new_brk < heap_bottom as isize {
            return None;
        }

        let result = if size < 0 {
            inner
                .memory_set
                .shrink_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        } else {
            inner
                .memory_set
                .append_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        };

        if result {
            inner.program_brk = new_brk as usize;
            Some(old_break)
        } else {
            None
        }
    }


}

/// 任务状态结构体
#[derive(Copy, Clone, PartialEq)]
// 通过 #[derive(...)] 让编译器为你的类型提供一些 Trait 的默认实现。
pub enum TaskStatus {
    /// 未初始化
    UnInit,
    /// 就绪
    Ready,
    /// 运行时
    Running,
    /// 已退出
    Zombie,
}
