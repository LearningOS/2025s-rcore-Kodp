
use super::{get_block_cache, BlockDevice, BLOCK_SZ};
use alloc::sync::Arc;

/// 为位图中的一个磁盘块定义一个类型别名 `BitmapBlock`，方便 [] 操作。
/// 将 512 字节的块视为 64 个 `u64` 整数（64 * 8B = 512B）。

type BitmapBlock = [u64; 64];

/// 表示单个磁盘块所能管理的位（bits）总数。
/// 512B = 4096 bits
const BLOCK_BITS: usize = BLOCK_SZ * 8;


/// 代表一个在磁盘上连续存放的位图区域。
/// 这是一个轻量级的 内存中 的数据结构，它本身不存储位图的具体内容，
/// 而是作为一个“句柄 (handle)”，记录了位图在磁盘上的起始位置和大小。
/// 这种设计的优点是节省内存：只有在需要对位图进行读写时，
/// 对应的磁盘块才会被加载到块缓存中，而不是将整个位图常驻内存。
pub struct Bitmap {
    /// 当前位图区域在块设备上的起始块号（起点）。
    start_block_id: usize,
    /// 位图区域占用的总块数。
    blocks: usize,
}

/// 将一个位编号分解为 (块索引，块内 u64 索引，u64 内位索引)
fn decomposition(mut bit: usize) -> (usize, usize, usize) {
    let block_pos = bit / BLOCK_BITS;
    bit %= BLOCK_BITS;
    (block_pos, bit / 64, bit % 64)
}

impl Bitmap {
    /// 创建一个新的 Bitmap 实例
    pub fn new(start_block_id: usize, blocks: usize) -> Self {
        Self {
            start_block_id,
            blocks,
        }
    }

    /// 在位图中查找并分配第一个可用的位（将其从 0 置为 1）。
    /// 成功时，返回 `Some(usize)`；如果位图已满，则返回 `None`。
    /// 
    /// 返回的 `usize` 值是一个逻辑编号，代表了所分配的位在整个位图覆盖范围内的、从0开始的绝对索引。
    ///
    /// 这个逻辑编号的具体语义由调用者决定：
    /// - 若本位图是 Inode 位图: 返回值即为 Inode 编号。
    /// - 若本位图是数据块位图: 返回值即为数据块在数据区内的逻辑块号。
    pub fn alloc(&self, block_device: &Arc<dyn BlockDevice>) -> Option<usize> {
        // 遍历位图区域中的每一个块，寻找可用的位。
        // block_id 是一个块偏移。
        for block_id in 0..self.blocks {
            // 所有磁盘 I/O 都应该在缓存层操作。
            let pos = get_block_cache(
                block_id + self.start_block_id as usize,
                Arc::clone(block_device),
            )
            .lock()
            .modify(0, |bitmap_block: &mut BitmapBlock| {
                // 在当前块内寻找第一个未满的 u64 (即存在 0 位的 u64)。
                // trailing_ones 计算一个 u64 数从最低位开始有多少个连续的 1, 也就恰好
                // 是第一个 0 的索引位置。 ...1011，trailing_ones 返回 2。
                if let Some((bits64_pos, inner_pos)) = bitmap_block
                    .iter()
                    .enumerate()  // 产出 (usize, &u64) 序列
                    .find(|(_, bits64)| **bits64 != u64::MAX)
                    .map(|(bits64_pos, bits64)| (bits64_pos, bits64.trailing_ones() as usize)) 
                {
                    // 将有空闲位的块的对应空闲位（最低 0 位）置为 1，表示分配成功
                    bitmap_block[bits64_pos] |= 1u64 << inner_pos;
                    // 计算位数（当前块起始位数 + 块内 u64 位数 + u64 内位数）
                    Some(block_id * BLOCK_BITS + bits64_pos * 64 + inner_pos as usize)
                }
                else {
                    // 当前块所有位都被分配，继续寻找下一个块
                    None
                }
            });

            // 一找到就返回
            if pos.is_some() {
                return pos;
            }
        }
        None
    }

    /// 释放一个位，将其标记为未使用。
    /// 对应于删除 inode 或释放数据块。
    pub fn dealloc(&self, block_device: &Arc<dyn BlockDevice>, bit: usize) {
        let (block_pos, bits64_pos, inner_pos) = decomposition(bit);

        // 1. 通过块缓存获取指定块
        // 2. 独占访问
        // 3. 将指定块的内容视作 BitmapBlock 类型读取，并抹掉 BitmapBlock 中指定块内
        //  指定 u64 内的那一位 1
        get_block_cache(block_pos + self.start_block_id, Arc::clone(block_device))
            .lock()
            .modify(0, |bitmap_block: &mut BitmapBlock| {
                // 在进行清零操作前，先断言该位确实是 1;
                // 帮助捕捉到重复释放（double-free）等逻辑错误
                assert!(bitmap_block[bits64_pos] & (1u64 << inner_pos) > 0);
                bitmap_block[bits64_pos] -= 1u64 << inner_pos;
            });
    }

    /// 该位图能管理的最多位数。
    pub fn maximum(&self) -> usize {
        self.blocks * BLOCK_BITS
    }
}