//! File trait & inode(dir, file, pipe, stdin, stdout)
//! 操作系统需要处理各种各样的 I/O 资源，例如：
//! - 磁盘上的普通文件和目录 (`inode.rs`)
//! - 标准输入/输出流 (`stdio.rs`)
//! - 进程间通信的管道 (Pipe)
//! - 网络套接字 (Socket)
//! 
//! 用一套接口统揽这些事情。

mod inode;
mod stdio;

use crate::mm::UserBuffer;

/// trait File for all file types
pub trait File: Send + Sync {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// read from the file to buf, return the number of bytes read
    fn read(&self, buf: UserBuffer) -> usize;
    /// write to the file from buf, return the number of bytes written
    fn write(&self, buf: UserBuffer) -> usize;
    fn get_stat(&self) -> Option<Stat> {
        None
    }
}

/// The stat of a inode
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// Device ID，标识了文件所在的存储设备（例如不同硬盘）
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// file type and mode
    pub mode: StatMode,
    /// 硬链接数量
    /// 删除一个文件名只是减少了 nlink 的计数。只有当 nlink 变为 0 时，系统才会认为这个
    ///     文件真正被删除了，并回收其占用的数据块。
    pub nlink: u32,
    /// unused pad
    pad: [u64; 7],
}

bitflags! {
    /// The mode of a inode
    /// whether a directory or a file
    pub struct StatMode: u32 {
        /// null
        const NULL  = 0;
        /// directory
        const DIR   = 0o040000;
        /// ordinary regular file
        const FILE  = 0o100000;
    }
}

pub use inode::{list_apps, open_file, OSInode, OpenFlags, list_apps_sorted, link_file, unlink_file};
pub use stdio::{Stdin, Stdout};
