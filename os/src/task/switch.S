.altmacro
.macro SAVE_SN n
    sd s\n, (\n+2)*8(a0)
.endm
.macro LOAD_SN n
    ld s\n, (\n+2)*8(a1)
.endm


# TaskContext 结构在内存中的布局 (Rust #[repr(C)]):
# offset | field
# -------|-----------------
#   0    | ra (8 bytes)
#   8    | sp (8 bytes)
#   16   | s[0] (s0)
#   24   | s[1] (s1)
#   ...  | ...

    # 将以下代码放入 .text ELF 段，即可执行代码段。
    .section .text
    # 将 __switch 符号声明为全局可见，以便 Rust 代码可以链接和调用它。
    .globl __switch
# __switch (
# -> 指向当前任务 A 上下文的指针，我们将把寄存器值保存到这里。
#   current_task_cx_ptr: *mut TaskContext, 
# -> 指向下一个任务 B 上下文的指针，我们将从这里恢复寄存器值。
#   next_task_cx_ptr; *const TaskContext
# )
__switch:
    # --------------------------------------------------------------------------
    # 阶段一: 保存当前任务的上下文 (将寄存器存入 a0 指向的 TaskContext)
    # --------------------------------------------------------------------------
    # 保存当前任务的内核栈顶指针 (sp)。
    # 对应 TaskContext.sp 字段，偏移量为 8。
    sd sp, 8(a0)

    # 保存当前任务的返回地址 (ra)。
    # call __switch 设置了 ra，指向 __switch 被调用后的下一条指令。
    # 对应 TaskContext.ra 字段，偏移量为 0。
    # 之后返回到调用 __switch 之后的位置。
    sd ra, 0(a0)

    # 使用 .rept 循环和 SAVE_SN 宏，批量保存 s0 到 s11 寄存器。
    # __switch 作为被调用者，必须自己负责保存这些寄存器。
    .set n, 0
    .rept 12
        SAVE_SN %n
        .set n, n + 1
    .endr

    # 至此，当前任务的上下文已全部保存在内核栈中，可以安全地切换到下一个任务。
    # --------------------------------------------------------------------------
    # 阶段二: 恢复下一个任务的 TaskContext (从 a1 指向的 TaskContext 读取值到寄存器)
    # --------------------------------------------------------------------------

    # 恢复下一个任务的返回地址 (ra)。
    # 这是整个切换过程的“魔法”所在：当函数末尾执行 ret 时，
    # CPU 会跳转到这个新加载的 ra 地址，从而将控制权交给下一个任务。
    # ra 地址默认设置成 __restore，这段代码在 task/context.rs::goto_restore
    #   初始化 TaskManagerInner 时被设置，这就启动了程序；在调用 __switch 是，ra 被
    #   设置为 __switch 的下一条指令，换句话说会沿着之前程序的路径继续。对于本章 ch5，应该是
    #   回到 run_tasks，
    ld ra, 0(a1)
    # 使用 .rept 循环和 SAVE_SN 宏，批量保存 s0 到 s11 寄存器。
    .set n, 0
    .rept 12
        LOAD_SN %n
        .set n, n + 1
    .endr

    # 恢复下一个任务的内核栈顶指针 (sp)。
    # 这条指令执行后，栈就切换到了下一个任务对应的内核栈。
    ld sp, 8(a1)

    # ret 是 `jalr zero, ra, 0` 的伪指令，它会无条件跳转到 ra 寄存器中保存的地址。
    # 因为 ra 已经在上面被恢复为“下一个任务”的返回地址，所以这行代码执行后，CPU 的执行流就
    #   切换到了下一个任务中。
    ret
