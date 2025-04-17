//! batch subsystem

use crate::sync::UPSafeCell;
use crate::trap::TrapContext;
use core::arch::asm;
use lazy_static::*;

const USER_STACK_SIZE: usize = 4096 * 2;
const KERNEL_STACK_SIZE: usize = 4096 * 2;
const MAX_APP_NUM: usize = 16;
const APP_BASE_ADDRESS: usize = 0x80400000;
const APP_SIZE_LIMIT: usize = 0x20000;

#[repr(align(4096))]
struct KernelStack {
    data: [u8; KERNEL_STACK_SIZE],
}

#[repr(align(4096))]
struct UserStack {
    data: [u8; USER_STACK_SIZE],
}

static KERNEL_STACK: KernelStack = KernelStack {
    data: [0; KERNEL_STACK_SIZE],
};
static USER_STACK: UserStack = UserStack {
    data: [0; USER_STACK_SIZE],
};

impl KernelStack {
    fn get_sp(&self) -> usize {
        self.data.as_ptr() as usize + KERNEL_STACK_SIZE
    }
    pub fn push_context(&self, cx: TrapContext) -> &'static mut TrapContext {
        let cx_ptr = (self.get_sp() - core::mem::size_of::<TrapContext>()) as *mut TrapContext;
        unsafe {
            *cx_ptr = cx;
        }
        unsafe { cx_ptr.as_mut().unwrap() }
    }
}

impl UserStack {
    fn get_sp(&self) -> usize {
        self.data.as_ptr() as usize + USER_STACK_SIZE
    }
}

struct AppManager {
    num_app: usize,
    current_app: usize,
    app_start: [usize; MAX_APP_NUM + 1],
}

impl AppManager {
    pub fn print_app_info(&self) {
        println!("[kernel] num_app = {}", self.num_app);
        for i in 0..self.num_app {
            println!(
                "[kernel] app_{} [{:#x}, {:#x})",
                i,
                self.app_start[i],
                self.app_start[i + 1]
            );
        }
    }
    /// 加载指定应用程序到约定的物理内存地址
    ///
    /// 本函数将应用程序二进制镜像从内核数据段复制到固定地址 `0x80400000`，该地址是操作系统与应用程序
    /// 预先约定的加载地址。应用程序编译时已通过链接脚本对齐此地址，体现冯诺依曼架构的"代码即数据"特性。
    ///
    /// # 参数
    /// - `app_id`: 应用程序索引号，必须满足 0 ≤ app_id < num_app
    ///
    /// # 错误处理
    /// - 当 `app_id ≥ num_app` 时触发 panic，表示所有应用程序已执行完毕
    ///
    /// # 安全要求
    /// 1. 调用者必须确保 `app_id` 有效，否则可能发生内存越界
    /// 2. 目标地址 `APP_BASE_ADDRESS` 必须具有可写且可执行的内存映射
    /// 3. 应用程序镜像必须完整包含在 `app_start[app_id]` 到 `app_start[app_id+1]` 区间内
    unsafe fn load_app(&self, app_id: usize) {
        if app_id >= self.num_app {
            println!("All applications completed!");
            use crate::board::QEMUExit;
            crate::board::QEMU_EXIT_HANDLE.exit_success();
        }
        println!("[kernel] Loading app_{}", app_id);
        // clear app area
        // 清空目标内存区域（确保无残留数据）
        core::slice::from_raw_parts_mut(APP_BASE_ADDRESS as *mut u8, APP_SIZE_LIMIT).fill(0);
        let app_src = core::slice::from_raw_parts(
            self.app_start[app_id] as *const u8,                 // 应用起始地址
            self.app_start[app_id + 1] - self.app_start[app_id],  // 应用长度
        );
        let app_dst = core::slice::from_raw_parts_mut(APP_BASE_ADDRESS as *mut u8, app_src.len());
        
        // 将应用二进制数据复制到目标地址
        app_dst.copy_from_slice(app_src);
        // Memory fence about fetching the instruction memory
        // It is guaranteed that a subsequent instruction fetch must
        // observes all previous writes to the instruction memory.
        // Therefore, fence.i must be executed after we have loaded
        // the code of the next app into the instruction memory.
        // See also: riscv non-priv spec chapter 3, 'Zifencei' extension.
        // 确保后续取指操作能看到新加载的指令，避免旧指令缓存影响
        asm!("fence.i");
    }

    pub fn get_current_app(&self) -> usize {
        self.current_app
    }

    pub fn move_to_next_app(&mut self) {
        self.current_app += 1;
    }
}


// lazy_static! 宏提供了全局变量的运行时初始化功能，
// 适用于运行期间才能得到初始值的全局变量。
// 这里使用这个宏，让 APP_MANAGER 这个实例在第一次用到时才初始化。
lazy_static! {
    /// 初始化全局实例
    /// static ref 并不是 Rust 语言原生的关键字组合，而是 ​​lazy_static! 宏的专用语法，
    /// 用于声明一个延迟初始化的静态变量
    static ref APP_MANAGER: UPSafeCell<AppManager> = unsafe {
        UPSafeCell::new({
            // 找到到 link_app.S 中提供的符号 _num_app，并从这里开始解析应用数量以及各个应用的起始地址。
            extern "C" {
                fn _num_app();
            }
            let num_app_ptr = _num_app as usize as *const usize;
            // 从 num_app_ptr 指向的内存地址读取一个 usize 类型的值。
            // read_volatile! 确保编译器不会对该读取进行优化，因为该值可能由其他硬件或线程修改。
            let num_app = num_app_ptr.read_volatile();
            let mut app_start: [usize; MAX_APP_NUM + 1] = [0; MAX_APP_NUM + 1];
            // 从 _num_app + 1 地址开始读取应用地址数组（长度 num_app + 1）
            let app_start_raw: &[usize] =
                core::slice::from_raw_parts(num_app_ptr.add(1), num_app + 1);
            // 将读取的地址数据复制到 app_start 数组
            app_start[..=num_app].copy_from_slice(app_start_raw);
            AppManager {
                num_app    ,      // 应用总数
                current_app: 0,   // 当前执行的应用索引（初始化为0）
                app_start  ,      // 应用地址数组
            }
        })
    };
}

/// init batch subsystem
pub fn init() {
    print_app_info();
}

/// print apps info
pub fn print_app_info() {
    APP_MANAGER.exclusive_access().print_app_info();
}

/// run next app
pub fn run_next_app() -> ! {
    let mut app_manager = APP_MANAGER.exclusive_access();
    let current_app = app_manager.get_current_app();
    unsafe {
        app_manager.load_app(current_app);
    }
    app_manager.move_to_next_app();
    drop(app_manager);
    // before this we have to drop local variables related to resources manually
    // and release the resources
    extern "C" {
        fn __restore(cx_addr: usize);
    }
    // 在内核栈上压入一个 Trap 上下文:
    // sepc 被设置为 APP_BASE_ADDRESS（即用户程序入口地址 0x80400000）
    // sp 被设置为用户栈顶地址
    // sstatus 设置为用户模式（SPP=User）
    //$ sret 指令​​会从用户模式的 sepc 寄存器读取地址并跳转执行。
    unsafe {
        __restore(KERNEL_STACK.push_context(
            TrapContext::app_init_context(  // app_init_context 初始化一个“任务初始上下文”
                APP_BASE_ADDRESS,
                USER_STACK.get_sp(),
        )) as *const _ as usize);
        // 这个push_context返回压入后的内核栈栈顶（低地址），作为__restore的参数。这样a0寄存器(第一个参数)就会被设为栈顶
        // 随后__restore里 mv sp,a0 把sp<-a0。
        // sscratch 是何时被设置为内核栈顶的？ KERNEL_STACK.push_context 会返回内核栈的栈顶地址，将这个栈顶地址传入 __restore函数的第一个寄存器a0，而 __restore第一行就是 mv sp,a0，就将sp设置成了内核栈的栈顶地址。这么做就可以统一trap返回和创建新应用程序 这两种方式返回到用户特权级。
        
    }
    panic!("Unreachable in batch::run_current_app!");
}
