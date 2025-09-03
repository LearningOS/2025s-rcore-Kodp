//! Rust wrapper around '__switch'
//! 
//! Switching to a different tasks's context happens here. The actual 
//! implementation must not be in Rust and (essentially) has to be in asembly
//! language (主要是因为操作 sp 和 ra？), so this module really is just a wrraper
//! around `switch.S`

use super::TaskContext;
use core::arch::global_asm;

global_asm!(include_str!("switch.S"));

extern "C" {
    /// Switch to the context of `next_task_cx_ptr`, saving to current context
    /// in `current_task_cx_ptr`.
    pub fn __switch(
        current_task_cx_ptr: *mut TaskContext, 
        next_task_cx_ptr: *const TaskContext,
    );
}
