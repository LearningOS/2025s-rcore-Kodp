//! 系统层面的 Inode
//! 这个文件定义了操作系统内核中对 inode 的抽象，是连接上层文件描述符和底层具体文件系统（easy_fs）的桥梁。
//! 为了实现并发访问、内部可变性以及与具体文件系统的解耦，这里采用了多层封装结构：
//! 
//! 1.  easy_fs::Inode: 这是底层文件系统的 inode 实现，负责具体的磁盘块读写
//!     和元数据管理。它本身不包含任何与操作系统状态（如读写偏移量）相关的信息。
//!
//! 2.  Arc<Inode>: 使用 Arc 将底层 Inode 包裹起来，允许多个上层结构安全地共享
//!     同一个底层 Inode。这意味着，如果用户多次打开同一个文件，内核中会有多个 OSInode 
//!     实例，但它们都指向同一个 Arc<Inode>，共享同一份磁盘元数据。
//!
//! 3.  OSInodeInner: 这个结构体代表了一个“打开的文件实例”的可变状态。最核
//!     心的字段是 offset，即每个打开实例独立的读写指针。
//!
//! 4.  UPSafeCell<OSInodeInner>: 在单核操作系统中，UPSafeCell 提供了一种安
//!     全的方式来实现“内部可变性”。它允许我们在只有共享引用 &self 的情况下，也能修改内部
//!     的 OSInodeInner（比如更新 offset）。这是实现 File Trait 中 read 和 
//!     write 方法的关键，因为这些方法都只接收 &self。
//!
//! 5.  OSInode: 这是最终暴露给 VFS 其他部分（如文件描述符表）的结构。它包含了打开文
//!     件时的权限（readable, writable）和指向内部可变状态的 UPSafeCell。

use super::{File, Stat, StatMode};
use crate::drivers::BLOCK_DEVICE;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use easy_fs::{EasyFileSystem, Inode};
use lazy_static::*;


/// 内存中的 OSInode。
///@ 这层抽象有两个目的：
/// 1. 实现 File trait，真正达到文件的概念（判断可读/可写，统一缓冲区）
/// 2. 管理多个“打开文件”状态（offset）。多个进程同时打开相同文件，
///     文件都是一致的，所以通过 Arc 来持有下层 Inode 的指针。另外，也维护不同的读写位置。
pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}

/// OSInode 的可变部分
pub struct OSInodeInner {
    /// 当前的读写偏移量（字节）
    offset: usize,
    /// 指向 easy_fs Inode 的 Arc 指针。
    inode: Arc<Inode>,
}

impl OSInode {
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>) -> Self {
        Self {
            readable,
            writable,
            inner: unsafe { UPSafeCell::new(OSInodeInner {offset: 0, inode}) },
        }
    }

    /// 从当前读写偏移量开始，读取文件后续的所有数据
    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer: Vec<u8> = Vec::with_capacity(512);
        buffer.resize(512, 0);
        let mut v: Vec<u8> = Vec::new();
        loop {
            // 调用底层 inode 的 read_at 方法，从当前偏移量开始读取。
            let len = inner.inode.read_at(inner.offset, &mut buffer);
            // 如果读取到的长度为 0，表示已到达文件末尾。
            if len == 0 {
                break;
            }
            // 更新读写偏移量
            inner.offset += len;
            // 追加结果
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }
}

// 全局唯一根目录 Inode
lazy_static! {
    pub static ref ROOT_INODE: Arc<Inode> = {
        let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
        Arc::new(EasyFileSystem::root_inode(&efs))
    };
}

/// 列出根目录下的所有应用程序。
pub fn list_apps() {
    println!("/**** APPS ****");
    for app in ROOT_INODE.ls() {
        println!("{}", app);
    }
    println!("**************/");
}

pub fn list_apps_sorted() {
    println!("/**** APPS ****");

    // 1. 从根目录获取所有应用程序的名称列表。
    //    注意：这里需要将 `apps` 声明为 `mut` (可变)，因为排序操作会直接修改它。
    let mut apps = ROOT_INODE.ls();

    // 2. 对列表进行排序（按字典序从小到大）。
    //    .sort() 是 Vec 类型的标准方法，会直接在原向量上进行排序。
    apps.sort();

    // 3. 遍历排序后的列表并打印。
    for app in apps {
        println!("{}", app);
    }
    
    println!("**************/");
}

bitflags! {
    /// open 系统调用的 flags 参数
    pub struct OpenFlags: u32 {
        /// 只读方式打开。
        const RDONLY = 0;
        /// 只写方式打开。
        const WRONLY = 1 << 0;
        /// 读写方式打开。
        const RDWR = 1 << 1;
        /// 如果文件不存在，则创建它。
        const CREATE = 1 << 9;
        /// 如果文件存在，则将其长度截断为 0。
        const TRUNC = 1 << 10;
    }
}

impl OpenFlags {
    /// 一个辅助函数，将 OpenFlags 解析为 (readable, writable) 的布尔元组。
    /// 为了简化，这里没有做严格的合法性检查（如 RDONLY 和 WRONLY 同时存在）。
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            // 标志为空，对应 O_RDONLY。
            (true, false)
        } else if self.contains(Self::WRONLY) {
            // 包含 O_WRONLY，则只有写权限。
            (false, true)
        } else {
            // 其他情况（如 O_RDWR）都视为具有读写权限。
            (true, true)
        }
    }
}

/// 打开文件
pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    let (readable, writable) = flags.read_write();
    // 创建文件
    if flags.contains(OpenFlags::CREATE) {
        // 已存在则清空
        if let Some(inode) = ROOT_INODE.find(name) {
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode)))
        }
        else {
            ROOT_INODE
                // 创建文件
                .create(name)  
                // 转换为 OSInode
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            // 如果文件存在且包含 TRUNC 标志，则清空文件。
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
}

/// 硬链接文件
pub fn link_file(old_name: &str, new_name: &str) -> Option<()> {
    if old_name == new_name {
        return None
    }
    ROOT_INODE.link(old_name, new_name)
}

/// 释放硬链接
pub fn unlink_file(name: &str) -> Option<()> {
    ROOT_INODE.unlink(name)
}


/// 为 OSInode 实现通用的 File Trait，使其能被文件描述符系统统一处理。
impl File for OSInode {
    fn get_stat(&self) -> Option<Stat> {
        let inner = self.inner.exclusive_access();
        let inode = &inner.inode;
        
       // 获取 inode 的元数据
       let (mode, nlink) = inode.stat();

       // 将元数据转换为 StatMode 枚举
       let mode = match mode {
           1 => StatMode::DIR,
           2 => StatMode::FILE,
           _ => {
            debug!("Unknown inode type: {}", mode);
            panic!("Unknown inode type")}
       };
       // 创建 Stat 结构体
       Some(Stat {
           dev: 0, // 设备号固定为 0
           ino: 0,
           mode,
           nlink,
           pad: Default::default(),
       })  
    }

    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    /// 从文件中读取数据到用户缓冲区
    fn read(&self, mut buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = inner.inode.read_at(inner.offset, *slice);
            if read_size == 0 {
                break;  // 达到文件末尾
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }

    /// 将用户缓冲区的数据写入文件。
    fn write(&self, buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = inner.inode.write_at(inner.offset, *slice);
            assert_eq!(write_size, slice.len());
            inner.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }
}