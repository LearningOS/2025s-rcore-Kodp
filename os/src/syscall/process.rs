//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    config::PAGE_SIZE, loader::get_app_data_by_name, mm::{page_table::PageTable, translated_refmut, translated_str, PhysAddr, VirtAddr}, task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, TASK_MANAGER,
    }, timer::get_time_us
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().pid.0
    );

    let us = get_time_us();
    // 当前任务的token
    let token = current_user_token();
    let page_table = PageTable::from_token(token);
    let va: VirtAddr = (ts as usize).into();
    let base_paddr = PhysAddr::from(
        page_table
            .find_pte(va.floor())
            .unwrap()
            .ppn()
    );
    let ts_paddr = (base_paddr.0 + va.page_offset()) as *mut TimeVal;
    unsafe {
        *ts_paddr = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        }
    }
    0
}

/// YOUR JOB: Implement mmap.
/// 
/// # ch4 中的说明：
/// 
/// 申请长度为 len 字节的物理内存（不要求实际物理内存位置，可以随便找一块），将其映射到 start 
/// 开始的虚存，内存页属性为 prot。
/// 
/// 参数：
/// - start 需要映射的虚存起始地址，要求按页对齐
/// - len 映射字节长度，可以为 0
/// - prot：第 0 位表示是否可读，第 1 位表示是否可写，第 2 位表示是否可执行。其他位无效且必须为 0
/// - 返回值：执行成功则返回 0，错误返回 -1
/// 
/// 为了简单，目标虚存区间要求按页对齐，len 可直接按页向上取整，不考虑分配失败时的页回收。
/// 
/// 可能的错误：
/// - start 没有按页大小对齐
/// - prot & !0x7 != 0 (prot 其余位必须为0)
/// - prot & 0x7 = 0 (这样的内存无意义)
/// - [start, start + len) 中存在已经被映射的页
/// - 物理内存不足
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mmap",
        current_task().unwrap().pid.0
    );
    if start % PAGE_SIZE != 0 || prot & (!0x7) != 0 || prot & 0x7 == 0 {
        return -1;
    }
    let cur = current_task().unwrap();
    let res = cur.inner_exclusive_access().memory_set.map(start, len, prot);
    drop(cur);  //? 为什么要 drop？不填 drop 编译好像报错
    res
}
/// YOUR JOB: Implement munmap.
/// 
/// # ch4 中的说明
/// 功能：取消到 [start, start + len) 虚存的映射。特别地，在 rCore 课程实验中，正确执行的
/// sys_munmap 仅会对应 唯一且完整 的 mmap 区间，不考虑交叉、截断区间的情况。  
/// 参数和返回值请参考 mmap。
///
/// 说明：为了简单，参数错误时不考虑内存的恢复和回收。
/// 
/// 可能的错误：[start, start + len) 中存在未被映射的虚存。
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munmap",
        current_task().unwrap().pid.0
    );
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    let cur = current_task().unwrap();
    let res = cur.inner_exclusive_access().memory_set.munmap(start, len);
    res
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
/// 
/// # ch5 中的说明
/// syscall ID: 400
/// 功能：新建子进程，使其执行目标程序。
/// 
/// 说明：成功返回子进程id，否则返回 -1。
/// 
/// 可能的错误：无效的文件名。
pub fn sys_spawn(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_spawn",
        current_task().unwrap().pid.0
    );
    // 可参考含有 path 的函数。
    // 参考 exec
    // 创建任务，不替换自己的地址空间；设置父子关系；加入任务队列，直接运行或等待调度执行

    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(elf_data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        let new_task = task.spawn(elf_data);
        add_task(new_task.clone());  // 加入调度器
        //? 这里可以加一个 schedule 吗？
        new_task.pid.0 as isize
    } else {
        -1 
    }
}

// YOUR JOB: Set task priority.
/// syscall ID：140
/// 设置当前进程优先级为 prio
/// 参数：prio 进程优先级，要求 prio >= 2
/// 返回值：如果输入合法则返回 prio，否则返回 -1
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority",
        current_task().unwrap().pid.0
    );
    if prio < 2 {
        return -1;
    }
    let cur = current_task().unwrap();
    cur.inner_exclusive_access().priority = prio;
    prio
}
