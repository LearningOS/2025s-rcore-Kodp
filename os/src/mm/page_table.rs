//! Implementation of [`PageTableEntry`] and [`PageTable`].

use super::{frame_alloc, FrameTracker, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    /// page table entry flags
    pub struct PTEFlags: u8 {
        /// Valid
        const V = 1 << 0;
        /// Readable
        const R = 1 << 1;
        /// Writable
        const W = 1 << 2;
        /// eXecutable
        const X = 1 << 3;
        /// User
        const U = 1 << 4;
        /// Global
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
/// PTE
pub struct PageTableEntry {
    /// bits of page table entry
    pub bits: usize,
}

impl PageTableEntry {
    /// Create a new page table entry
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }
    /// Create an empty page table entry
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    /// Get the physical page number from the page table entry
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }
    /// Get the flags from the page table entry
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }
    /// The page pointered by page table entry is valid?
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is readable?
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is writable?
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is executable?
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

/// page table structure
/// 页表结构：根页号，页帧
pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

/// Assume that it won't oom when creating/mapping.
impl PageTable {
    /// Create a new page table
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }
    /// Find PageTableEntry by VirtPageNum, create a frame for a 4KB page table if not exist
    /// 根据给定的虚拟页号 vpn，在多级页表树中查找对应的三级（叶子）页表项 (PTE)。
    /// 如果在查找过程中发现任何中间级别的页表节点不存在（即父级 PTE 无效），它会自动分配
    /// 一个新的物理页帧来创建该节点，并更新父级 PTE 使其指向新节点。
    /// 这个方法常用于建立新的虚拟地址映射时，确保页表路径完整。
    // 1. 首先获取 `vpn` 的三级索引。
    // 2. 从根页表（一级页表，其位置由 `self.root_ppn` 给出）开始。
    // 3. 循环遍历页表层级（一级 -> 二级 -> 三级）。在每一级：
    //     - 使用当前级别的物理页号 (`ppn`) 获取该级别页表的内存内容（作为一个 PTE 数组），并通过当前级别的索引 (`idxs[i]`) 定位到相应的 PTE。
    //     - 如果是最后一级（三级），就找到了目标 PTE，保存其引用并退出循环。
    //     - 如果是中间级别：检查该 PTE 是否有效 (`is_valid()`)。如果无效，说明路径断了。
    //     - 如果路径断了 (`!pte.is_valid()`)，**分配**一个新的物理页帧，将父级 PTE 更新为指向这个新帧并标记为有效，
    //       将新帧的 `FrameTracker` 添加到 `self.frames` 列表进行跟踪。然后继续循环，现在 `ppn` 已经更新为指向新创建的下一级页表。
    //     - 如果 PTE 有效，就获取它指向的下一级页表的物理页号 (`pte.ppn()`)，并在下一次循环中使用它。
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes(); // 1. 获取虚拟页号的各级索引
        let mut ppn = self.root_ppn; // 2. 从根页表的物理页号开始查找
        let mut result: Option<&mut PageTableEntry> = None; // 用于存储最终找到的三级页表项的可变引用

        // 3. 循环遍历页表级别 (i=0为一级, i=1为二级, i=2为三级)
        for (i, idx) in idxs.iter().enumerate() {
            // 4. 获取当前级别的页表，并根据索引找到对应的页表项 (PTE)
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 { // 5. 如果是最后一级 (三级页表)
                result = Some(pte); // 这是我们要找的最终页表项，保存其引用
                break;
            }
            // 6. 如果是中间级别 (一级或二级)，检查当前 PTE 是否有效 (是否指向下一级页表)
            if !pte.is_valid() {
                // 7. 如果 PTE 无效，说明下一级页表节点不存在，需要创建它
                let frame = frame_alloc().unwrap();
                // 重要：新分配的物理页帧需要被清零，以确保其内的所有 PTE 最初都是无效的！
                // 这段代码片段中缺少清零操作，但在实际内核中通常需要：frame.ppn.get_bytes_array().fill(0);
                // 8. 更新当前 PTE，使其指向新分配的物理页帧，并标记为有效
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                // 9. 将新分配的页帧（用 FrameTracker 包装）添加到 PageTable 的 frames 列表中
                // 这确保了当 PageTable 被销毁时，这个页帧也会被自动回收 (RAII)
                self.frames.push(frame); // 栈上分配一个页帧给它，可能是新的，也可能是回收的。新的比较“连续”，回收的比较“分散”。
            }
            // 10. 移动到下一级：获取当前 PTE 指向的物理页号 (即下一级页表的物理地址)，用于下一次循环
            ppn = pte.ppn();
        }
        result
    }
    /// Find PageTableEntry by VirtPageNum
    /// 根据给定的虚拟页号 vpn 在多级页表树中查找对应的三级（叶子）页表项 (PTE)。
    /// 如果发现任何中间级别的页表节点无效，它不会创建新的节点，
    /// 而是立即判断映射不存在，并返回 None。
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
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
    /// 在当前的页表 (self) 中，为虚拟页号 VPN 建立一个到物理页号 PPN 的映射，
    /// 并设置该映射的权限和状态标志 flags。
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        // 1. 查找或创建通往 vpn 对应三级页表项的路径，并获取该三级页表项的可变引用
        let pte = self.find_pte_create(vpn).unwrap();
        // 调用 find_pte_create 会根据 vpn 索引遍历页表树。
        // 如果路径上某个中间节点（二级或三级页表页）不存在，它会分配新的物理页帧并创建这些节点，更新父级 PTE 指向新节点。
        // 它返回目标三级页表项 (PTE) 的可变引用。
        // unwrap() 表示，如果 find_pte_create 内部发生错误（比如分配物理页帧失败并 panic），这里的 map 方法也会跟着 panic。

        // 2. 断言检查：确保该虚拟页号 vpn 在调用 map 之前是未被映射的
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);

        // 3. 建立新的映射：用新的 PTE 覆盖掉旧的 (无效的) 三级页表项
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        // PageTableEntry::new(ppn, flags | PTEFlags::V) 创建一个新的 PageTableEntry 实例：
        //   - ppn: 将要映射到的物理页号。
        //   - flags | PTEFlags::V: 将传入的权限和状态标志 (flags) 与 有效位 (PTEFlags::V) 进行按位或操作。
        //     有效位必须设置，MMU 才能识别这个映射。
    }
    /// remove the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        // 1. 查找通往 vpn 对应三级页表项的路径，并获取该三级页表项的可变引用
        let pte = self.find_pte(vpn).unwrap();

        // 2. 断言检查：确保该虚拟页号 vpn 在调用 unmap 之前是有效的 (已被映射)
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);

        // 3. 移除映射：用一个空的 (无效的) PTE 覆盖掉旧的 (有效的) 三级页表项
        *pte = PageTableEntry::empty();
    }
    /// get the page table entry from the virtual page number
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    /// get the token from the page table
    pub fn token(&self) -> usize {
        8usize << 60 | self.root_ppn.0
    }
}

/// Translate&Copy a ptr[u8] array with LENGTH len to a mutable u8 Vec through page table
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
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
