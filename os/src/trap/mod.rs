//! Trap handling functionality
//! 
//! 包含 Trap 处理入口 trap_handler。
//! 
//! 对于 rCore 教程，我们设计了一个单一的中断入口点，即 `__alltraps`。
//! 在初始化函数 [`init()`] 中，我们将 `stvec` 控制状态寄存器 (CSR) 设置为指向这个入口点。
//!
//! ## 工作流程
//! 所有的中断、异常和系统调用 (统称为 Trap) 都会首先进入 `__alltraps`。
//! `__alltraps` 是在 `trap.S` 中用汇编语言定义的。汇编代码只做最少的工作：
//! 1. 保存用户态的所有寄存器（即 Trap 上下文）。
//! 2. 切换到内核栈。
//! 3. 将控制权转移给 Rust 函数 [`trap_handler()`]。
//!
//! [`trap_handler()`] 在安全的 Rust 环境中运行，它会根据 Trap 的具体原因进行分发。
//! 例如，系统调用 (syscall) 会被分发到 [`syscall()`] 函数处理，而不可恢复的硬件异常
//! (如非法指令) 会导致当前应用被终止。
mod context;

use crate::config::{BIG_STRIDE, TRAMPOLINE, TRAP_CONTEXT_BASE};
use crate::syscall::syscall;
use crate::task::{
    current_task, current_trap_cx, current_user_token, exit_current_and_run_next, suspend_current_and_run_next
};
use crate::timer::set_next_trigger;
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, stval, stvec,
};

global_asm!(include_str!("trap.S"));

/// 初始化内核中断向量入口
pub fn init() {
    set_kernel_trap_entry();
}

/// 初始化内核中断向量入口
fn set_kernel_trap_entry() {
    unsafe {
        stvec::write(trap_from_kernel as usize, TrapMode::Direct);
    }
}

/// 初始化用户中断向量入口（跳板页起始地址）
fn set_user_trap_entry() {
    unsafe {
        stvec::write(TRAMPOLINE as usize, TrapMode::Direct);
    }
}

/// 启动 S 模式的时钟中断
//@ 这是 RISC-V 硬件实现的中断吗？？就是设置一个寄存器，硬件按照寄存器值经过的时间发出中断？
// 是的。
pub fn enable_timer_interrupt() {
    unsafe {
        sie::set_stimer();
    }
}

// 用 no_mangle 防止编译器对函数名做处理。因为 trap_handler 在 trap.S 中被直接调用。
#[no_mangle]
/// 统一的 Trap 处理函数，从 __alltraps 中跳转过来。
pub fn trap_handler() -> ! {
    // 当从用户态进入中断时，首先将 stvec 设置为内核中断入口。
    // 这样，如果在内核处理中断的过程中再次发生中断（例如设备中断），
    // 程序会跳转到 trap_from_kernel，从而捕获内核自身的错误。
    set_kernel_trap_entry();
    let scause = scause::read();
    let stval = stval::read();

    match scause.cause() {
        // 1. 请求系统调用
        Trap::Exception(Exception::UserEnvCall) => {
            let mut cx = current_trap_cx();
            // sepc 向后移动一个指令位置，即 ecall 之后的指令。结束 Trap 后 CPU 会跳转该地址。
            cx.sepc += 4;
            // 调用 syscall 处理函数，并获取返回值。
            // 根据 RISC-V 调用约定：
            // - a7 (即 x[17]) 存放系统调用号。
            // - a0, a1, a2, a3 (即 x[10], x[11], x[12], x[13]) 存放系统调用的前四个参数。
            // - 系统调用的返回值会存放在 a0 (即 x[10]) 中
            let result = syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12], cx.x[13]]);
            // 因为当前任务可能被切换了（例如调用 sys_exec），所以我们需要再次调用 current_trap_cx
            // 获取当前任务的 Trap 上下文，并将返回值写入 a0 寄存器。
            cx = current_trap_cx();
            cx.x[10] = result as usize;
        }
        // 2. 存取错误
        // - `InstructionPageFault`: CPU 取指令时发生页错误。比如，`sepc` 指向一个未映射或无执行权限的虚拟地址。
        // - `InstructionFault`: 取指令时，页表转换成功，但在访问最终的物理地址时出错（如总线错误）。
        // - `Load/StoreFault`: 加载/存储数据时发生物理地址访问错误。
        // - `Load/StorePageFault`: 加载/存储数据时发生页错误（最常见）。
        Trap::Exception(Exception::StoreFault) 
        | Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::InstructionFault)
        | Trap::Exception(Exception::InstructionPageFault)
        | Trap::Exception(Exception::LoadFault)
        | Trap::Exception(Exception::LoadPageFault)
        => {
            // Simple handling for now
            println!(
                "[kernel] trap_handler:  {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                scause.cause(),
                stval,
                current_trap_cx().sepc,
            );
            print_memory(stval);
            // 终止当前应用并切换到下一个应用。
            // page fault exit code
            exit_current_and_run_next(-2);
        }
        // 3. 非法指令
        Trap::Exception(Exception::IllegalInstruction) => {
            println!("[kernel] IllegalInstruction in application, kernel killed it.");
            // 终止当前应用并切换到下一个应用。
            exit_current_and_run_next(-3);
        }
        // 4. 时间片用完
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            let cur = current_task().unwrap();
            let prio = cur.inner_exclusive_access().priority;
            cur.inner_exclusive_access().stride += BIG_STRIDE / prio; 
            // 设置下一次时钟中断的触发时间。
            set_next_trigger(); 
            // 挂起当前任务，并切换到下一个任务。
            suspend_current_and_run_next();
        }
        _ => {
            panic!{
                "Unsupported trap {:?}, stval = {:#x}",
                scause.cause(),
                stval
            }
        }
    }
    // 调用 trap_return 函数，准备返回用户态。
    trap_return();
}

/// 从中断处理返回用户空间
/// 准备恢复用户态需要的信息，然后跳转 __restore 。
#[no_mangle]
pub fn trap_return() -> ! {
    // 在返回用户态之前，将 stvec 重新设置为用户中断入口（之前设置为了内核中断入口）。
    set_user_trap_entry();
    let trap_cx_ptr = TRAP_CONTEXT_BASE;
    let user_satp = current_user_token();
    extern "C" {
        fn __alltraps();
        fn __restore();
    }

    // 计算 __restore 函数在 TRAMPOLINE 页中的虚拟地址:
    // 偏移+基址，因为 __alltraps 在跳板页开头。
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;

    unsafe {
        asm!(
            // 刷新指令缓存：切换页表可能导致一些原先存放某个应用代码的物理页帧，
            // 如今用来存放数据或者是其他应用的代码。加上它是安全的做法
            "fence.i",
            // 跳转到 __restore
            "jr {restore_va}",
            // in(reg) 表示将 restore_va 变量的值加载到一个通用寄存器中。
            restore_va = in(reg) restore_va,

            // 设置 __restore 参数：TrapContext 在用户地址空间的地址；用户地址空间的 token
            // in("a0") 表示将 trap_cx_ptr 的值加载到 a0 寄存器。
            in("a0") trap_cx_ptr,
            // in("a1") 表示将 user_satp 的值加载到 a1 寄存器。
            in("a1") user_satp,

            // options(noreturn) 告诉编译器这个内联汇编块不会返回。
            // 因为执行流已经通过 jr 指令跳转走了。
            options(noreturn)
        );
    }
}

/// 内核态中断处理函数
/// 目前直接 panic
///? 目前内核必须要在时钟中断时间片内处理完用户中断，否则，就无法工作，因为一次被打断就结束了。
#[no_mangle]
pub fn trap_from_kernel() -> ! {
    use riscv::register::sepc;
    trace!("stval = {:#x}, sepc = {:#x}", stval::read(), sepc::read());
    panic!("a trap {:?} from kernel!", scause::read().cause());
}



// 从子模块 `context` 中重新导出 `TrapContext`。
// Why: 这是一种常见的 Rust 编程模式，使得其他模块可以直接通过 `crate::trap::TrapContext`
//      来使用这个结构体，而不需要关心它具体是在 `trap/mod.rs` 还是 `trap/context.rs` 中定义的，
//      使得模块的内部结构更加灵活。
pub use context::TrapContext;

// Crate (箱)：是 Rust 的一个编译单元。你的整个作业系统专案就是一个 Crate。Crate 的根档案通常是 main.rs (对于可执行档) 或 lib.rs (对于函式库)。
// Module (模组)：是 Crate 内部组织程式码的方式。你可以把模组想像成一棵树，Crate 的根档案是这棵树的树根。
// 
// Rust 透过 mod 关键字来建立模组树的节点。
// 在 main.rs 或 lib.rs 中，mod trap; 会告诉编译器去寻找 trap.rs 或 trap/mod.rs 档案，并将其内容建立成一个名为 trap 的模组。
// 同样地，在 trap.rs (或 trap/mod.rs) 中，mod context; 会寻找 context.rs 或 context/mod.rs，并将其建立成 trap 的子模组。
// 
// 默认情况下，一个模组内的所有东西（函数、结构体等）都是私有的。只有在前面加上 pub 关键字，它才能被模组外部存取。为了让 main 能存取 TrapContext，trap 模组、context 模组以及 TrapContext 结构体本身都需要是 pub 的。
// 
// 
// pub use context::TrapContext;
// 
// 这行程式码被称为重新汇出 (re-exporting)。它做了两件事：
// use: 将 context::TrapContext 这个路径汇入到目前 trap 模组的作用域中。
// pub: 让这个汇入的路径再次公开。
// trap 模组负责处理所有与中断相关的功能。TrapContext 结构体的定义虽然是中断处理的一部分，但为了程式码整洁，开发者把它放到了一个单独的 context.rs 档案里。
// 然而，对于 trap 模组的使用者来说（例如 main.rs 或其他模组），他们不应该也不需要关心 TrapContext 到底是直接定义在 trap.rs 中，还是在一个叫 context 的子模组里。这属于 trap 模组的内部实作细节。
// 透过 pub use context::TrapContext;，trap 模组对外提供了一个更简洁、更稳定的 API。其他模组现在可以用一个更短的路径来引用 TrapContext：crate::trap::TrapContext,而不是用那个冗长的内部路径crate::trap::context::TrapContext
// 
// 这么做最大的优点是：
// 未来如果开发者决定重构程式码，比如把 context.rs 删掉，直接将 TrapContext 的定义放回 trap/mod.rs 文件里，他只需要修改 trap 模组内部，而所有使用 crate::trap::TrapContext 的外部程式码完全不需要做任何改动。

fn print_memory(stval: usize) {
    println!("[kernel] --- Memory Fault Report ---");
    println!("[kernel] Faulting Address (stval) = {:#x}", stval);
    
    // Verifying hypothesis: Is it writing to a read-only section?
    extern "C" {
        fn stext();    // Start address of .text section
        fn etext();    // End address of .text section
        fn srodata();  // Start address of .rodata section
        fn erodata();  // End address of .rodata section
        fn sbss();     // Start address of .bss section
        fn ebss();     // End address of .bss section
    }

    // Getting address ranges of sections from linker symbols
    // Note: These symbols are essentially function pointers, we need to cast them to usize for comparison
    let text_start = stext as usize;
    let text_end = etext as usize;
    let rodata_start = srodata as usize;
    let rodata_end = erodata as usize;
    let bss_start = sbss as usize;
    let bss_end = ebss as usize;

    // Printing memory map for reference
    println!("[kernel] Memory Map:");
    println!("[kernel]   .text:   [{:#x}, {:#x})", text_start, text_end);
    println!("[kernel]   .rodata: [{:#x}, {:#x})", rodata_start, rodata_end);
    println!("[kernel]   .bss:    [{:#x}, {:#x})", bss_start, bss_end);

    println!("[kernel] --- Verification ---");
    // Core verification logic
    if stval >= text_start && stval < text_end {
        println!("[kernel] ✅ SUCCESS: The faulting address is inside the .text section!");
    } else if stval >= rodata_start && stval < rodata_end {
        println!("[kernel] ✅ SUCCESS: The faulting address is inside the .rodata section!");
    } else if stval >= bss_start && stval < bss_end {
        // This branch helps us understand the location of USER_STACK
        println!("[kernel] INFO: The faulting address is inside the .bss section. This is unexpected for a R/W violation.");
    } else {
        println!("[kernel] FAILED: The address is outside known kernel sections. The hypothesis might be wrong.");
    }
}