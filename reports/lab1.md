## 总结本次实验
本次实验主要实现以下内容：
- 对结构体 `TaskControlBlock` 添加了成员 `syscall_times: [u32; MAX_SYSCALL_NUM]`，记录当前任务使用的每种系统调用的数量。
	- `syscall_id` 就是数字，所以以它作为键访问 `syscall_times`。
- 增加全局变量 `MAX_SYSCALL_NUM`，表示最大系统调用数量。
- 为 `TaskManager` 实现 `add_syscall_times(syscall_id)` 方法，用于更新当前正在执行的任务的 `syscall_times`。
	- 当前任务调用 `syscall` 时，将其 `syscall_times` 中对应条目加一。
- 为 `TaskManager` 实现 `get_syscall_times()` 方法，返回正在执行的任务的 `syscall_times`。
- 按照实验要求实现 `sys_trace`。


## 简答作业

##### 问题 1
正确进入 U 态后，程序的特征还应有：使用 S 态特权指令，访问 S 态寄存器后会报错。 请同学们可以自行测试这些内容（运行 三个 bad 测例 (`ch2b_bad_*.rs`) ）， 描述程序出错行为，同时注意注明你使用的 sbi 及其版本。

**答：**
sbi 版本：
```
[rustsbi] RustSBI version 0.3.0-alpha.2, adapting to RISC-V SBI v1.0.0
```

运行程序，输出如下：
```log
I am ch2b_bad_address.
[kernel] PageFault in application, bad addr = 0x0, bad instruction = 0x804003c8, kernel killed it.
I am ch2b_bad_instructions.
[kernel] IllegalInstruction in application, kernel killed it.
I am ch2b_bad_register.
[kernel] IllegalInstruction in application, kernel killed it.
```
- 第一个程序尝试写入非法内存，被阻止并被杀死。
- 第二个程序尝试执行 `sret` 指令，`sret` 是 S-Mode 指令，用户态无法执行，被阻止并被杀死。
- 第三个程序尝试执行 `csrr sstatus` 指令，`sstatus` 寄存器只能在 S-Mode 或更高等级下访问，用户态无法使用，被阻止并被杀死。

##### 问题 2
深入理解 [trap.S](https://github.com/LearningOS/rCore-Camp-Code-2025S/blob/ch3/os/src/trap/trap.S) 中两个函数 `__alltraps` 和 `__restore` 的作用，并回答如下问题:

1. **L40：刚进入 `__restore` 时，`sp` 代表了什么值。请指出 `__restore` 的两种使用情景。**
```nasm
ld t0, 32*8(sp)
ld t1, 33*8(sp)
ld t2, 2*8(sp)
csrw sstatus, t0
csrw sepc, t1
csrw sscratch, t2
```

**答：**
- 刚进入时，`sp` 指向内核栈。
- `__restore` 既可以用于开始执行一个 app，也可以用于在处理完 trap 后跳转回 app 的执行流。


2. **L43-L48：这几行汇编代码特殊处理了哪些寄存器？这些寄存器的的值对于进入用户态有何意义？请分别解释。**
    ```
	ld t0, 32*8(sp)
	ld t1, 33*8(sp)
	ld t2, 2*8(sp)
	csrw sstatus, t0
	csrw sepc, t1
	csrw sscratch, t2
	```

**答：**
- 这几行汇编代码先处理的是 `sstatus` 、`sepc` 和 `sscratch` 寄存器。
- `sstatus` 给出 Trap 发生前 CPU 所处特权级（S/U）等信息，进入用户态需要恢复；
- `sepc` 是 trap 处理完后应跳转的目标地址，恢复这个寄存器，程序才能正确跳转到原先控制流。
- `sscratch` 在上面代码运行后被恢复为指向内核栈。这个用于处理内核栈和用户栈的交换。


3. **L50-L56：为何跳过了 `x2` 和 `x4`？**
```
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
```
**答：**
- `sp` 已通过 `mv sp, a0` 显式设置为内核栈顶地址，后续通过 `csrrw sp, sscratch, sp` 切换为用户栈指针，所以无须从栈中恢复
- `x4` 目前未使用，无须恢复。

4. **L60：该指令之后，`sp` 和 `sscratch` 中的值分别有什么意义？**
```
    csrrw sp, sscratch, sp
```
**答：**
- 在该指令之后，`sp->内核栈`，`sscratch->用户栈`。

5. **`__restore`：中发生状态切换在哪一条指令？为何该指令执行之后会进入用户态？**

**答：**
- 状态切换发生在 `sret` 指令。
- `sret` 会根据 ` sstatus.SPP` 位将特权级从 S-mode 降为 U-mode, 然后将 `sepc` 的值加载到 PC，也就是跳转用户态下一条待执行指令。

6. **L13：该指令之后，`sp` 和 `sscratch` 中的值分别有什么意义？**
```
    csrrw sp, sscratch, sp
```

**答：**
-  交换，让 `sp->用户栈`，`sscratch->内核栈`。



7. **从 U 态进入 S 态是哪一条指令发生的？**

**答：**
- 用户程序的 `ecall` 指令。


## 荣誉准则    
我参考了 **以下资料** ，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：
-  [管理多道程序 - rCore-Camp-Guide-2025S 文档](https://learningos.cn/rCore-Camp-Guide-2025S/chapter3/3multiprogramming.html#id3)
- [清华大学云盘ch3](https://cloud.tsinghua.edu.cn/d/eec08e3c8f224e27b01d/files/?p=%2Frcore%20%E5%AE%9E%E9%AA%8C%E4%B8%89%20%E9%A9%AC%E6%80%9D%E6%BA%90.mp4)

我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。 我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响起评分。

我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。 我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。 我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。 我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。