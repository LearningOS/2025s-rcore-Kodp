use super::{BlockDevice, BLOCK_SZ};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
use spin::Mutex;

/// BlockCache 结构体代表一个位于内存中的磁盘块缓存。
/// 它的核心设计目标是减少对底层块设备（通常是慢速的I/O设备）的直接访问次数，
/// 从而提升文件系统的整体性能。
/// 它将磁盘块的数据读入内存（cache字段），并跟踪该数据是否被修改过（modified字段）。
pub struct BlockCache {
    /// 缓存的块数据。
    /// 这是一个与硬件块大小一致的字节数组，作为磁盘数据的内存副本，
    /// 使得CPU可以高速访问和修改这些数据。
    cache: [u8; BLOCK_SZ],

    /// 缓存数据对应的磁盘块编号。
    /// 这个ID是缓存与物理存储之间的唯一链接。
    block_id: usize,

    /// 底层块设备的引用。
    /// 为了在需要时（如初次加载或写回时）能够与物理设备交互，
    /// 必须持有对块设备驱动的引用。使用 Arc<dyn BlockDevice> 是为了
    /// 1. Arc: 允许多个BlockCache实例共享同一个块设备对象的所有权。
    /// 2. dyn BlockDevice: 使用trait对象，实现对具体块设备驱动的解耦，增强了文件系统的通用性。
    /// 
    /// dyn <Trait> 表示一个运行时才能确定，但保证实现了 Trait 这个 trait 的类型。
    /// 只要一个结构实现了 BlockDevice 这个 trait，easy-fs 就能在上面运行。
    /// 类似于一个多协议充电器，只要手机实现了USB-C接口，就能给这个手机充电。
    block_device: Arc<dyn BlockDevice>,

    /// 标记缓存是否被修改过（“脏”位）。
    /// 关键的性能优化设计。只有当 modified 为 true 时，才需要在缓存被换出或系统同步
    /// 时将数据写回磁盘。这避免了对未修改数据的无效写操作。
    modified: bool,
}

impl BlockCache {
    /// 从磁盘加载数据，创建一个新的块缓存实例。
    /// 设计上，BlockCache的创建总是与一次磁盘读取操作绑定。
    /// 这是因为缓存的意义就在于持有磁盘数据的副本，因此实例化的第一步就是获取这个副本。
    pub fn new(block_id: usize, block_device: Arc<dyn BlockDevice>) -> Self {
        let mut cache = [0u8; BLOCK_SZ];
        // 读数据填缓冲区
        block_device.read_block(block_id, &mut cache);
        Self {
            cache,
            block_id,
            block_device,
            modified: false, // 初始状态下，缓存是干净的
        }
    }

    /// 获取缓存内部指定偏移量的地址。
    /// 这是一个内部辅助函数，其设计目的是为了在上层提供类型安全的接口（get_ref/get_mut）
    /// 而将底层的指针和地址计算封装起来。
    fn addr_of_offset(&self, offset: usize) -> usize {
        &self.cache[offset] as *const _ as usize
    }

    /// 将缓存中的一段字节数据解析为特定类型的不可变引用。
    /// 这是一个泛型方法，旨在提供一种类型安全的方式来访问缓存中的数据结构（如SuperBlock, DiskInode）。
    /// 用户无需关心底层的字节序或内存布局，可以直接操作结构体。
    pub fn get_ref<T>(&self, offset: usize) -> &T 
    where T: Sized {
        let type_size = core::mem::size_of::<T>();
        // 保证要访问的数据结构不会超出单个块的边界
        assert!(offset + type_size <= BLOCK_SZ);
        let addr = self.addr_of_offset(offset);
        unsafe { &*(addr as *const T) }
    }

    /// 将缓存中的一段字节数据解析为特定类型的可变引用。
    pub fn get_mut<T>(&mut self, offset: usize) -> &mut T 
    where T: Sized {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        // 一旦获取了可变引用，就必须假设缓存内容可能被修改。
        self.modified = true;  // 只要外界请求了可变访问，我们就视为脏
        let addr = self.addr_of_offset(offset);
        unsafe { &mut *(addr as *mut T) }
    }

    /// 提供一个更易用的接口来读取缓存中的数据结构。
    /// 
    /// 这种基于闭包的设计模式，将“获取引用”和“使用引用”两个步骤合并。利用闭包来安全地、临时性
    /// 地开放内部数据的访问权限，避免了复杂的生命周期管理问题。
    /// 
    /// 为什么不直接返回一个 &T 就好了？像 get_ref 那样？
    /// 答案在于生命周期 (Lifetime) 和所有权。get_ref 返回的引用 &T 的生命周期，与 self
    ///  (即 BlockCache 对象) 的生命周期是绑定的。在某些复杂的情况下（比如 BlockCache 
    /// 本身被一个锁保护），这个引用可能无法“逃离”当前的作用域，导致使用起来非常不方便，容易
    /// 出现编译错误。
    /// 而 read 函数采用的闭包模式则完美地解决了这个问题。它的逻辑是：
    /// 你 (调用者): "嘿，read 函数，我想用一下你内部偏移量为 offset 的那个 SuperBlock 数据。"
    /// read 函数: "直接把 &SuperBlock 给你有点危险，生命周期不好管。这样吧，你告诉我你想对它做什么，把你要做的事情写在一个闭包 f 里给我。"
    /// 你 (调用者): "好的，我只想读取它的 total_blocks 字段。我给你这个闭包：|sb: &SuperBlock| sb.total_blocks。"
    /// read 函数: "收到！"
    /// 它在内部调用 get_ref，安全地拿到了 &SuperBlock 的引用。
    /// 它立刻调用你给的闭包 f，并把引用传进去：f(&SuperBlock)。
    /// 你的闭包执行，返回了 total_blocks 的值（一个 u32）。
    /// read 函数拿到这个 u32 值，然后把它作为自己的返回值返回给你。
    /// 你 (调用者): 你拿到了 total_blocks 的值。
    /// 
    /// 整个过程中，你只得到了一个安全的 u32 值，而那个有生命周期限制的 &SuperBlock 引用从未暴露给你，它被完美地限制在了 read 函数的内部。
    /// 
    /// 语法：
    /// FnOnce 表示接受所有类型的闭包。
    /// impl FnOnce 表示任何实现了 FnOnce trait 的类型。
    /// (&T) -> V 表示这个函数的签名。
    /// 
    /// f：|disk_inode| disk_inode.read_at(offset, buf, &self.block_device)
    ///     这个函数传入类型是 &DiskInode，于是 get_ref 会返回 &DiskInode类型；
    ///     随后 f 开始作用，从 inode 指定偏移量开始读取数据到缓冲区 buf，
    ///     最多不超过 buf 长度和数据量
    pub fn read<T, V>(&self, offset: usize, f: impl FnOnce(&T) -> V) -> V {
        f(self.get_ref(offset))
    }

    /// 提供一个更易用的接口来修改缓存中的数据结构。
    pub fn modify<T, V>(&mut self, offset: usize, f: impl FnOnce(&mut T) -> V) -> V {
        f(self.get_mut(offset))
    }

    /// 同步函数，将脏的缓存块写到磁盘。
    /// 实现写回（Write-Back）缓存策略的核心。
    pub fn sync(&mut self) {
        if self.modified {  // 加一个判断，减少无效写，性能优化
            self.modified = false;
            self.block_device.write_block(self.block_id, &self.cache);
        }
    }
}


/// 实现 RAII，当一个 BlockCache 的 Arc 引用计数变为0，其生命周期结束时，`drop` 中调用
///  `self.sync()` 确保了任何未保存的修改都会被自动写回磁盘。
/// 
///@ drop trait 实现后，生命周期结束会自动回收？
/// 无论你是否为你的类型实现 Drop trait，当一个对象的生命周期结束时，它占用的内存【都】会被
/// Rust 自动回收。 impl Drop 的作用是让你能够在内存被回收之前，执行一些你自定义的清理逻辑。
/// 例如，当你离开房间，电灯自动关闭。
impl Drop for BlockCache {
    fn drop(&mut self) {
        self.sync()
    }
}

/// 有没有可能出现队列已满，且其中所有的块缓存都正在使用的情形呢？
/// 只要我们的上限 BLOCK_CACHE_SIZE 设置的足够大，超过所有应用同时访问的块总数上限，
/// 那么这种情况永远不会发生。但是，如果我们的上限设置不足，内核将 panic （基于简单内核设计的思路）。
const BLOCK_CACHE_SIZE: usize = 16;

/// 块缓存管理器，负责全局管理所有活动的 BlockCache 实例。
/// 它的设计目标是：
/// 1. 作为访问块缓存的统一入口，避免上层代码直接创建和管理 BlockCache。
/// 2. 实现缓存替换算法，在有限的内存空间内容纳最可能被访问的磁盘块。
pub struct BlockCacheManager {
    /// 使用双端队列（VecDeque）来维护缓存块。
    /// 选用 VecDeque 是因为它能高效地在队尾添加元素（新加载的块）和从队头查找并移除元素，
    /// 这符合类FIFO（先进先出）替换算法的需求。
    /// 
    /// 元组 `(usize, Arc<Mutex<BlockCache>>)` 中：
    /// - `usize`: 存储 block_id，用于快速查找，避免在查找时锁住Mutex。
    /// - `Arc<Mutex<BlockCache>>`:
    ///   - `Arc`: 允许多处代码（管理器自身和请求缓存的用户）共享对同一个BlockCache的所有权。
    ///   - `Mutex`: 提供了内部可变性和线程安全。即使外部持有的是不可变引用 `&BlockCacheManager` 或 `Arc<...>`，
    ///     也能安全地修改内部的 BlockCache 或管理器队列。
    queue: VecDeque<(usize, Arc<Mutex<BlockCache>>)>,
}

impl BlockCacheManager {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// 获取指定 block_id 的块缓存。
    /// 这是块缓存层提供给文件系统其他部分的核心接口。
    pub fn get_block_cache(
        &mut self,
        block_id: usize,
        block_device: Arc<dyn BlockDevice>,
    ) -> Arc<Mutex<BlockCache>> {
        // 1. 缓存命中：如果在队列中找到了对应 block_id 的缓存。
        if let Some(pair) = self.queue.
            iter().
            find(|pair| pair.0 == block_id) 
        {
            // 直接克隆 Arc 并返回。Arc::clone() 操作非常轻量，仅增加引用计数，
            // 使得调用者也获得了该缓存的所有权。
            Arc::clone(&pair.1)
        } 
        // 2. 缓存未命中：需要从磁盘加载新的块。
        else { 
            if self.queue.len() == BLOCK_CACHE_SIZE {
                // 缓存已满，执行替换
                // 类 FIFO：从队头（最旧）开始查找第一个可以被替换的块。
                // “可以被替换”的判断标准是强引用计数为1，说明只有管理器自身持有该缓存的引用，
                // 没有任何外部代码正在使用它，因此可以安全地换出。
                if let Some((idx, _)) = self
                    .queue
                    .iter()
                    .enumerate()
                    .find(|(_, pair)| Arc::strong_count(&pair.1) == 1) 
                {
                    self.queue.drain(idx..=idx);
                } 
                // 如果所有缓存块的引用计数都 > 1，意味着它们都在被使用，超过所有应用能同时
                // 访问的块总数上限，因此直接 panic(基于简单内核设计的思路)。
                else {
                    panic!("Run out of BlockCache")
                }
            }
            // 加载新的块到内存中，并用 Arc 和 Mutex 包装起来。
            let block_cache = Arc::new(Mutex::new(BlockCache::new(
                block_id,
                Arc::clone(&block_device),
            )));
            // 加入队列末尾
            self.queue.push_back((block_id, Arc::clone(&block_cache)));
            block_cache
        }
    }
}

lazy_static! {
    pub static ref BLOCK_CACHE_MANAGER: Mutex<BlockCacheManager> = 
        Mutex::new(BlockCacheManager::new());
}

/// 提供一个全局的、更简洁的函数来获取块缓存。
pub fn get_block_cache (
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
) -> Arc<Mutex<BlockCache>> {
    BLOCK_CACHE_MANAGER
        .lock()
        .get_block_cache(block_id, block_device)
}

/// 将所有脏的缓存块同步到磁盘。
pub fn block_cache_sync_all() {
    let manager = BLOCK_CACHE_MANAGER.lock();
    for (_, cache) in manager.queue.iter() {
        cache.lock().sync();
    }
}