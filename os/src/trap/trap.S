.altmacro # Enable ".rept" 宏，在第一行就要加。
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
    .section .text
    .globl __alltraps
    .globl __restore
    .align 2  # 对齐地址

#@ __alltraps:把各个寄存器压到栈里，把栈顶地址传给a0，跳转执行trap_handler
#$ sscratch： 特权级切换的时候我们需要用内核栈，就需要有寄存器存储内核栈地址。
# sscratch就是存内核栈地址的。这样，只需要交换sp和sscratch，就切换了用户/内核栈。
__alltraps:
    # 交换sscratch 和 sp；在这一行前sp->用户栈，sscratch->内核栈；
    csrrw sp, sscratch, sp
    # 现在sp->内核栈，sscratch->用户栈
    # allocate a TrapContext on kernel stack
    # 分配34*8字节
    addi sp, sp, -34*8
    #@ 保存通用寄存器
    # 从低地址到高地址分别按顺序放置x0~x31这些通用寄存器
    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    #$ 跳过了x2和x4，但是保留了空间。保证了xn存放在[sp+8n,sp+8n+8]。
    sd x3, 3*8(sp)
    # skip tp(x4), application does not use it
    # save x5~x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    #@ 保存 CSR 寄存器 sstatus 和 sepc 到内核栈
    # 可以任用t0,t1,t2(x5,x6,x7)了，因为刚才已经保存到内核栈。
    #$ 常规指令无法读写csr；必须先用读写csr的专用指令把csr的值传到通用寄存器，再操作。
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)  # 将 sstatus 读入 t0 并保存到 sp + 32*8
    sd t1, 33*8(sp)  # 将 sepc 读入 t1 并保存到 sp + 33*8
    #@ 处理用户栈指针
    # read user stack from sscratch and save it on the kernel stack
    csrr t2, sscratch # 将 sscratch（进入陷阱前的 sp，指向用户栈）读入 t2
    sd t2, 2*8(sp)    # 保存到 sp + 2*8，（x2的位置，也就是sp的对应位置）
    #@ 设置 trap_handler 的参数：内核栈指针
    # 此时内核栈已经设置好trap上下文；
    # trap_handler要读寄存器的值，所以它要获得trap上下文，给它传内核栈地址sp。
    # trap_handler读寄存器值的作用：例如系统调用的时候应用程序传过来的 syscall ID 和参数。
    # set input argument of trap_handler(cx: &mut TrapContext)
    mv a0, sp  # a0存储内核栈顶（低地址）
    call trap_handler # 调用 trap_handler 处理陷阱，a0 传递 TrapContext 地址
# 从trap_handler返回后继续执行：
__restore:
    # case1: start running app by __restore
    # case2: back to U after handling trap
    #$ 由trap_handler保证，a0不会变化，仍然存储内核栈顶（低地址）
    mv sp, a0  # sp=a0
    # now sp->kernel stack(after allocated), sscratch->user stack
    # restore sstatus/sepc
    #@ 从内核栈恢复通用寄存器和csr
    # 先恢复csr，再恢复通用寄存器。这样t0,t1,t2才能正确恢复。
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    ld t2, 2*8(sp)
    csrw sstatus, t0
    csrw sepc, t1  # sepc设置为发生异常指令的下一条地址
    csrw sscratch, t2
    # restore general-purpuse registers except sp/tp
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    # release TrapContext on kernel stack
    #@ 回收内核栈
    addi sp, sp, 34*8
    # now sp->kernel stack, sscratch->user stack
    #@ 交换，让sp->用户栈，sscratch->内核栈
    csrrw sp, sscratch, sp
    sret  #@ 返回U特权级(case1:开始执行用户程序,case2:返回执行用户程序)

