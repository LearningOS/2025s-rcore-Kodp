use super::{get_block_cache, BlockDevice, BLOCK_SZ};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter, Result};

/// 魔数，识别 easy-fs
const EFS_MAGIC: u32 = 0x3b800001;


/// 直接索引块的数量。
/// 对于小文件，仅使用直接索引即可，无需额外的索引块开销，非常高效。
const INODE_DIRECT_COUNT: usize = 28;

/// 文件名的最大长度限制。

const NAME_LENGTH_LIMIT: usize = 27;
/// 一级间接索引块能指向的数据块数量。
/// 一个块大小为 512 字节，每个块号用 u32（4字节）存储，所以一个索引块能存放 512 / 4 = 128 个块号。
const INODE_INDIRECT1_COUNT: usize = BLOCK_SZ / 4;

/// 二级间接索引块能指向的数据块数量。
/// 二级索引块的每个条目指向一个一级索引块，因此总容量是一级索引容量的平方。
const INODE_INDIRECT2_COUNT: usize = INODE_INDIRECT1_COUNT * INODE_INDIRECT1_COUNT;

/// 直接索引的范围上界 28。
const DIRECT_BOUND: usize = INODE_DIRECT_COUNT;

/// 一级间接索引的范围上界 28 + 128。
const INDIRECT1_BOUND: usize = DIRECT_BOUND + INODE_INDIRECT1_COUNT;

/// 二级间接索引的范围上界 28 + 128 + 128*128。
#[allow(unused)]
const INDIRECT2_BOUND: usize = INDIRECT1_BOUND + INODE_INDIRECT2_COUNT;


/// 超级块 SuperBlock
#[repr(C)]  // 与 C 语言的布局规则保持一致
// 因为我们需要将这个结构体直接作为一个字节序列写入磁盘或从磁盘读取
pub struct SuperBlock {
    /// 魔数
    magic: u32,
    /// 文件系统总块数
    pub total_blocks: u32,
    /// inode 位图占用块数
    pub inode_bitmap_blocks: u32,
    /// inode 区域占用块数
    pub inode_area_blocks: u32,
    /// 数据块位图占用块数
    pub data_bitmap_blocks: u32,
    /// 数据块区域占用块数
    pub data_area_blocks: u32,
}

impl Debug for SuperBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        //? 什么意思
        f.debug_struct("SuperBlock")
            .field("total_blocks", &self.total_blocks)
            .field("inode_bitmap_blocks", &self.inode_bitmap_blocks)
            .field("inode_area_blocks", &self.inode_area_blocks)
            .field("data_bitmap_blocks", &self.data_bitmap_blocks)
            .field("data_area_blocks", &self.data_area_blocks)
            .finish()
    }
}

impl SuperBlock {
    pub fn initialize(
        &mut self,
        total_blocks: u32,
        inode_bitmap_blocks: u32,
        inode_area_blocks: u32,
        data_bitmap_blocks: u32,
        data_area_blocks: u32,
    ) {
        *self = Self {
            magic: EFS_MAGIC,
            total_blocks,
            inode_bitmap_blocks,
            inode_area_blocks,
            data_bitmap_blocks,
            data_area_blocks,
        }
    }

    /// 检查魔数
    pub fn is_valid(&self) -> bool {
        self.magic == EFS_MAGIC
    }
}

/// 磁盘上 inode 的类型枚举。
#[derive(PartialEq, Clone)]
pub enum DiskInodeType {
    File,
    Directory,
}

/// 定义“间接索引块”的类型别名。
/// 其实就是一个装下一级块号的数组，能装 128 个。
/// 
/// 一级索引块和二级索引块都是 type IndirectBlock = [u32; BLOCK_SZ / 4];
/// 由 DiskInode 区分它们
type IndirectBlock = [u32; BLOCK_SZ / 4];

/// 定义“数据块”的类型别名。
/// 其实就是一个字节数组, BLOCK_SZ bytes。
type DataBlock = [u8; BLOCK_SZ];

/// 索引节点。
/// 
/// 包含了一个文件的基本属性（大小、文件或目录类型）和该文件包含的所有数据块的块号。
/// 这个数据结构是无状态的：不持有任何引用。
/// 完全不管如何分配或回收块，只依赖传进来的块号填充。
/// 
/// 使用 `#[repr(C)]` 来保证稳定的内存布局。
/// DiskInode 的大小被精心设计为 128 字节，这样每个 512 字节的磁盘块正好能容纳 4 个。
#[repr(C)]
pub struct DiskInode {
    /// 对于文件，是以字节为单位的大小。
    /// 对于目录，是目录项的字节数量。
    pub size: u32,

    /// 直接索引表
    pub direct: [u32; INODE_DIRECT_COUNT],  //& 存 28 个块的块号。

    /// 一级间接索引，其实是索引块块号。
    pub indirect1: u32,

    /// 二级间接索引，其实是索引块块号。
    pub indirect2: u32,

    /// 硬链接数量
    pub nlink: u8,
    /// Inode 类型：文件或目录
    pub type_: DiskInodeType,
    // 1 + 28 + 1 + 1 + 1 = 32, 32 * u32 = 32 * 4B = 128B
}

impl DiskInode {
    /// 初始化一个新磁盘 inode。
    pub fn initialize(&mut self, type_: DiskInodeType) {
        self.size = 0;
        self.direct.iter_mut().for_each(|v| *v = 0);
        self.indirect1 = 0;
        self.indirect2 = 0;
        self.nlink = 1;
        self.type_ = type_;
    }
    /// 判断此 inode 是否为目录
    pub fn is_dir(&self) -> bool {
        self.type_ == DiskInodeType::Directory
    }
    /// 判断此 inode 是否为文件。
    #[allow(unused)]
    pub fn is_file(&self) -> bool {
        self.type_ == DiskInodeType::File
    }

    // --- 以下是与文件大小和块分配计算相关的辅助函数 ---
    // 这些函数的设计目的是将复杂的块数量计算逻辑封装起来，
    // 为上层的文件增长（increase_size）和清空（clear_size）操作提供支持。

    /// 根据当前文件大小，计算需要多少个**数据块**来存储内容。
    pub fn data_blocks(&self) -> u32 {
        Self::_data_blocks(self.size)
    }
    fn _data_blocks(size: u32) -> u32 {
        (size + BLOCK_SZ as u32 - 1) / BLOCK_SZ as u32  // 上取整
    }

    /// 根据文件大小，计算总共需要多少个块（包括数据块和所有层级的索引块）。
    pub fn total_blocks(size: u32) -> u32 {
        let data_blocks = Self::_data_blocks(size) as usize;
        let mut total = data_blocks;
        // 如果数据块数量超过了直接索引的范围，就需要一个一级间接索引块。
        if data_blocks > INODE_DIRECT_COUNT {
            total += 1;
        }

        // 如果数据块数量进一步超过了一级索引的范围，就需要一个二级间接索引块，
        // 以及由它指向的若干个一级间接索引块。
        if data_blocks  > INDIRECT1_BOUND {
            // 需要一个二级索引块
            total += 1;
            // 计算需要多少个额外的一级索引块
            // data_blocks - INDIRECT1_BOUND 表示超出一级索引部分的块数，
            // + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT 表示上取整
            // 后计算还需要多少个一级索引块，这些块的块号由二级索引管理。
            total += ((data_blocks - INDIRECT1_BOUND) + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT;
        }
        total as u32
    }

    /// 计算当文件从当前大小增长到 new_size 时，需要额外分配多少个块。
    /// 这个函数让上层模块（EFS）可以预先知道需要分配多少块，然后再调用 `increase_size`。
    pub fn blocks_num_needed(&self, new_size: u32) -> u32 {
        assert!(new_size >= self.size);
        Self::total_blocks(new_size) - Self::total_blocks(self.size)
    }

    /// 根据文件内的 **逻辑块号** inner_id 查找并返回对应的物理块号
    /// 
    /// 这是 DiskInode 最核心的功能：将文件的逻辑视图映射到磁盘的物理存储。
    /// 实现了直接/间接索引的查找逻辑。
    pub fn get_block_id(&self, inner_id: u32, block_device: &Arc<dyn BlockDevice>) -> u32 {
        let inner_id = inner_id as usize;

        // 1. 逻辑块号落在直接索引区。
        if inner_id < INODE_DIRECT_COUNT {
            self.direct[inner_id]
        }
        // 2. 逻辑块号落在一级间接索引区。
        else if inner_id < INDIRECT1_BOUND {
            get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect_block: &IndirectBlock| {
                    indirect_block[inner_id - INODE_DIRECT_COUNT]
                })
        }
        // 3. 逻辑块号落在二级间接索引区
        else {
            let last: usize = inner_id - INDIRECT1_BOUND;

            // 读取二级间接索引块，找到对应的一级间接索引块的块号。
            let indirect_block_id = get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect2: &IndirectBlock| {
                    indirect2[last / INODE_INDIRECT1_COUNT]
                });
            
            // 读取该一级间接索引块，最终找到数据块的块号
            get_block_cache(indirect_block_id as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect1: &IndirectBlock| {
                    indirect1[last % INODE_INDIRECT1_COUNT]
                })
        }
    }

    /// 增加 inode 的大小，并将新分配的块（new_blocks）填充到索引结构中。
    pub fn increase_size(
        &mut self,
        new_size: u32,
        new_blocks: Vec<u32>,
        block_device: &Arc<dyn BlockDevice>,
    ) {
        let mut current_blocks = self.data_blocks();
        self.size = new_size;
        let total_blocks = self.data_blocks();
        let mut new_blocks = new_blocks.into_iter();

        // 逻辑是分阶段填充：优先填充直接索引，然后是一级，最后是二级。
        // --- 填充直接索引 ---
        while current_blocks < total_blocks.min(INODE_DIRECT_COUNT as u32) {
            self.direct[current_blocks as usize] = new_blocks.next().unwrap();
            current_blocks += 1;
        }

        if total_blocks <= INODE_DIRECT_COUNT as u32 { return; }

        // --- 分配并填充一级间接索引 ---
        // 如果是首次进入一级索引区，需要先为 indirect1 指针分配一个索引块。
        if current_blocks == INODE_DIRECT_COUNT as u32 {
            self.indirect1 = new_blocks.next().unwrap();
        }
        get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect1: &mut IndirectBlock| {
                while current_blocks < total_blocks.min(INDIRECT1_BOUND as u32) {
                    indirect1[(current_blocks - INODE_DIRECT_COUNT as u32) as usize] = new_blocks.next().unwrap();
                    current_blocks += 1;
                }
            });

        if total_blocks <= INDIRECT1_BOUND as u32 { return; }
        
        // --- 分配并填充二级间接索引 ---
        // 逻辑与一级类似，但更复杂，因为涉及到两层索引块的分配和填充。
        if current_blocks == INDIRECT1_BOUND as u32 {
            self.indirect2 = new_blocks.next().unwrap();
        }
        
        let mut l2_current = current_blocks - INDIRECT1_BOUND as u32;
        let l2_total = total_blocks - INDIRECT1_BOUND as u32;

        get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect2: &mut IndirectBlock| {
                while l2_current < l2_total {
                    let l2_idx = l2_current as usize / INODE_INDIRECT1_COUNT;
                    let l1_idx = l2_current as usize % INODE_INDIRECT1_COUNT;
                    // 如果进入一个新的二级索引条目（即需要一个新的从属一级索引块）
                    if l1_idx == 0 {
                        indirect2[l2_idx] = new_blocks.next().unwrap();
                    }
                    // 填充从属的一级索引块
                    get_block_cache(indirect2[l2_idx] as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            indirect1[l1_idx] = new_blocks.next().unwrap();
                        });
                    l2_current += 1;
                }
            });
    }

    /// 清空 inode 内容，回收占用的数据块和索引块
    /// 返回一个包含所有回收块号的向量，交由上层模块 EFS 进行 dealloc
    pub fn clear_size(&mut self, block_device: &Arc<dyn BlockDevice>) -> Vec<u32> {
        let mut v: Vec<u32> = Vec::new();
        let mut data_blocks = self.data_blocks() as usize;
        self.size = 0;
        let mut current_blocks = 0usize;
        // direct
        while current_blocks < data_blocks.min(INODE_DIRECT_COUNT) {
            v.push(self.direct[current_blocks]);
            self.direct[current_blocks] = 0;
            current_blocks += 1;
        }
        // indirect1 block
        if data_blocks > INODE_DIRECT_COUNT {
            v.push(self.indirect1);
            data_blocks -= INODE_DIRECT_COUNT;
            current_blocks = 0;
        } else {
            return v;
        }
        // indirect1
        get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect1: &mut IndirectBlock| {
                while current_blocks < data_blocks.min(INODE_INDIRECT1_COUNT) {
                    v.push(indirect1[current_blocks]);
                    //indirect1[current_blocks] = 0;
                    current_blocks += 1;
                }
            });
        self.indirect1 = 0;
        // indirect2 block
        if data_blocks > INODE_INDIRECT1_COUNT {
            v.push(self.indirect2);
            data_blocks -= INODE_INDIRECT1_COUNT;
        } else {
            return v;
        }
        // indirect2
        assert!(data_blocks <= INODE_INDIRECT2_COUNT);
        let a1 = data_blocks / INODE_INDIRECT1_COUNT;
        let b1 = data_blocks % INODE_INDIRECT1_COUNT;
        get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect2: &mut IndirectBlock| {
                // full indirect1 blocks
                //@ take 表示取迭代器的前 n 个元素
                // 这里表示只取被使用的块
                for entry in indirect2.iter_mut().take(a1) {
                    v.push(*entry);
                    get_block_cache(*entry as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            for entry in indirect1.iter() {
                                v.push(*entry);
                            }
                        });
                }
                // last indirect1 block
                if b1 > 0 {
                    v.push(indirect2[a1]);
                    get_block_cache(indirect2[a1] as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            for entry in indirect1.iter().take(b1) {
                                v.push(*entry);
                            }
                        });
                    //indirect2[a1] = 0;
                }
            });
        self.indirect2 = 0;
        v
    }

    /// 从 Inode 指定偏移量开始读取数据到缓冲区 buf，最多不超过 buf 长度和数据量
    /// 返回实际读取的字节数
    pub fn read_at(
        &self,
        offset: usize,  // 偏移
        buf: &mut [u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {

        // 首先通过偏移 offset 计算出相对块号，然后计算得到绝对块号
        let mut start = offset;
        let end = (offset + buf.len()).min(self.size as usize);
        if start >= end {
            return 0;
        }
        let mut read_size = 0usize;
        // 循环处理，因为一次读取可能跨越多个块。
        loop {
            let start_block = start / BLOCK_SZ;
            // 计算本次循环在当前块内需要读取的数据范围。
            let end_current_block = ((start / BLOCK_SZ) + 1) * BLOCK_SZ;
            let end_in_block = end.min(end_current_block);
            let block_read_size = end_in_block - start;
            
            // 获取当前逻辑块对应的物理块号。
            let block_id = self.get_block_id(start_block as u32, block_device);
            
            // 通过块缓存读取数据。
            get_block_cache(block_id as usize, Arc::clone(block_device))
                .lock()
                .read(0, |data_block: &DataBlock| {
                    let src = &data_block[start % BLOCK_SZ .. start % BLOCK_SZ + block_read_size];
                    let dst = &mut buf[read_size .. read_size + block_read_size];
                    dst.copy_from_slice(src);
                });
            read_size += block_read_size;
            if end_in_block == end { break; } // 已读完所有需要的数据
            start = end_in_block; // 更新下一次循环的起始位置
        }
        read_size
    }

    /// 写入数据；不会因为不够写而自动扩容。扩容交给上层调用。
    pub fn write_at(
        &mut self,
        offset: usize,
        buf: &[u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {
        let mut start = offset;
        // 确定最终写入位置，如果超过文件边界则直接舍弃。
        let end = (offset + buf.len()).min(self.size as usize);
        assert!(start <= end);
        let mut write_size = 0usize;
        loop {
            let start_block = start / BLOCK_SZ;
            let end_current_block = ((start / BLOCK_SZ) + 1) * BLOCK_SZ;
            let end_in_block = end.min(end_current_block);
            let block_write_size = end_in_block - start;
            let block_id = self.get_block_id(start_block as u32, block_device);

            // 通过块缓存写入数据（modify 会将缓存标记为“脏”）。
            get_block_cache(block_id as usize, Arc::clone(block_device))
                .lock()
                .modify(0, |data_block: &mut DataBlock| {
                    let src = &buf[write_size .. write_size + block_write_size];
                    let dst = &mut data_block[start % BLOCK_SZ .. start % BLOCK_SZ + block_write_size];
                    dst.copy_from_slice(src);
                });
            
            write_size += block_write_size;
            if end_in_block == end { break; }
            start = end_in_block;
        }
        write_size
    }
}

/// 目录项
/// 
/// 目录由多个目录项组成。每个目录项都是一个二元组，二元组的首个元素是目录下面的一个文件
/// （或子目录）的文件名（或目录名），另一个元素则是文件（或子目录）所在的索引节点编号。
/// 同样使用 `#[repr(C)]`。其大小被设计为 32 字节，方便对齐和计算。
#[repr(C)]
pub struct DirEntry {
    name: [u8; NAME_LENGTH_LIMIT + 1], // 文件名或目录名
    inode_id: u32,      // 文件或目录对应的 inode 编号
}

/// 目录项长度（字节）
pub const DIRENT_SZ: usize = 32;

impl DirEntry {
    /// 创建一个空的目录项。
    pub fn empty() -> Self {
        Self {
            name: [0u8; NAME_LENGTH_LIMIT + 1],
            inode_id: 0,
        }
    }
    /// 根据文件名和 inode 编号创建一个新的目录项。
    pub fn new(name: &str, inode_id: u32) -> Self {
        let mut bytes = [0u8; NAME_LENGTH_LIMIT + 1];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Self {
            name: bytes,
            inode_id,
        }
    }

    /// 将目录项结构体序列化为字节切片，用于写入磁盘。
    /// 使用 `unsafe` 是因为这里进行了从结构体指针到裸字节指针的强制转换，
    /// 其安全性由 `#[repr(C)]` 和固定大小 `DIRENT_SZ` 来保证。
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const _ as *const u8, DIRENT_SZ) }
    }

    /// 将目录项结构体序列化为可变字节切片，用于从磁盘读取数据。
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self as *mut _ as *mut u8, DIRENT_SZ) }
    }

    /// 从字节数组中解析出文件名字符串。
    /// 它通过查找第一个空字符 `\0` 来确定文件名的实际长度。
    pub fn name(&self) -> &str {
        let len = (0..).find(|&i| self.name[i] == 0).unwrap();
        core::str::from_utf8(&self.name[..len]).unwrap()
    }

    /// 获取目录项对应的 inode 编号。
    pub fn inode_id(&self) -> u32 {
        self.inode_id
    }
}