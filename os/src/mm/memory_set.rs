//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{
    KERNEL_STACK_SIZE, MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE,
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
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}
/// 地址空间
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
}

impl MemorySet {
    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }
    /// Get the page table token
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    ///@ push 将一个MapArea push到自己的areas，如果有数据就写入数据到内存。 
    /// 把MapArea内的每个都页都放入页表（map）
    /// 对于每个应用程序，其trampoline虚拟地址对应的物理页号也都放入了自己的地址空间，这样它才能跳转到trap处理程序
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }
    /// 将跳板页面映射到地址空间.
    /// 注意: 跳板页面不由 MemoryArea 管理.
    fn map_trampoline(&mut self) {

        //@ 将TRAMPOLINE->strampoline放入页表
        self.page_table.map(
            // 虚拟地址: TRAMPOLINE (最高虚拟页,说最高是因为 TRAMPOLINE 定义为 usizeMAX - 1Page)
            VirtAddr::from(TRAMPOLINE).into(),
            // 物理地址: strampoline 符号的地址 (代码所在物理页帧)
            PhysAddr::from(strampoline as usize).into(),
            // 权限: 可读 (R) | 可执行 (X)
            PTEFlags::R | PTEFlags::X,
        );
    }
    ///@ 建立页表恒等映射 (Identity Mapping)
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        memory_set.push(
            MapArea::new(
                // 指定虚拟地址范围
                (stext as usize).into(),
                (etext as usize).into(),
                // 指定恒等映射
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
        // ekernel 到 MEMORY_END 的映射，使内核可以用简单的、与物理地址相同的虚拟地址来访问任意物理内存（例如，用于物理页帧分配、访问设备寄存器等）。
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        // 上述一通push完成，当系统启用分页后：
        // CPU 产生的任何虚拟地址，如果落在了 .text, .rodata, .data, .bss 或 ekernel 到 MEMORY_END 的范围内，
        // MMU 通过查询内核页表进行地址转换时，找到的物理地址与该虚拟地址完全相同。
        // 恒等映射也保证了我们启动分页的平滑过渡：启动分页前最后一条指令和启动分页后第一条指令连续。

        //? 问题：恒等映射的这些页表占多少空间？
        // 含这个问题的上下文的ai： https://gemini.google.com/app/51ce53f706953582
        memory_set
    }
    /// Include sections in elf and trampoline and TrapContext and user stack,
    /// also returns user_sp_base and entry point.
    /// 解析 ELF 文件，根据其中的信息创建用户程序的代码、数据、栈、Trap 上下文等内存区域，
    /// 并建立这些区域对应的页表映射。
    /// 返回一个元组，包含：
    ///     - 构建好的 MemorySet 实例，代表用户程序的地址空间。
    ///     - 用户栈的栈顶虚拟地址，作为用户程序启动时的栈指针。
    ///     - 用户程序的入口点虚拟地址，即程序开始执行的第一条指令的地址。
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        // 跳板页用于用户态和内核态之间的切换。它在所有用户地址空间中映射到TRAMPOLINE位置
        memory_set.map_trampoline();

        // map program headers of elf, with U flag
        // 使用 xmas_elf 库解析输入的 ELF 文件数据。
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        // 检查 ELF 文件头的 magic number ([0x7f, 'E', 'L', 'F'])，确保是有效的 ELF 格式。
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);

        // 遍历 ELF 文件中的程序头 (Program Headers)，程序头描述了 ELF 文件中各个段（如代码段、数据段）应该如何被加载到内存中。
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            // 只处理类型为 'Load' 的程序头，其要被加载到虚拟地址空间并建立映射的。
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                // 获取该加载段在虚拟地址空间中的起始地址和结束地址 (基于其内存大小)。
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();

                // 根据 ELF 段的标志 (flags) 确定该内存区域的访问权限。
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
                // 创建一个 MapArea 实例，添加到 MemorySet 中，并提供段的初始数据。
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();  // 最大虚拟页号
                // push 方法会执行以下操作：
                //      - 调用 map_area.map()：遍历 MapArea 范围内的虚拟页，
                //        为每个页分配物理页帧 (因为 MapType::Framed)，并在页表中建立 vpn -> ppn 的映射。
                //      - 调用 map_area.copy_data()：将 ELF 文件中该段的内容复制到刚刚分配的物理页帧中。
                //      - 将 map_area 添加到 memory_set.areas 列表中。
                
                memory_set.push(
                    map_area, // data: Some(&[u8])
                    Some(&elf.input[ph.offset() as usize .. (ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // map user stack with U flags
        // 映射用户栈区域。
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        // 在用户栈和 ELF 段之间留出一个页大小的未映射或特殊映射区域。
        // 这样，如果用户栈溢出，触碰到这个 Guard Page 会立即触发页错误，而不是覆盖到前面的代码/数据段。
        // 这里通过简单地将栈底地址跳过一个页大小PAGE_SIZE来实现，Guard Page 本身没有被 push 到 areas 中，因此是未映射的。
        user_stack_bottom += PAGE_SIZE;

        // 计算用户栈的栈顶地址 (栈从高地址向低地址增长，所以栈顶地址 > 栈底地址)
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        
        // 创建并映射用户栈的 MapArea。
        //    映射类型为 Framed (需要分配新的物理页帧作为栈空间)。
        //    权限为 用户可读写 (R | W | U)，用户栈不需要可执行。
        //    初始数据为 None，因为栈内容由程序运行时动态生成。
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // 映射 sbrk 区域 (用于支持动态堆扩展)。
        //     通常在用户栈之后放置一个零大小的 MapArea，标记动态内存分配（如堆）的起始位置。
        //     用户程序通过 sbrk 等系统调用请求更多内存时，OS 会扩展这个 MapArea
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),  // sbrk 区域起始地址，通常紧随用户栈顶
                user_stack_top.into(),  // sbrk 区域结束地址，初始大小为 0
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,  // 用户可读写权限
            ),
            None,
        );
        // 映射 Trap 上下文 (TrapContext) 区域。
        //     TrapContext 用于在用户/内核模式切换时保存用户态的 CPU 寄存器状态。
        //     它被放置在用户地址空间中一个固定的高地址 (TRAP_CONTEXT_BASE)，紧邻跳板下方。
        //     这样在内核处理 Trap 时，可以通过一个固定的虚拟地址访问到保存的用户上下文。
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT_BASE.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        // 返回构建好的 MemorySet、初始用户栈顶地址和程序入口点。
        //     操作系统调度器在第一次运行这个用户进程时会使用这些信息：
        //     - 将 memory_set 的根页表地址加载到 satp 寄存器。
        //     - 将 user_stack_top 加载到用户栈指针寄存器 (如 sp)。
        //     - 将 entry_point 加载到程序计数器 (PC)，开始执行用户程序。
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize, // 用户进程执行的第一个指令地址
        )
    }
    /// Change page table by writing satp CSR Register.
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            // 从执行 satp::write 指令的时刻起，SV39 分页模式就被启用了，MMU 开始使用内核地址空间的多级页表进行后续的地址转换。
            satp::write(satp);
            asm!("sfence.vma");
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }
}
/// map area structure, controls a contiguous piece of virtual memory
pub struct MapArea {
    vpn_range: VPNRange,
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    /// 建立单页VPN的映射：向自身添加VPN->PPN（直接映射）、向页表里添加一个VPN->PPN（三级映射）。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        //1. 更新data_frames
        match self.map_type {
            MapType::Identical => {
                // 不向data_frames里推数据（恒等映射）
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap(); // 自己分配一个物理页
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        // 页表内添加vpn->ppn的映射
        page_table.map(vpn, ppn, pte_flags);
    }
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    #[allow(unused)]
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    /// 向已经分配并映射好的物理页帧中写入数据
    /// 通常是在MapArea 调用了 map 方法完成所有页的映射后（此时 map 内部多次调用了 map_one），data_frames 被填充了需要追踪的物理页帧信息，
    /// 然后才会调用 copy_data 来向这些已分配并映射的物理页写入数据。
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();  // +1, 一页一页的放数据
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// map type for memory set: identical or framed
pub enum MapType {
    Identical,
    Framed,
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        ///Readable
        const R = 1 << 1;
        ///Writable
        const W = 1 << 2;
        ///Excutable
        const X = 1 << 3;
        ///Accessible in U mode
        const U = 1 << 4;
    }
}

/// 根据 app_id 计算内核栈在内核地址空间中的位置 (bottom, top)
pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    // 内核栈位于跳板页下方，每个栈之间有一个 PADDING 页
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// remap test in kernel space
#[allow(unused)]
pub fn remap_test() {
    // 获取代码段、只读数据段、数据段中间位置的虚拟地址
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    // 检查页面是否不可写（代码段和只读数据段的页面不可写）
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    // 检查数据段页面不可执行
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
