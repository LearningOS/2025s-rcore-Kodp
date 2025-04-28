//! Process management syscalls
use crate::{config::PAGE_SIZE, mm::{app_vaddr_to_paddr,app_vaddr_to_paddr_prot}, task::{change_program_brk, exit_current_and_run_next, get_syscall_times, suspend_current_and_run_next, TASK_MANAGER}, timer::get_time_us};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}



///TODO: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
/// tz时区，不管
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let token = TASK_MANAGER.get_current_token();
    let ts_paddr = app_vaddr_to_paddr(token, ts as *const u8, ).unwrap() as *mut TimeVal;
    unsafe {
        *ts_paddr = TimeVal{
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

///TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
/// 这个系统调用有三种功能，根据 trace_request 的值不同，执行不同的操作：
///     trace_request==0，则 id 应被视作 *const u8 ，读取当前任务 id 地址处一个字节的无符号整数值。此时应忽略 data 参数。返回值为 id 地址处的值。
///     trace_request==1，则 id 应被视作 *mut u8 ，写入 data （作为 u8，即只考虑最低位的一个字节）到该用户程序 id 地址处。返回值应为0。
///     trace_request==2，表示查询当前任务调用编号为 id 的系统调用的次数，返回值为这个调用次数。本次调用也计入统计。
/// 在读取（trace_request 为 0）时，如果对应地址用户不可见或不可读，则返回值应为 -1（isize 格式的 -1，而非 u8）。
/// 在写入（trace_request 为 1）时，如果对应地址用户不可见或不可写，则返回值应为 -1（isize 格式的 -1，而非 u8）。
/// 否则，忽略其他参数，返回值为 -1。
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    let token = TASK_MANAGER.get_current_token();
    let mut prot = 0;
    if trace_request == 0 {
        prot = 1 << 1;
    }
    else if trace_request == 1 {
        prot = 1 << 2;
    }
    let res = app_vaddr_to_paddr_prot(token, id as *const u8, prot); 
    match trace_request {
        0 => {
            if let Some(paddr) = res {
                unsafe { *(paddr as *const u8) as isize } 
            } else {
                -1 as isize
            }
        }
        1 => {
            if let Some(paddr) = res {
                unsafe { *(paddr as *mut u8) = data as u8; 0 }
            }
            else {
                -1 as isize
            }
        }
        2 => {
            let syscall_times =  get_syscall_times();
            syscall_times[id] as isize
        } 
        _ => -1,
    }
}


/// syscall ID：222
/// 申请长度为 len 字节的物理内存（不要求实际物理内存位置，可以随便找一块），
/// 将其映射到 start 开始的虚存，内存页属性为 prot。
///     start 需要映射的虚存起始地址，要求按页对齐
///     len 映射字节长度，可以为 0
///     prot：第 0 位表示是否可读，第 1 位表示是否可写，第 2 位表示是否可执行。其他位无效且必须为 0
/// 为了简单，目标虚存区间要求按页对齐，len 可直接按页向上取整，不考虑分配失败时的页回收。
/// 可能的错误：
///     1. start 没有按页大小对齐
///     2. prot & !0x7 != 0 (prot 其余位必须为0)
///     3. prot & 0x7 == 0 (这样的内存无意义)
///     4. [start, start + len) 中存在已经被映射的页
///     5. 物理内存不足 ？
/// 返回值：执行成功则返回 0，错误返回 -1
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    if (start % PAGE_SIZE != 0) || (prot & (!0x7) != 0) || (prot & 0x7 == 0) {
        return -1;
    }
    let mut task_manager_inner  = TASK_MANAGER.inner.exclusive_access();
    let cur = task_manager_inner.current_task;

    let res =  task_manager_inner.tasks[cur].memory_set
        .map(start, len, prot);
    res
}

/// syscall ID：215
/// 取消到 [start, start + len) 虚存的映射
/// 参数和返回值请参考 mmap
/// 说明：
/// 为了简单，参数错误时不考虑内存的恢复和回收。
/// 可能的错误：
/// [start, start + len) 中存在未被映射的虚存。
pub fn sys_munmap(start: usize, len: usize) -> isize {
    println!("#### sys_munmap ####");
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    let mut task_manager_inner  = TASK_MANAGER.inner.exclusive_access();
    let cur = task_manager_inner.current_task;

    let res =   task_manager_inner.tasks[cur].memory_set.munmap(start, len);
    res
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
