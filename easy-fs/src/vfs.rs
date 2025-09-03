//! VFS 层：定义内存中的 Inode 结构，作为文件系统暴露给上层的统一接口。
//! 它封装了所有底层细节，提供面向文件和目录的对象化操作。
//! 
//! EasyFileSystem 实现了我们设计的磁盘布局并能够将所有块有效的管理起来。但是对于文件系统的
//! 使用者而言，他们往往不关心磁盘布局，而是更希望直接看到目录树结构中的文件和目录。为此我们设
//! 计索引节点 Inode 暴露给文件系统的使用者，让他们能够直接对文件和目录进行操作。 


use crate::BLOCK_SZ;

use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode,
    DiskInodeType, EasyFileSystem, DIRENT_SZ,
};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};

/// 内存中的索引节点。
/// 
/// 它本身不实现块分配算法，也不实现索引填充算法。它将这些具体的任务委托给相应的专家组件。
pub struct Inode {
    /// 对应的 `DiskInode` 在磁盘上的块号。
    block_id: usize,
    /// 对应的 `DiskInode` 在块内的偏移量。
    block_offset: usize,
    /// 所属文件系统的全局管理器，用于需要全局信息的操作（如块分配）。
    fs: Arc<Mutex<EasyFileSystem>>,
    /// 底层块设备句柄。
    block_device: Arc<dyn BlockDevice>,
}

impl Inode {
    /// 获取 inode 状态信息
    pub fn stat(&self) -> (i32, u32) {
        let (mode, nlink) = self.read_disk_inode(|disk_inode| {
            let mode = if disk_inode.is_dir() {
                1
            } else {
                2
            };
            let nlink = disk_inode.nlink;
            (mode, nlink as u32)
        });
        (mode, nlink as u32)
    }

    /// 获取该 inode 在位图中的位置
    pub fn get_inode_id(&self, inode_area_start_block: usize) -> u32 {
        let inode_size = core::mem::size_of::<DiskInode>();
        let inodes_per_block = BLOCK_SZ / inode_size;
        ((self.block_id - inode_area_start_block) * inodes_per_block 
            + self.block_offset / inode_size) as u32
    }

    /// link
    /// link
    pub fn link(&self, old_name: &str, new_name: &str) -> Option<()> {

        let mut fs = self.fs.lock();

        // 1. 找到目标文件的 inode
        //    由于已持有锁，不能调用 self.find() (会造成死锁)，
        //    因此我们使用其内部实现来查找 inode 编号。
        let (old_inode_id, new_exists) = self.read_disk_inode(|disk_inode| {
            let old_id = self.find_inode_id(old_name, disk_inode);
            let new_ex = self.find_inode_id(new_name, disk_inode).is_some();

            (old_id, new_ex)
        });
        
        if new_exists { return None; }
        let Some(old_inode_id) = old_inode_id else { return None; };

        let (old_inode_block_id, old_inode_block_offset) = fs.get_disk_inode_pos(old_inode_id);

        // 2. 增加目标文件的硬链接计数
        get_block_cache(old_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(old_inode_block_offset, |old_disk_inode: &mut DiskInode| {
                old_disk_inode.nlink += 1;
            });

        // 3. 添加一个新的目录项到当前目录
        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;

            // fs 锁已在函数开头获取，这里直接使用
            self.increase_size(new_size as u32, root_inode, &mut fs);
            
            let dirent = DirEntry::new(new_name, old_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device
            );
        });

        block_cache_sync_all();
        Some(())
    }

   /// unlink
    pub fn unlink(&self, name: &str) -> Option<()> {
        let Some(inode) = self.find(name) else{
            return None;
        }; //链接对象不存在的时候返回 None
        
        let mut fs = self.fs.lock();
        
        // 减少文件的链接计数
        let need_dealloc = inode.modify_disk_inode(|disk_inode: &mut DiskInode| {
            disk_inode.nlink -= 1;
            disk_inode.nlink == 0 
        });
        
        // 如果链接计数为0，回收数据块和inode
        if need_dealloc {
            
            // 获取并清除文件的数据块
            inode.modify_disk_inode(|disk_inode: &mut DiskInode| {
                let size = disk_inode.size;
                let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
                assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
                // 回收数据块
                for data_block in data_blocks_dealloc.into_iter() {
                    fs.dealloc_data(data_block);
                }
            });
            block_cache_sync_all();
            
            // 回收inode
            let inode_id = inode.get_inode_id(fs.inode_area_start_block as usize);
            
            fs.dealloc_inode(inode_id);
        }
        
        // 从目录中删除文件项
        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let mut dirent = DirEntry::empty();
            
            // 遍历，找到要删除的文件项的位置
            let mut found_idx = 0;
            
            for i in 0..file_count {
                assert_eq!(
                    root_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device),
                    DIRENT_SZ,
                );
                if dirent.name() == name {
                    found_idx = i;
                    break;
                }
            }
            
            
            // 如果是最后一个目录项，直接减小目录大小
            if found_idx == file_count - 1 {
                root_inode.size -= DIRENT_SZ as u32;
            } else {
                
                // 否则，将最后一个文件项移动到要删除的位置
                let last_idx = file_count - 1;
                let mut last_dirent = DirEntry::empty();
                assert_eq!(
                    root_inode.read_at(DIRENT_SZ * last_idx, last_dirent.as_bytes_mut(), &self.block_device),
                    DIRENT_SZ,
                );
                
                // 将最后一个文件项写入到要删除的位置
                root_inode.write_at(
                    DIRENT_SZ * found_idx,
                    last_dirent.as_bytes(),
                    &self.block_device,
                );
                
                // 减小目录大小
                root_inode.size -= DIRENT_SZ as u32;
            }
        });
        
        block_cache_sync_all();
        
        Some(())

    }

    /// 创建一个内存 Inode 实例。
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }

    /// 获取并锁定 DiskInode，接受闭包函数来操作。
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }

    /// 辅助函数：通过闭包安全地修改底层 `DiskInode`。
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }

    /// 根据文件名和目录文件的 DiskInode，查找 inode 编号。
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        assert!(disk_inode.is_dir());
        //@ 目录的 size 是什么时候设置的？
        // 构建文件系统、填入文件的时候。 
        // 具体来说是 easy-fs-fuse/src/main.rs::easy_fs_pack() 里，root_inode.create
        // -> increase_size 处。
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device);
            if dirent.name() == name {
                return Some(dirent.inode_id() as u32);
            }
        }
        None
    }

    /// 基于文件名查找文件。若成功则创建并返回对应的内存 Inode。
    ///@ 穿越所有层的情况：
    /// Inode::find(name)
    /// -> find_inode_id(name, disk_inode)
    /// -> 遍历目录文件查找名称
    /// -> DiskInode::read_at(&self, offset, buf, block_device) 到 buf，也就是 dirent 里面
    /// -> dirent.inode_id() 返回 Inode 编号
    /// -> Inode 编号通过转换变为 Inode 块号，封装出 Inode 结构体，随后变为 Option<Arc。
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        //? 每一层嵌套的含义
        let fs = self.fs.lock();
        // disk_inode 是在 Inode 的 block_id 指定的块上读取出来的
        self.read_disk_inode(|disk_inode | {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                // map 如果是被 None 调用则返回 None，没有对应 DiskInode 就会返回 NOne
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }

    /// 辅助函数：协调 `EasyFileSystem` 分配新块，并调用 `DiskInode` 的方法来扩容
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size <= disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }

    /// 在 self 这个目录中，创建一个名为 name 的新文件
    /// 这个函数必须由目录文件调用！！
    /// 流程：查重 -> 分配新 inode -> 初始化 inode -> 修改目录文件 -> 同步回盘。
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        // 锁定整个文件系统
        let mut fs = self.fs.lock();
        // 如果已经存在 name 这个文件，就返回 None
        if self.read_disk_inode(|disk_inode| self.find_inode_id(name, disk_inode)).is_some() {
            return None;
        }

        // 读取文件系统的 DiskInode 位图，分配一个新 inode 编号，并将这个位置的
        //  DiskInode 数据块加载到内存、初始化为一个空的文件 
        let new_inode_id = fs.alloc_inode();  // 绝对 id
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        
        // 对目录文件写入新目录项
        self.modify_disk_inode(|parent_inode| {
            // ... 计算父目录的新大小 ...
            let file_count = (parent_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // ... 如果需要，为父目录扩容 ...
            self.increase_size(new_size as u32, parent_inode, &mut fs);
            // ... 创建一个新的目录项 ...
            let dirent = DirEntry::new(name, new_inode_id);
            // ... 将新目录项写入父目录的数据末尾 ...
            parent_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(), 
                &self.block_device,
            );
        });
        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        // 写回磁盘
        block_cache_sync_all();

        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )))
    }

    /// 读取目录下的所有目录项，返回文件名列表。
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device);
                v.push(String::from(dirent.name()));
            }
            v
        })
    }

    /// 读取从 offset 开始的文件内容到 buf 中。
    /// 返回实际读取的字节数。
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }

    /// VFS 层的写接口，加锁、按需扩容并委托给 `DiskInode::write_at`。
    /// 返回实际写入的字节数。
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode|{
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }

    /// 清空文件内容并回收所有块。
    /// 流程：调用 `DiskInode::clear_size` 收集所有块号，再交由 `EasyFileSystem` 逐个回收。
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
}