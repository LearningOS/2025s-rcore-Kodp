//! 实现 TaskContext
use crate::trap::trap_return;
#[repr(C)]
/// task context structure containing some registers
pub struct TaskContext {
    /// return position after task switching
    ra: usize,
    /// stack pointer
    sp: usize,
    /// S0-11 register, callee saved
    s: [usize; 12]
}

impl TaskContext {
    /// Create a new empty task context
    pub fn zero_init() -> Self {
        Self {
            ra: 0,
            sp: 0,
            s: [0; 12],
        }
    }
    /// 构造一个 TaskContext：
    /// - 返回地址 ra 为 trap_return
    /// - 栈地址为内核栈上用户 TrapContext 的地址
    /// 
    /// 使得任务调度器在第一次执行 __switch 时，
    /// 执行流能够进入 trap_return，进而完成从内核态到用户态的切换。
    /// 最终开始执行应用程序第一行代码。
    pub fn goto_trap_return(kstack_ptr: usize) -> Self {
        Self {
            ra: trap_return as usize,
            sp: kstack_ptr,
            s: [0; 12],
        }
    }
}
