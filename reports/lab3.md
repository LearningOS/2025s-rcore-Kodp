## 总结本次实验
-  `sys_get_time`: 向 `ts` 位置写入当前时间。用户传来的 `ts` 位置为虚拟地址，我们通过用户页表翻译出目的地址，写入。
- `sys_mmap`：申请一块内存。我们在 `MemorySet` 里实现一个 `map` 函数完成主要功能。首先检查待分配的区域是否于已分配的区间相交；然后调用 `insert_framed_area` 插入新区域。
- `sys_munmap`：解除一块已分配内存。我们在 `MemorySet` 里实现一个 `munmap` 函数完成主要的功能。遍历待解除范围中的每一个 vpn，如果存在 vpn 没有映射或不在 map_area 区域内则返回错误，否则解除这个 vpn 映射。
- `sys_spawn`：创建一个新进程，执行指定程序。我们在 `TaskControlBlock` 里实现一个 `spawn` 函数完成主要功能。首先根据 ELF 数据地址空间，然后创建 TCB、弱引用父进程，随后对父进程添加子进程指针，最后设置 `TrapContext`。
- `sys_set_priority`：实现 stride 优先级调度。修改 `TaskControlBlockInner`，添加 `priority` 字段和 `stride` 字段。`sys_set_priority` 本身只修改当前进程的 `priority` 字段。调度发生在 `run_tasks` 函数，它每次调用 `fetch_task` 取新任务。我们修改 `TaskManager::fetch`，每次取 `ready_queue` 里 `stride` 最小的返回。更新 `stride` 的时机我们放在 `Trap::Interrupt(Interrupt::SupervisorTimer)` 发生的时候，此时更新当前进程的 `stride`。

## 问答题
> stride 算法原理非常简单，但是有一个比较大的问题。例如两个 pass = 10 的进程，使用 8bit 无符号整形储存 `stride`， `p1.stride = 255, p2.stride = 250`，在 p2 执行一个时间片后，理论上下一次应该 p1 执行。实际情况是轮到 p1 执行吗？为什么？

答：虽然 `p2.stride` 会加 10，但是由于溢出问题会变成 250+10=255+5=5，`p2.stride` 还是小，所以下一次还是 p2 执行。


> 我们之前要求进程优先级 `>= 2` 其实就是为了解决这个问题。可以证明， **在不考虑溢出的情况下** , 在进程优先级全部 >= 2 的情况下，如果严格按照算法执行，那么 `STRIDE_MAX – STRIDE_MIN <= BigStride / 2`。为什么？尝试简单说明（不要求严格证明）。

答：pass 小于等于 `BigStride / 2`，`STRIDE_MAX – STRIDE_MIN` 所以不会超过这个数。


> 已知以上结论，**考虑溢出的情况下**，可以为 Stride 设计特别的比较器，让 `BinaryHeap<Stride>` 的 `pop` 方法能返回真正最小的 Stride。补全下列代码中的 `partial_cmp` 函数，假设两个 Stride 永远不会相等。
> TIPS: 使用 8 bits 存储 stride, BigStride = 255, 则: `(125 < 255) == false`, `(129 < 255) == true`.


答：所有待调度进程的 `pass` 值，在任何时刻，都分布在一个 `BigStride / 2` 范围内。于是，所有进程的最大差距不会超过 `BigStride / 2`。把 stride 当无符号数处理。

```rust
use core::cmp::Ordering;

struct Stride(u8);

impl PartialOrd for Stride {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        const HALF: u8 = u8::MAX / 2;
        
        // 这个减法在无符号整数下会自动处理溢出（回绕）。
        let diff = self.0.wrapping_sub(other.0);

        // 根据前提，所有 pass 值的真实差距不会超过 HALF (127)。
        // 如果 diff < HALF，表示从 other 到 self 的顺时针距离很短，
        // 这意味着 self 在环上「领先」于 other，所以 self 比较大
        if diff < HALF {
            Some(Ordering::Greater)
        } 
        // 如果 diff > HALF，表示从 other 到 self 的顺时针距离很长（绕了远路）
        // 这意味着 self 在环上落后于 other，所以 self 比较小
        else {
            Some(Ordering::Less)
        }
    }
}

impl PartialEq for Stride {
    fn eq(&self, other: &Self) -> bool {
        false
    }
}
```


## 荣誉准则

我参考了 **以下资料** ，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

- [第五章：进程及进程管理 - rCore-Camp-Guide-2025S 文档](https://learningos.cn/rCore-Camp-Guide-2025S/chapter5/index.html)

我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。 我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响起评分。

我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。 我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。 我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。 我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。