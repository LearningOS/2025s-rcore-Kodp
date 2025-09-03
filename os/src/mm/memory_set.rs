//! 内存管理实现
//! 
//! 本文件实现了地址空间。
//! 核心设计思想是：
//! 1. 逻辑段（MapArea）：用一个结构体描述一段连续的、具有相同映射方式和权限的虚拟内存区间。
//! 2. 地址空间（MemorySet）：由一个页表（PageTable）和多个逻辑段（MapArea）组成，完整描述了一个进程的虚拟地址空间。
//! 3. RAII（资源获取即初始化）：通过 FrameTracker、MapArea 和 MemorySet 的嵌套所有权，确保当地址空间生命周期结束时，
//!    其所拥有的所有物理页帧都能自动且安全地被回收，避免内存泄漏

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{
    MEMORY_END, MMIO, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE,
    USER_STACK_SIZE,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// 内核地址空间
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
    // new_kernel() 初始化了内核地址空间
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}

/// kernel token
pub fn kernel_token() -> usize {
    KERNEL_SPACE.exclusive_access().token()
}

///@ 地址空间
/// 由一份多级页表 和 一系列逻辑段组成。
/// `page_table` 管理页表节点所在的物理页帧，`areas` 管理数据所在的物理页帧，
/// 两者结合形成一个完整的 RAII 容器，确保地址空间销毁时所有页帧都能被回收。
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
}

impl MemorySet {
    /// 添加一个给定地址范围并分配实际的物理页帧
    pub fn map(&mut self, start: usize, len: usize, prot: usize) -> isize {
        let start_va = VirtAddr::from(start);
        let end_va = VirtAddr::from(start + len);
        let vpn_range = VPNRange::new(start_va.floor(), end_va.ceil());

        // 如果新分配的区域和地址空间相交，则返回错误
        if vpn_range.into_iter().any(|vpn| {
            self.page_table
                .find_pte(vpn)
                .map_or(false, |pte| pte.is_valid())
        }) {
            return -1
        }

        let mut permission = MapPermission::U;
        if prot & 1 != 0 { permission |= MapPermission::R }  // 用户 1,2,4，对应的是R,W,X
        if prot & 2 != 0 { permission |= MapPermission::W }
        if prot & 4 != 0 { permission |= MapPermission::X }

        println!("start_va:{:#x}, end_va:{:#x}, map_perm:{:#x}", start, start+len, permission);

        // 分配记录区间
        self.insert_framed_area(start_va, end_va, permission);
        0
    }

    /// 回收一个给定的地址范围并回收已分配的页帧
    pub fn munmap(&mut self, start: usize, len: usize) -> isize {
        // 1. 将 start 和 len 转换为一个虚拟页号的范围 vpn_range
        let start_va = VirtAddr::from(start);
        let end_va = VirtAddr::from(start + len);
        let vpn_range = VPNRange::new(start_va.floor(), end_va.ceil());
        
        // 2. 逐个遍历这个范围中的每一个 vpn。
        for vpn in vpn_range {
            // 3. 对于每一个 vpn，它必须：
            //  a.  找到这个 vpn 属于哪个逻辑内存区域（MapArea）。
            //  b.  确认这个 vpn 确实在页表中有一个有效的映射。
            //  c.  如果以上两点都满足，就执行真正的解除映射操作。
            // 如果在整个过程中任何一个 vpn 的处理失败，整个 munmap 操作就失败。
            let mut found = false;
            // 尝试找到这个 vpn 对应的 MapArea
            for area in &mut self.areas {
                if vpn >= area.vpn_range.get_start() && vpn < area.vpn_range.get_end() {
                    // 找到了！现在检查页表是否真的有映射
                    let pte = self.page_table.find_pte(vpn);
                    // 如果 MapArea 有记录，则表明该页有分配。但页表中却查不到条目，或条目
                    // 无效，这说明内存状态出现矛盾，立即返回错误。
                    if pte.is_none() || !pte.unwrap().is_valid() {
                        return -1; 
                    }
                    
                    // 执行释放操作
                    area.unmap_one(&mut self.page_table, vpn);
                    found = true;
                    break; // 找到后立即跳出内层循环
                }
            }
            // 如果内层循环结束，但还没有找到该 vpn 对应的 MapArea，则返回错误
            // 因为在尝试 unmap 一个不属于任何 MapArea 的 vpn
            if !found {
                return -1;
            }
        }
        // 如果所有页面都成功处理，返回 0
        0
    }
    /// 新建一个空的地址空间
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }

    /// 获取用于写入 satp 寄存器的地址空间 token
    pub fn token(&self) -> usize {
        self.page_table.token()
    }

    /// 插入一个以 Framed 方式映射的逻辑段。
    /// 主要用于插入那些不需要初始数据的内存区域，如 .bss 段或栈。
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        )
    }

    /// 删除一个 area
    /// 
    ///@ 参数什么时候用 &mut self，什么时候用 &self？&Self 和 &self 有区别？可以用 self 吗？
    /// Self 才代表当前类型名；
    /// self 是 self: Self 的语法糖
    /// &self 是 self: &Self 的语法糖
    /// &mut self 是 self: &mut Self 的语法糖
    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some((idx, area)) = self
            .areas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.vpn_range.get_start() == start_vpn)
        {
            area.unmap(&mut self.page_table);
            self.areas.remove(idx);
        }
    }

    /// 给定虚拟地址范围 map_area，插入到当前地址空间（含页表）中。
    /// 
    /// 隐含的假设：提供 data，则 map_area 必须以 Framed 方式映射
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        // 关键一步：在自己的页表中建立从虚拟地址到物理页帧的映射；活让 map_area 干。

        map_area.map(&mut self.page_table);
        // 如果提供了初始化数据，则将其拷贝到新分配的物理页帧中
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }

    /// 在当前的页表中，建立一个映射：最高虚拟地址页面->存放跳板代码的物理页。
    /// 
    /// 跳板页是一个特殊的、位于最高虚拟地址空间的页面，用于在 S 态和 U 态之间进行上下文切换。
    /// 因为它在内核和所有应用地址空间中都必须存在，所以需要单独进行映射。
    fn map_trampoline(&mut self) {
        
        self.page_table.map(
            // TRAMPOLINE 是虚拟地址最后一个页面的起点。
            VirtAddr::from(TRAMPOLINE).into(),

            // strampoline 是跳板代码的物理起始地址，即 __alltraps 所在的地址。
            PhysAddr::from(strampoline as usize).into(),

            PTEFlags::R | PTEFlags::X
        );
    }

    /// 创建一个完整的内核地址空间
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        //@ 为内核映射跳板页
        memory_set.map_trampoline();
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );

        info!("mapping .text section");
        // 将  [stext, etext) 加入自己的页表，如此，访问任意 [stext, etext) 的虚拟地址
        // 都等同于访问 [stext, etext) 的物理地址
        memory_set.push(
            // 建立 [stext, etext) 地址范围，恒等映射
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        info!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        info!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping physical memory");
        // MapArea::new 仅仅是定义了映射的范围和属性，并没有实际修改页表
        // 真正的映射工作是在 push 函数内部完成的
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );

        // 映射外设对应的地址空间
        info!("mapping memory-mapped registers");
        for pair in MMIO {
            memory_set.push(
                MapArea::new(
                    (*pair).0.into(),
                    ((*pair).0 + (*pair).1).into(),
                    MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            );
        }
        memory_set
    }

    /// 从 ELF 中构建应用地址空间。
    /// 该函数返回的应用地址空间、栈虚拟地址、应用入口点地址，会被用于创建应用的任务控制块。
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        //@ 1. 为用户程序映射跳板页
        memory_set.map_trampoline();


        // 2. 映射 ELF 文件的程序头
        // 使用外部 crate xmas_elf 来解析传入的应用 ELF 数据
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        // 取出 ELF 的魔数来判断它是不是一个合法的 ELF 
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");

        // 得到 program header 的数目
        let ph_count = elf_header.pt2.ph_count();
        // 记录目前涉及到的最大虚拟页号
        let mut max_end_vpn = VirtPageNum(0);

        // 遍历所有的 program header 并将合适的区域加入到应用地址空间中
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            // 只处理类型为 LOAD 的程序头，这些是需要加载到内存中的代码和数据
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();

                // 根据 ELF 权限标志（R/W/X），设置逻辑段权限，并默认包含 U 标志，表示用户态可访问
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }

                // 用户数据都是用 Framed 方式映射的，是分配的。
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                // 将逻辑段插入地址空间，并填充对应的 ELF 数据
                memory_set.push(
                    map_area,
                    Some(&elf.input[ph.offset() as usize .. (ph.offset() + ph.file_size()) as usize]),
                )
            }
        }

        // 3. 映射用户栈，并在其下方插入一个保护页，能防止栈溢出时覆盖其他内存区域。
        let max_end_va: VirtAddr = max_end_vpn.into();  // 最后一页的下一页起始地址
        let mut user_stack_bottom: usize = max_end_va.into();
        user_stack_bottom += PAGE_SIZE; // 保护页
        // 用户栈放置在用户数据的上面的保护页的上面
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U
            ),
            None,
        );

        // 4. 映射一个空的 sbrk 区域，用于支持 sbrk 系统调用动态扩展堆内存。
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U
            ),
            None,
        );

        // 5. 映射 Trap 上下文页。这个页面用于在系统调用或中断发生时，保存和恢复
        // 用户态寄存器的状态。
        // 在应用地址空间中映射次高页面来存放 Trap 上下文。
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT_BASE.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None
        );

        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize,
        )
    }

    ///? 从一个用户地址空间创建一个新的用户地址空间
    pub fn from_existed_user(user_space: &Self) -> Self {
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        for area in user_space.areas.iter() {
            let new_area = MapArea::from_another(area);
            memory_set.push(new_area, None);
            for vpn in area.vpn_range {
                let src_ppn = user_space.translate(vpn).unwrap().ppn();
                let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                dst_ppn
                    .get_bytes_array()
                    .copy_from_slice(src_ppn.get_bytes_array());
            }
        }
        memory_set
    }

    /// 激活当前虚拟地址空间。
    /// 将当前页表的根物理页号写入 `satp` 寄存器，并刷新 TLB，从而开启分页模式。
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            asm!("sfence.vma");
        }
    }

    /// 将一个虚拟页号翻译为对应的页表项
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// 删除 MapArea
    pub fn recycle_data_pages(&mut self) {
        self.areas.clear();
    }

    /// 缩小一个逻辑段
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        // 1. 查找与给定起始地址匹配的 MapArea。
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            // 2. 如果找到，调用 MapArea 的 shrink_to 方法，收缩范围到 [start, new_end)
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// 扩展一个逻辑段
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        // 1. 查找与给定起始地址匹配的 MapArea。
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            // 2. 如果找到，调用 MapArea 的 append_to 方法，增加范围到 [start, new_end)
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }
}

/// 一个连续的虚拟地址区间，为页表 PageTable 搞服务的，可以为任意页表映射自身的地址区间。
/// 含有一段连续虚拟地址区间的端点、映射方式、访问权限、实际数据的物理页号。
pub struct MapArea {
    /// 逻辑段虚拟页号范围
    vpn_range: VPNRange,
    /// 存储数据的物理页帧，键为虚拟页号
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    /// 映射方式
    map_type: MapType,
    /// 访问权限
    map_perm: MapPermission,
}

impl MapArea {
    /// 新建一个逻辑段
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        // 将地址转为页号，之后遍历的是页号！
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        // end_vpn 在迭代中不会访问。 [start_vpn, end_vpn)
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }

    /// 复制一份
    pub fn from_another(another: &Self) -> Self {
        Self {
            vpn_range: VPNRange::new(another.vpn_range.get_start(), another.vpn_range.get_end()),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
        }
    }

    /// 映射单个虚拟页。
    /// 这个函数根据映射类型，决定是否分配新的物理页帧，并将其插入页表。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0)  // ppn = vpn, 无须分配物理页
                //@ 恒等映射，虚拟地址等于物理地址。
                // 一段物理内存包含一些物理页，这些物理页号组成一个集合 A
                // 我们在页表里添加 A->A。
            }
            // 分配一页并写入数据
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                // 插入 vpn->FrameTracker
                // 将新分配的 FrameTracker 的所有权转移给 MapArea
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();

        // 关键一步：调用页表的 map 接口写入映射
        page_table.map(vpn, ppn, pte_flags);
    }

    /// 解除单个虚拟页的映射
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            // 如果含有 Framed 物理页帧，则移除 FrameTracker，其 drop 方法会自动回收页帧
            //? 为什么 BTreeMap remove 其中一个元素会让其内部元素 drop？ 
            self.data_frames.remove(&vpn);
        }
        // 调用页表的 unmap 接口解除映射
        page_table.unmap(vpn);
    }

    /// 映射整个逻辑段：遍历整个虚拟页号范围 vpn_range，并对每个虚拟页号调用 map_one 方法。
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }

    /// 解除映射整个逻辑段
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }

    /// 缩小逻辑段。
    /// 通过缩小结束地址来缩小；起始地址不变。
    #[allow(unused)]
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        // 将后半部分的映射解除
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn);
        }
        // 更新范围
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end)
    }

    /// 扩展逻辑段
    /// 通过增大结束地址来库扩展；起始地址不变。
    #[allow(unused)]
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn);
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }

    ///? 将给定数据复制到自身逻辑段所对应的物理页中。
    /// 这个函数能够正确地将逻辑上连续的数据，按页分段并复制到物理上可能不连续的页帧中。
    /// 复制到前面对齐。
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        //@ 为什么要是 Framed
        // 只有动态分配出来的帧，我们才认为可以自由修改、写数据。如果是恒等映射的，我们
        // 不应该修改。
        assert_eq!(self.map_type, MapType::Framed);  
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();

        loop {
            // 1. 获取源数据切片，长度<=一页（防止越过 len）
            let src = &data[start..len.min(start + PAGE_SIZE)];

            // 2. 通过页表翻译找到目标物理页帧，并获取可变字节数组
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            
            // 3. 复制
            dst.copy_from_slice(src);

            // 4. 移动到下一个虚拟页
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();
        }
    }

}

/// 映射方式
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    /// 恒等映射，虚拟地址与物理地址相同
    Identical,
    /// 映射到新分配的物理地址
    Framed,
}

bitflags! {
    /// 逻辑段访问权限
    /// 它是页表项标志位 PTEFlags 的一个子集，仅保留 U/R/W/X 四个标志位，
    /// 因为其他的标志位仅与硬件的地址转换机制细节相关，这样的设计能避免引入错误的标志位。
    pub struct MapPermission: u8 {
        /// 可读
        const R = 1 << 1;
        /// 可写
        const W = 1 << 2;
        /// 可执行
        const X = 1 << 3;
        /// 用户态可访问
        const U = 1 << 4;
    }
}


/// 一个简单的测试函数，用于验证内核地址空间的映射和权限设置是否正确。
/// 它检查内核的 .text、.rodata 和 .data 段的写权限和执行权限是否符合预期。
#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();

    // 检查 .text 段不可写
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable()
    );
    // 检查 .rodata 段不可写
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable()
    );
    // 检查 .data 段不可执行
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable()
    );
    println!("remap_test passed!");
}
