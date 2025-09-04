> 1. 在我们的多线程实现中，当主线程 (即 0 号线程) 退出时，视为整个进程退出， 此时需要结束该进程管理的所有线程并回收其资源。 
>     - 需要回收的资源有哪些？ 
>     - 其他线程的 TaskControlBlock 可能在哪些位置被引用，分别是否需要回收，为什么？

答：需要回收进程的地址空间 `MemorySet`、进程内所有线程的 `TaskControlBlock` 及其对应的内核栈、进程打开的文件描述符表 `fd_table` 和 IPC 对象。

> 
> 2. 对比以下两种 `Mutex` 中的实现，二者有什么区别？这些区别可能会导致什么问题？
>     ```rust
>     impl Mutex for Mutex1 {
>         fn lock(&self) {
>             loop {
>                 let mut mutex_inner = self.inner.exclusive_access();
>                 if mutex_inner.locked {
>                     mutex_inner.wait_queue.push_back(current_task().unwrap());
>                     drop(mutex_inner);
>                     block_current_and_run_next();
>                 } else {
>                     mutex_inner.locked = true;
>                     break;
>                 }
>             }
>         }
>     
>         fn unlock(&self) {
>             let mut mutex_inner = self.inner.exclusive_access();
>             assert!(mutex_inner.locked);
>             mutex_inner.locked = false;
>             if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
>                 add_task(waking_task);
>             }
>         }
>     }
>     
>     impl Mutex for Mutex2 {
>         fn lock(&self) {
>             let mut mutex_inner = self.inner.exclusive_access();
>             if mutex_inner.locked {
>                 mutex_inner.wait_queue.push_back(current_task().unwrap());
>                 drop(mutex_inner);
>                 block_current_and_run_next();
>             } else {
>                 mutex_inner.locked = true;
>             }
>         }
>     
>         fn unlock(&self) {
>             let mut mutex_inner = self.inner.exclusive_access();
>             assert!(mutex_inner.locked);
>             if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
>                 add_task(waking_task);
>             } else {
>                 mutex_inner.locked = false;
>             }
>         }
>     }
>     ```


答： `Mutex2` 的实现存在致命错误。主要区别在于 `lock` 函数。`Mutex1` 在线程被唤醒后，会通过 `loop` 循环重新检查锁状态，`Mutex2` 被唤醒后则直接假定已持有锁，缺少此检查。