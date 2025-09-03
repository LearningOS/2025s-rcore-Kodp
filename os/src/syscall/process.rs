//! 包含任务处理相关的 syscall
use alloc::sync::Arc;

use crate::{
    config::PAGE_SIZE,
    fs::{open_file, OpenFlags},
    mm::{
        translated_refmut, translated_str, PageTable,
        PhysAddr, VirtAddr,
    },
    task::{ 
        add_task, current_task, current_user_token, exit_current_and_run_next, 
        suspend_current_and_run_next, 
    }, 
    timer::get_time_us, 
};


#[repr(C)]  //@ 按照 C 语言的布局
#[derive(Debug)] //@ 自动推导 Debug trait，让打印函数可以直接打印该结构体。
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// APP 结束并提交退出码
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// 应用主动交出 CPU 所有权并切换到其他应用。
/// syscall ID：124
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

/// 创建一个与当前任务几乎完全相同的子任务。
/// 
/// `fork` 被调用一次，返回两次：
/// - 在父进程中，返回新创建的子进程的 PID。
/// - 在子进程中，返回 0。
/// syscall ID：220
pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();

    // 1. 调用 TCB 的 `fork` 方法。这是核心步骤，它会：
    //    - 完整复制父进程的地址空间。
    //    - 为子进程分配新的 PID 和内核栈。
    //    - 创建并初始化子进程的 TCB。
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;

    // 2. 修改子进程的 Trap 上下文，这是 fork 返回两次的关键。
    //    - `fork()` 方法已经完整复制了父进程的 Trap 上下文，
    //      所以子进程的上下文与父进程在调用 fork 时完全相同。
    //    - 我们需要手动修改子进程上下文中的返回值寄存器 `a0` (即 `x[10]`) 为 0。
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    trap_cx.x[10] = 0;

    // 3. 将新创建的子任务加入就绪队列，等待调度器执行。
    add_task(new_task);

    // 4. 对父进程而言，fork 调用结束，返回子进程的 PID。
    new_pid as isize
}

/// 将当前进程的地址空间清空并加载一个特定的可执行文件，返回用户态后开始它的执行。
/// 
/// @param path *const u8 - 指向用户空间中以 `\0` 结尾的、要执行的程序路径字符串的裸指针。
/// @return isize - 如果成功，永不返回；如果失败（如找不到文件），则返回 -1。
/// syscall ID：221
pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    // 打开文件，拷贝数据，传给 exec 执行，非常自然（相比于 get_app_data_by_name 和手工复制打包）
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice());
        0
    } else {
        -1
    }
}


/// 当前进程等待一个子进程变为僵尸进程，回收其全部资源并收集其返回值。
///
/// @param pid isize - 要等待的子进程 PID。
///                    - 如果 `pid` 为 -1，则等待任意一个子进程。
///                    - 如果 `pid` > 0，则等待指定的子进程。
/// @param exit_code_ptr *mut i32 - 指向用户空间内存的裸指针，用于接收子进程的退出码。
/// @return isize - 成功时，返回结束的子进程的 PID。
///               - 如果没有指定的子进程，返回 -1。
///               - 如果有指定的子进程但它尚未结束，返回 -2。
/// syscall ID：260
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();

    // 1. 锁定父进程的内部 TCB，以安全地访问其 `children` 列表。
    let mut inner = task.inner_exclusive_access();

    // 2. 检查是否存在匹配的子进程（无论其状态如何）。
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;   // 错误：没有指定的子进程。
    }

    // 3. 查找一个已经结束（处于 Zombie 状态）并且匹配 pid 的子进程。
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
    });

    // 4. 根据查找结果进行处理。
    if let Some((idx, _)) = pair {
        // 4a. 找到了一个僵尸子进程，进行资源回收。
        let child = inner.children.remove(idx);
        // 确认在父进程的 `children` 列表中移除后，`child` 的强引用计数为 1。
        // 这保证了 `child` 这个 Arc<TCB> 在函数结束时被销毁，从而触发 Drop，回收所有资源。
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        let exit_code = child.inner_exclusive_access().exit_code;
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        // 4b. 没有找到处于 Zombie 状态的子进程（即孩子还在运行）。
        -2
    }
}


/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
/// 获取当前的时间，保存在 TimeVal 结构体 ts 中，_tz 在我们的实现中忽略
/// syscall ID：169
pub fn sys_get_time(ts: *mut TimeVal, tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().pid.0
    );

    let us = get_time_us();
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
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let elf_data = app_inode.read_all();
        let task = current_task().unwrap();
        let new_task = task.spawn(&*elf_data);
        add_task(new_task.clone());
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