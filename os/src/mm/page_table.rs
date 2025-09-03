//! PageTableEntry 和 PageTable 的实现

use super::{frame_alloc, FrameTracker, PhysPageNum, PhysAddr, 
    StepByOne, VirtAddr, VirtPageNum};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    /// 页表项控制位
    pub struct PTEFlags: u8 {
        /// valid
        const V = 1 << 0;
        /// readable
        const R = 1 << 1;
        /// writable
        const W = 1 << 2;
        /// executable
        const X = 1 << 3;
        /// user
        const U = 1 << 4;
        ///@ global 全局位。全局映射，在所有地址空间中都存在。
        const G = 1 << 5;
        /// Accessed
        const A = 1 << 6;
        /// Dirty
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
pub struct PageTableEntry {
    /// 二进制内容
    pub bits: usize,
}

impl PageTableEntry {
    /// 创建一个新的页表项；将物理页号左移10位，然后和标志位与。
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }

    /// 创建一个空的、无效的页表项
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }

    /// 从页表项中提取物理页号  
    /// 去掉低 10 位的标记位得到 44 位物理页号
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()

        //@ into 和 from 在实现了什么的时候能用？
        // 如果你为类型 U 实现了 From<T>，那么类型 T 会自动获得 Into<U> 的实现。
        // - 实现 From。
        // - from 通过 T::from(value) 调用。
        // - into 通过 value.into() 调用，更常用。
    }

    /// 从页表项中提取标志位
    /// 将 bits 转换为 u8（截断），然后使用 from_bits 创建 PTEFlags 实例。
    /// usize 转 u8 会截断，只保留低 8 位。
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }

    /// 检查页表项（PTE）是否有效。
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }

    /// 检查该页是否可读。
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }

    /// 检查该页是否可写。
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }

    /// 检查该页是否可执行。
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

/// 页表
pub struct PageTable {
    /// 根页表的物理页号
    root_ppn: PhysPageNum,
    
    /// frames 管理页表拥有的所有物理页号的列表
    /// 
    /// FrameTracker 是一个 RAII 包装器，当 PageTable 被销毁时，
    /// frames 这个 Vec 会被销毁，从而自动调用其中所有 FrameTracker 的 drop 方法，
    ///  实现该多级页表所有页帧的自动回收，避免内存泄漏。
    frames: Vec<FrameTracker>,
}

///@ 这里假设了在创建/映射时不会发生内存不足 (oom)?
/// 是的，因为 frame_alloc().unwrap() 是一个明确的信号。frame_alloc() 函数本身
/// 返回一个 Option<FrameTracker>：
/// - Some(frame): 成功分配了一个页帧。
/// - None: 内存不足，分配失败。
/// 代码中直接使用了 .unwrap()，它的作用是：如果结果是 Some，就取出里面的值；
/// 如果结果是 None，系统会立即 panic（崩溃）。
impl PageTable {
    /// 创建一个新的空页表，分配一个根页。
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap(); // 分配根页
        // extern "C" {
        //     fn ekernel();
        // }
        // println!("ekernel at {:#x}", ekernel as usize);
        // warn!("The root page table frame address: {:#x?}", PhysAddr::from(frame.ppn));
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],  // 将页帧的所有权转移给 PageTable
        }
    }

    /// 从 satp 寄存器值来创建一个专门用于手动查的页表，
    ///     其 frames 字段为空，即不实际管理任何资源。
    pub fn from_token(satp: usize) -> Self {
        // satp 是 RISC-V 中的一个控制寄存器，它存储了根页表的物理页号和分页模式。
        Self {
            // 从 satp 中提取 PPN
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }

    /// “手动”通过虚拟页号查找对应的页表项 (PTE)。如果中间页表不存在，则创建它们。
    /// 这是为了方便后面的实现。
    /// 
    /// 返回页表项的引用（地址）。如果不存在，则返回 None。
    /// 
    /// 1. 从 vpn 获取三级页表索引。
    /// 2. 从根页表开始，逐级向下遍历。
    /// 3. 在每一级，检查 PTE 是否有效 (is_valid)。
    /// 4. 如果无效，说明下一级页表不存在。此时分配一个新的物理页帧作为下一级页表，
    ///    用 PTEFlags::V 更新当前 PTE，且让其指向新分配的页帧。
    /// 5. 将新分配的页帧加入 frames 列表。
    /// 6. 更新 ppn 为下一级页表的物理页号，继续循环。
    /// 7. 到达最后一级（第三级）时，返回最终数据页对应的 PTE 的可变引用。
    pub fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            // get_pte_array 返回物理页号对应的一整页的引用；页表页
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                let frame = frame_alloc().unwrap();
                // 不仅要更新物理页号，还要将标志位 V 置 1， 不然硬件在查多级页表的时候，
                // 会认为这个页表项不合法，从而触发 Page Fault 而不能向下走。
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
        //@ 用 Option 似乎没有什么意义，因为要么能找到（即使分配），要么分配失败 unwrap
        // 导致崩溃。那么，这么写可能只是为了和 find_pte 的 API 保持一致。
    }

    /// 通过虚拟页号查找对应的页表项 (PTE)。
    /// 返回页表项的引用（地址）。如果不存在，则返回 None。
    /// 
    /// 这是 find_pte_create 的只读版本。它不会创建新的页表，
    /// 在遍历过程中如果遇到无效的 PTE，就直接返回 None，表示映射不存在。
    pub fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;

        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }

    /// 建立虚拟页号 vpn 到物理页号 ppn 的映射：添加 vpn->ppn 到页表。
    /// 
    /// 为了 MMU 能够通过地址转换、正确找到应用地址空间中的数据实际被内核放在内存中的位置，
    /// 操作系统需要动态维护一个虚拟页号到页表项的映射，支持插入/删除键值对。这就是 map/unmap。
    /// 
    /// 1. 调用 find_pte_create 找到（或创建）vpn 对应的页表项（PTE）。
    /// 2. 使用 assert! 确保这个 PTE 之前是无效的，防止重复映射。
    /// 3. 写入 PTE，包含目标 ppn 和指定的 flags，并设置有效位 V。
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        // 经过 find_pte_create 的层层“探路”和“修路”，最终，它会返回一个指向最高级别
        //  页表项的引用（&mut PageTableEntry）。
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        //@ 这里 pte 是一个 &mut PTE， 用 *pte 向里面写相当于写那个 PTE 吗？
        // 是的，上面代码的含义是：
        // “计算出一个新的 PageTableEntry 值，然后把它覆盖写入到 pte 这个引用所指向的内存地址上”
    }

    /// 解除虚拟页号 vpn 的映射。
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }

    /// 返回 vpn 对应的页表项（拷贝），类型为 `Option<PageTableEntry>`
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        // map 将 &mut PageTableEntry 转换为 PageTableEntry
        self.find_pte(vpn).map(|pte| *pte)
    }

    /// 将一个虚拟地址（VirtAddr）翻译成对应的物理地址（PhysAddr）
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.find_pte(va.clone().floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    /// 返回用于写入 satp 寄存器的 token; 代表 Sv39 和根页表物理地址。
    /// 
    /// 设置 satp 寄存器的高 4 位为 8，这在 RISC-V 中代表使用 Sv39 分页模式。
    /// 低 44 位则存储根页表的物理页号。
    pub fn token(&self) -> usize {
        8usize << 60 | self.root_ppn.0
    }

}

/// 通过页表翻译一个裸指针（虚拟地址）指向的、长度为 len 的字节数组，返回一个分段的缓冲区。
/// 
/// 该函数将一个逻辑上连续、但物理上可能不连续的用户空间虚拟地址范围，转化为一个包含多个物理
/// 上连续的字节切片（引用）的向量。这些切片可以直接在内核空间中访问。该函数常用于在内核中
/// 安全地读取或修改用户空间的数据，例如 sys_write 和 sys_read 系统调用。
/// 
/// 由于内核和应用地址空间的隔离， sys_write 不再能够直接访问位于应用空间中的数据，而需要
/// 手动查页表才能知道数据被放置在哪些物理页帧上并进行访问。为此，页表模块 page_table 
/// 提供了将应用地址空间中一个缓冲区转化为在内核空间中能够直接访问的形式的辅助函数：
pub fn translated_byte_buffer(
    token: usize,    // 应用地址空间
    ptr: *const u8,  // 起始地址
    len: usize)      // 长度
-> Vec<&'static mut [u8]> {
    // 1. 从 token “重建”页表
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start + len;
    let mut v = Vec::new();

    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let ppn = page_table.translate(vpn).unwrap().ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    v
}

/// 基于页表 token，从裸指针安全地读取一个以 \0 结尾的字符串返回
pub fn translated_str(token: usize, ptr: *const u8) -> String {
    let page_table = PageTable::from_token(token);
    let mut string = String::new();
    let mut va = ptr as usize;
    loop {
    // 查询过程：
    // 1. 内核通过 token（即 satp 的值）得知用户 A 的根页表物理页号是 0xABCD。
    // 2. 内核将 0xABCD 转换为物理地址，例如 0x80000000 + 0xABCD * 4096 = 0x8ABC_D000。
    // 3. 因为内核空间是**恒等映射**的，所以内核直接使用 0x8ABC_D000 这个值作为虚拟地址去访问内存。
    // 4. MMU 通过内核自己的页表进行翻译，发现 VA(0x8ABC_D000) 确实映射到了 PA(0x8ABC_D000)。
    // 5. 内核成功读取了用户 A 的根页表内容。
    // 6. 内核从根页表中读出下一级页表的物理页号，比如 0xEFGH，然后重复上述过程，用虚拟地址 0x8EFG_H000 去访问它。
        let ch: u8 = *(page_table
            .translate_va(VirtAddr::from(va))
            .unwrap()
            .get_mut()
        );
        if ch == 0 {
            break;
        } 
        string.push(ch as char);
        va += 1;
    }
    string
}


#[allow(unused)]
/// 基于页表 token，将一个裸指针翻译成一个内核空间可用的不可变引用
pub fn translated_ref<T>(token: usize, ptr: *const T) -> &'static T {
    let page_table = PageTable::from_token(token);
    page_table
        .translate_va(VirtAddr::from(ptr as usize))
        .unwrap()
        .get_ref()
}

/// 基于页表 token，将一个裸指针翻译成一个内核空间可用的可变引用
pub fn translated_refmut<T>(token: usize, ptr: *mut T) -> &'static mut T {
    let page_table = PageTable::from_token(token);
    let va = ptr as usize;
    page_table
        .translate_va(VirtAddr::from(va))
        .unwrap()
        .get_mut()
}

/// An abstraction over a buffer passed from user space to kernel space
pub struct UserBuffer {
    /// A list of buffers
    pub buffers: Vec<&'static mut [u8]>,
}

impl UserBuffer {
    /// Constuct UserBuffer
    pub fn new(buffers: Vec<&'static mut [u8]>) -> Self {
        Self { buffers }
    }
    /// Get the length of the buffer
    pub fn len(&self) -> usize {
        let mut total: usize = 0;
        for b in self.buffers.iter() {
            total += b.len();
        }
        total
    }
}

impl IntoIterator for UserBuffer {
    type Item = *mut u8;
    type IntoIter = UserBufferIterator;
    fn into_iter(self) -> Self::IntoIter {
        UserBufferIterator {
            buffers: self.buffers,
            current_buffer: 0,
            current_idx: 0,
        }
    }
}

/// An iterator over a UserBuffer
pub struct UserBufferIterator {
    buffers: Vec<&'static mut [u8]>,
    current_buffer: usize,
    current_idx: usize,
}

impl Iterator for UserBufferIterator {
    type Item = *mut u8;
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_buffer >= self.buffers.len() {
            None
        } else {
            let r = &mut self.buffers[self.current_buffer][self.current_idx] as *mut _;
            if self.current_idx + 1 == self.buffers[self.current_buffer].len() {
                self.current_idx = 0;
                self.current_buffer += 1;
            } else {
                self.current_idx += 1;
            }
            Some(r)
        }
    }
}
