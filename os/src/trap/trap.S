.altmacro
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
    .section .text.trampoline
    .globl __alltraps
    .globl __restore
    .align 2
__alltraps:
    # 交换 sp 和 sscratch.
    # Trap 发生时, sp 指向用户栈, sscratch 指向用户空间 TrapContext 地址
    csrrw sp, sscratch, sp
    # 现在 sp->*TrapContext 在用户空间, sscratch->用户栈
    # save other general purpose registers
    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    sd x3, 3*8(sp)
    # skip tp(x4), application does not use it
    # save x5~x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    # we can use t0/t1/t2 freely, because they have been saved in TrapContext
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)
    # read user stack from sscratch and save it in TrapContext
    csrr t2, sscratch
    sd t2, 2*8(sp)

    # --- 从这里开始准备切换到内核 ---
    # load kernel_satp into t0
    ld t0, 34*8(sp)
    # load trap_handler into t1
    ld t1, 36*8(sp)
    # move to kernel_sp
    ld sp, 35*8(sp)
    # switch to kernel space
    csrw satp, t0
    sfence.vma
    # jump to trap_handler
    jr t1

__restore:
    # a0: TrapContext 在用户空间的地址 (常量 TRAMPOLINE - PAGE_SIZE)
    # a1: 目标用户地址空间的 token
    # --- 切换回用户地址空间 ---
    csrw satp, a1
# 如果修改了 `satp` 寄存器，说明内核切换到了一个与先前映射方式完全不同的页表（切换了地址空间）。
# 此时快表里面存储的映射（用虚拟页号访问，返回物理页号）已经失效了，这种情况下内核要在紧邻修改
#  `satp` 的指令后使用 `sfence.vma` 指令来刷新清空整个 TLB。
    sfence.vma
    # 此时已在用户地址空间, 但 sp 仍是内核栈指针 (无效), a0 指向 TrapContext
    csrw sscratch, a0
    # 将 sp 指向 TrapContext, 以便恢复寄存器
    mv sp, a0

    # --- 开始恢复寄存器 (基于 sp) ---
    # now sp points to TrapContext in user space, start restoring based on it
    # restore sstatus/sepc
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    csrw sstatus, t0
    csrw sepc, t1
    # restore general purpose registers except x0/sp/tp
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    # back to user stack
    ld sp, 2*8(sp)
    sret  # S 模式返回指令
