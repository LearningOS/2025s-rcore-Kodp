//! 包含文件 I/O 相关的 syscall。

use crate::fs::{link_file, open_file, unlink_file, OSInode, OpenFlags, Stat};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
use crate::task::{current_task, current_user_token};

const FD_STDIN: usize = 0;
const FD_STDOUT: usize = 1;

/// write buf of length `len` to a file with `fd`
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

/// sys_read 系统调用实现
/// 从文件描述符 fd 指向的文件中读取数据到用户提供的缓冲区。
/// 参数：
/// - `fd`: 用户程序提供的文件描述符，用于索引当前任务的文件描述符表。
/// - `buf`: 指向用户空间缓冲区的裸指针，数据将被读入此缓冲区。
/// - `len`: 希望读取的字节数。
/// 成功时返回实际读取的字节数 (usize)，失败时返回 -1 (isize)。
pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }

    // 检查 fd 对应的表项是否为 Some(file)，即文件是否真的被打开了。
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

/// sys_open 系统调用实现
/// 根据路径和标志打开或创建一个文件，并返回一个新的文件描述符。
///
/// 参数：
/// - `path`: 指向用户空间字符串的裸指针，表示要打开的文件路径。
/// - `flags`: 文件打开标志 (如 O_RDONLY, O_CREATE 等)。
///
/// 成功时返回新分配的文件描述符 (usize)，失败时返回 -1 (isize)。
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = open_file(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = task.inner_exclusive_access();
        // 在文件描述符表中分配一个空闲的位置。
        let fd = inner.alloc_fd();
        // 将新打开的 inode 放入该位置。
        inner.fd_table[fd] = Some(inode);
        // 将新分配的文件描述符 fd 返回给用户程序
        fd as isize
    } else {
        -1
    }
}


/// sys_close 系统调用实现
/// 关闭一个文件描述符。
///
/// 参数
/// - `fd`: 要关闭的文件描述符。
///
/// 成功时返回 0，失败时返回 -1。
pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
     // 检查 fd 对应的文件是否本就未打开。
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
    //? 当一个文件的引用计数减少到 0 时，文件所占用的资源具体是怎么回收的？
}

/// YOUR JOB: Implement fstat.
/// syscall ID: 80
/// 
/// 功能：获取文件状态，填入 Stat。
/// 
/// Ｃ接口： int fstat(int fd, struct Stat* st)
/// 
/// Rust 接口： fn fstat(fd: i32, st: *mut Stat) -> i32
/// 
/// 参数：
/// fd: 文件描述符
/// st: 文件状态结构体
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().pid.0
    );
    //一个 Stat 占用 80 字节，可能会跨页，干脆按照字节来读
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }

    if let Some(file) = &inner.fd_table[fd] {
        // 获取文件的元数据
        if let Some(stat) = file.get_stat() {
            debug!("file state 0: {:?}", stat);            
            // 将 Stat 结构体写入用户空间
            let stat_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&stat as *const Stat) as *const u8,
                    core::mem::size_of::<Stat>(),
                )
            };
            let stat_ptr = st as *mut u8; //转换为字节指针指向用户态的 Stat
            // 由于 Stat 占用 80个字节，可能跨页 所以逐个字节写入
            let v = translated_byte_buffer(token, stat_ptr, core::mem::size_of::<Stat>());
            let flatten_dest_iter = v.into_iter().flatten();
            for (dest_byte, src_byte) in flatten_dest_iter.zip(stat_bytes.iter()) {
                *dest_byte = *src_byte;
            }
            return 0;
        }
        // 如果文件不支持获取状态，返回错误
        return -1;
    } else {
        // 文件描述符无效
        -1
    }
}

/// YOUR JOB: Implement linkat.
/// syscall ID: 37
/// 功能：创建一个文件的一个硬链接， linkat标准接口 。
/// 
/// 参数：
/// olddirfd，newdirfd: 仅为了兼容性考虑，本次实验中始终为 AT_FDCWD (-100)，可以忽略。
/// flags: 仅为了兼容性考虑，本次实验中始终为 0，可以忽略。
/// oldpath：原有文件路径
/// newpath: 新的链接文件路径。
/// 
/// 说明：
/// 为了方便，不考虑新文件路径已经存在的情况（属于未定义行为），除非链接同名文件。
/// 
/// 返回值：如果出现了错误则返回 -1，否则返回 0。
/// 
/// 可能的错误
/// 链接同名文件。
/// 
/// 思路：根据 old_name 检索到 inode 编号 n，将对应的 DiskInode 链接计数 + 1， 然后
/// 创建一个新的目录项，其 name 为 new_name，索引节点为 n，写入目录文件。
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_linkat",
        current_task().unwrap().pid.0
    );
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    if old_name == new_name {
        return -1;
    }
    if link_file(&old_name, &new_name).is_none() {
        return -1
    }
    0
}

/// YOUR JOB: Implement unlinkat.
/// syscall ID: 35
/// 
/// 功能：取消一个文件路径到文件的链接, unlinkat标准接口 。
/// 
/// Ｃ接口： int unlinkat(int dirfd, char* path, unsigned int flags)
/// 
/// Rust 接口： fn unlinkat(dirfd: i32, path: *const u8, flags: u32) -> i32
/// 
/// 参数：
/// dirfd: 仅为了兼容性考虑，本次实验中始终为 AT_FDCWD (-100)，可以忽略。
/// flags: 仅为了兼容性考虑，本次实验中始终为 0，可以忽略。
/// path：文件路径。
/// 
/// 说明：
/// 注意考虑使用 unlink 彻底删除文件的情况，此时需要回收inode以及它对应的数据块。
/// 
/// 返回值：如果出现了错误则返回 -1，否则返回 0。
/// 
/// 可能的错误
/// 文件不存在。
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().pid.0
    );
    let token = current_user_token();
    let name = translated_str(token, name);
    if unlink_file(&name).is_none() {
        return -1;
    }
    0
}
