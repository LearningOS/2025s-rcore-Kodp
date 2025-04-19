.altmacro
.macro SAVE_SN n
    sd s\n, (\n+2)*8(a0)
.endm
.macro LOAD_SN n
    ld s\n, (\n+2)*8(a1)
.endm
    .section .text
    .globl __switch

# pub struct TaskContext 
#     Ret position after task switching
#     ra: usize,
#     Stack pointer
#     sp: usize,
#      s0-11 register, callee saved
#     s: [usize; 12],
# 
__switch:
    # __switch(
    #     current_task_cx_ptr: *mut TaskContext, #@ a0, 当前栈
    #     next_task_cx_ptr: *const TaskContext   #@ a1, 切换目标栈
    # )
    # save kernel stack of current task
    sd sp, 8(a0)  # 保存TaskContext的sp
    # save ra and s0~s11 of current execution
    sd ra, 0(a0)  # 保存TaskContext的ra
    # 保存 ra 很重要，它记录了 __switch 函数返回之后应该跳转到哪里继续执行
    .set n, 0
    .rept 12
        SAVE_SN %n
        .set n, n + 1
    .endr
    # restore ra and s0~s11 of next execution
    ld ra, 0(a1)  # 恢复TaskContext的ra（跳转地址）
    .set n, 0
    .rept 12
        LOAD_SN %n
        .set n, n + 1
    .endr
    # restore kernel stack of next task
    ld sp, 8(a1)  # 恢复TaskContext的sp
    # 编译器不会帮我们处理这些寄存器，只能用汇编写
    ret # 跳转ra

