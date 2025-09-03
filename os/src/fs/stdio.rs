use super::File;
use crate::mm::UserBuffer;
use crate::sbi::console_getchar;
use crate::task::suspend_current_and_run_next;

/// 标准输入 Stdin。
/// 它是一个零大小的结构体，仅作为一种类型标记，其行为完全由 `File` Trait 的实现来定义。
pub struct Stdin;

/// 标准输出 Stdout。
/// 同样是一个零大小的类型标记。
pub struct Stdout;

impl File for Stdin {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, mut user_buf: UserBuffer) -> usize {
        assert_eq!(user_buf.len(), 1);
        let mut c: usize;
        loop {
            c = console_getchar();
            if c == 0 {
                // 如果 `console_getchar` 返回 0，表示当前没有字符输入，
                // 此时不能原地空转浪费 CPU ，而是调用调度器，主动暂停当前任务，
                // 切换到其他任务运行。
                suspend_current_and_run_next();
                // 当调度器下一次切换回本任务时，从这里继续循环，再次尝试获取输入。
                continue;
            }
            else {
                // 如果获取到了字符（返回值不为 0），则跳出循环。
                break;
            }
        }
        let ch = c as u8;
        unsafe {
            user_buf.buffers[0].as_mut_ptr().write_volatile(ch);
        }
        1  // 返回成功读取 1 字节
    }
    fn write(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot write to stdin!");
    }
}

impl File for Stdout {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot read from stdout!");
    }
    fn write(&self, user_buf: UserBuffer) -> usize {
        for buffer in user_buf.buffers.iter() {
            // 将字节切片转换为 UTF-8 字符串，并使用 `print!` 宏打印到控制台。
            print!("{}", core::str::from_utf8(*buffer).unwrap());
        }
        user_buf.len() // 返回写入的总字节数。
    }
}