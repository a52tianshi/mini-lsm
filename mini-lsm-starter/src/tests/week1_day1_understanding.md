# Week 1 Day 1: Memtable — Test Your Understanding

结合当前实现（`src/mem_table.rs`、`src/lsm_storage.rs`）回答官方文档的 Test Your Understanding。

原文链接：<https://skyzh.github.io/mini-lsm/week1-01-memtable.html#test-your-understanding>

## 1. Why doesn't the memtable provide a `delete` API?

因为“删除”在 LSM 里是用一条特殊的 **写入**（tombstone，墓碑记录）来表示的，而不是真的从数据结构里摘掉一个 entry：

```rust
// lsm_storage.rs
pub fn delete(&self, _key: &[u8]) -> Result<()> {
    self.put(_key, &[])   // 用空 value 表示删除
}
```

`get()` 里再统一识别空 value 当作“已删除”：

```rust
if let Some(value) = snapshot.memtable.get(_key) {
    if value.is_empty() { return Ok(None); }
    return Ok(Some(value));
}
```

这样设计的好处：删除操作和普通 put 走的是完全相同的路径（写 memtable → flush 成 SST → 参与 compaction），不需要在 skiplist 层面单独实现一个“真删除”的接口，也不需要在合并迭代器、compaction 等所有下游逻辑里都单独处理"删除"这个特殊 case——它们只需要统一处理"空 value = tombstone"这一条规则（参见 `lsm_iterator.rs` 的 `skip_deleted`）。真正把这条记录从磁盘上抹掉，是靠后续 compaction 把过期版本连同 tombstone 一起丢弃完成的。

## 2. Does it make sense for the memtable to store all write operations instead of only the latest version of a key?

在当前（week 1，无 MVCC/timestamp）阶段**不需要**：`MemTable` 内部就是 `SkipMap<Bytes, Bytes>`，`put` 时对同一个 key 直接覆盖（`insert` 语义），所以 `a->1, a->2, a->3` 连续写入后，memtable 里只保留最后一次 `a->3`。这是合理的，因为：

- week 1 只需要支持"读到最新值"的语义，不需要支持读历史版本/快照读。
- 只保留最新版本能显著节省内存，避免同一个 key 被反复覆盖时（比如计数器场景）memtable 无限膨胀。

但是到了 week 3 引入 MVCC（key 里嵌入时间戳 `key+ts`）之后，情况就不同了：那时候需要支持"某个快照时间点读到某个版本"，所以 memtable 就必须把每个 `(key, ts)` 当作不同的 entry 存下来，此时"存所有版本"才是必要的。所以这个问题的答案是分阶段的：**当前阶段不需要，但为后续 MVCC 打基础时是需要的**。

## 3. Is it possible to use other data structures as the memtable in LSM? Pros/cons of skiplist?

可以。常见候选：

- **有序 `Vec` + 二分查找**：内存紧凑、局部性好，但插入是 `O(n)`（要挪动元素），不适合频繁写入的可变 memtable，一般只用在已经不可变的结构里（比如 immutable memtable 或直接当 SST 的中间表示）。
- **`BTreeMap`**：`O(log n)` 插入/查找，天然有序，但标准库版本不是无锁的，并发写入需要外层加锁，粒度粗。
- **`HashMap`**：`O(1)` 点查最快，但没有顺序，做不了 range scan，而 LSM 的 `scan`/合并迭代器强依赖有序遍历，所以不适用。
- **跳表（当前用的 `crossbeam_skiplist::SkipMap`）**：`O(log n)` 期望时间复杂度的查找/插入/删除，天然保持有序（支持高效 range scan），而且是**无锁并发**的数据结构，允许多个线程同时读写而不需要一把大锁保护整个结构——这正好匹配 memtable 需要“边写边被并发 `get`/`scan`”的场景。

跳表的代价：每个节点要维护多层 forward 指针，内存开销比数组/紧凑结构大；节点是分散的堆分配，缓存局部性不如连续数组（详见第 6 题）。总体上，跳表是在“并发友好 + 有序遍历”和“内存/局部性”之间做的一个折中，对 LSM 的写多读多场景是合理选择。

## 4. Why do we need a combination of `state` and `state_lock`? Can we only use `state.read()`/`state.write()`?

```rust
pub(crate) state: Arc<RwLock<Arc<LsmStorageState>>>,
pub(crate) state_lock: Mutex<()>,
```

`state` 这把 `RwLock` 保护的是"整体切换到哪个 `Arc<LsmStorageState>` 快照"这个瞬间的原子性——读者（`get`/`scan`）只需要 `state.read()` 拿到当前快照的 `Arc` clone 就可以放心读，不阻塞其他读者，也不会读到"写到一半"的中间状态。

但像 `force_freeze_memtable` 这种"结构性更新"操作，逻辑上是一整个 **读-克隆-改-写回** 的事务：

```rust
let mut guard = self.state.write();
let mut snapshot = guard.as_ref().clone(); // 读 + 克隆
snapshot.imm_memtables.insert(0, guard.memtable.clone());
snapshot.memtable = Arc::new(MemTable::create(0));
*guard = Arc::new(snapshot);               // 写回
```

如果只有 `state.read()`/`state.write()`，没有额外的 `state_lock`，两个线程可以**同时**各自决定要 freeze（比如都在 `put()` 里检测到 memtable 超过阈值），然后各自去抢 `state.write()`；`RwLock` 只保证每次 `write()` 期间没人能同时读写，但**不保证**两次独立的"读-改-写回"事务不会交错执行——第二个线程写回时用的 snapshot 基础是它自己早先 clone 的（可能是 freeze 之前的老状态），会把第一个线程刚做的 freeze 结果整个覆盖掉，造成"丢失更新"。

`state_lock: Mutex<()>` 的作用就是把这一整个"读-改-写回"过程当成一个临界区序列化起来，同一时间只允许一个线程执行 freeze/flush/compaction 这类结构性更新，读者（`state.read()`）依然可以随时并发进行，不受 `state_lock` 影响。这是经典的"用一把额外的协调锁保护多步读写事务，同时保留 `RwLock` 让纯读者不被卡住"的模式。

## 5. Why does the order to store and probe the memtables matter?

`imm_memtables` 是一个 `Vec`，`force_freeze_memtable` 每次冻结都用 `insert(0, ...)` 把新冻结出来的 memtable 塞到最前面：

```rust
snapshot.imm_memtables.insert(0, guard.memtable.clone());
```

所以约定是：**index 0 是最近冻结的（相对更新），越往后越旧**。`get()` 探测顺序必须严格遵循"当前 memtable → `imm_memtables[0]` → `imm_memtables[1]` → …"：

```rust
if let Some(value) = snapshot.memtable.get(_key) { ... return; }
for imm_memtable in snapshot.imm_memtables.iter() { ... return on hit; }
```

这个顺序很关键：同一个 key 可能在当前 memtable、某个较新的 imm memtable、某个较旧的 imm memtable里都存在不同的值（后写的覆盖先写的）。因为只有最新写入的那个版本才是"正确答案"，一旦命中某一层就必须立刻返回，不能继续往更旧的层找，也不能反过来"从旧到新"扫——否则会把新值被旧值覆盖返回，产生读到过期数据的 bug。

## 6. Is the memory layout of the memtable efficient / does it have good data locality?

不算高效。原因：

- `MemTable::map` 是 `Arc<SkipMap<Bytes, Bytes>>`：跳表本身节点是各自独立堆分配、通过多层指针互相链接的，遍历/范围扫描时需要不断"指针跳转"，而不是像连续数组那样顺序访问内存，CPU cache 命中率差。
- `Bytes`（`bytes` crate）本身又是一层引用计数的堆指针，key 和 value 各自额外一次堆分配（`Bytes::copy_from_slice` 会拷贝一份数据到新分配的 buffer），所以一条 KV 记录实际上分散在至少 3 处内存（跳表节点 + key 的 Bytes buffer + value 的 Bytes buffer），局部性进一步变差。

可能的优化方向：
- 用 arena/slab 分配器把跳表节点和它们持有的 key/value 数据打包分配在连续内存块里，减少零散的小对象分配。
- 对小 key/小 value 做内联优化（类似 `smallvec`/`SmolStr` 的思路），避免额外一次堆分配。
- Scan 时做块预取（prefetch）缓解指针跳转带来的 cache miss。
- 极端场景下可以考虑用"不可变有序数组 + 定期合并"替代可变跳表（牺牲部分写并发换局部性），这也是一些 LSM 实现（比如某些"chunked memtable"设计）采用的思路。

## 7. Is parking_lot's read-write lock a fair lock?

**不是严格的 FIFO 公平锁**，但也不是完全无限制的"谁抢到算谁的"：`parking_lot::RwLock` 采用的是一种"任务公平"（task-fair）策略，专门用来**防止写者饿死**——一旦有一个写者在排队等待现有读者释放锁，之后新来的读者请求会被挡在这个写者后面（不能插队），必须等这个写者拿到并释放锁之后才能继续获取读锁；但已经持有读锁的那些读者不会被打断，可以正常读完。

也就是说：写者不会被"源源不断的新读者"无限期饿死，但整体上并不保证严格的先进先出顺序（比如两个写者之间、或读者与读者之间的相对顺序不是完全确定的）。在我们的实现里，`state.read()` 拿到快照后很快就 clone/drop 掉了（读锁持有时间很短），所以即便不是严格公平锁，实际发生写者长时间饥饿的概率也比较低。

## 8. After freezing the memtable, is it possible that some threads still hold the old LSM state and wrote into these immutable memtables? How does your solution prevent it?

理论上"持有旧快照"是可能发生的：`state.read()` 返回的是 `Arc<LsmStorageState>` 的一个 clone，如果某个线程把这个 `Arc` 缓存下来（而不是马上用完就扔），在这期间另一个线程完成了 `force_freeze_memtable`，那么这个缓存的 `Arc` 里的 `memtable` 字段指向的就是"已经被冻结进 `imm_memtables` 的旧 memtable"。

但在当前实现里，`put()` **不会**发生这种情况，因为它每次都是现取现用，没有跨越 freeze 操作缓存旧快照：

```rust
pub fn put(&self, _key: &[u8], _value: &[u8]) -> Result<()> {
    if self.state.read().memtable.approximate_size() > self.options.num_memtable_limit {
        let _ = self.force_freeze_memtable(&self.state_lock.lock());
    }
    self.state.write().memtable.put(_key, _value)  // 重新获取一次最新快照
}
```

第一次 `state.read()` 只是用来"读一下 size 决定要不要冻结"，读完立刻丢弃；真正写入数据用的是**紧接着重新获取**的 `state.write().memtable`，这是 freeze 完成之后的最新快照，所以不会写到已经被冻结、脱离"current"位置的旧 memtable 里。换句话说，我们的防护手段不是靠锁本身，而是**从不跨越可能发生结构性变化的操作去长期持有一个旧的 `state` 快照**——每次要读/写都重新从 `self.state` 取一次当前指针。

（这里有一个可以进一步讨论的小 gap：`put()` 里 size 检查和真正 freeze 之间没有用同一把锁做原子性保护，两个线程可能同时都判断"超限了"然后都各自去 freeze 一次，产生一次多余的空 freeze；但这不会导致写入丢失或写入到孤立 memtable，只是多做了一次无害的冻结。）

## 9. Read-lock-then-drop-then-write-lock vs. directly upgrading the read lock — what's the difference?

- **先读锁、释放、再写锁**（当前实现的方式）：两次获取锁之间存在一个"空窗期"，其他线程完全可能在这个空窗期里拿到写锁并修改状态。所以在拿到写锁之后，代码必须**重新校验**之前基于读锁做出的判断是否仍然成立（或者干脆设计成"即使判断过期了也不会造成错误，只是多做一次无害操作"，就像上一题里的场景）。好处是读锁持有时间短，两次获取之间完全不互斥，其他读者/写者都可以正常插入进来，并发度更高。
- **原地把读锁升级为写锁**（比如 `parking_lot::RwLock::upgradable_read()` 再 `.upgrade()`）：整个"检查条件 → 决定要写 → 真正写"过程中锁没有被释放过（至少 upgradable read 阶段就已经排除了其他写者/其他升级者），保证了"检查"和"写入"之间原子、不会有别的线程插进来改变状态，天然避免 TOCTOU（check-then-act）竞态，逻辑更简单，不需要写完之后再校验一遍。

代价：`upgradable_read` 本身是排他的（同一时刻只能有一个线程持有 upgradable read，即使还没真正 upgrade 到写锁），比普通 `read()` 的并发度低；如果检查条件的逻辑本身很轻量、而"检查后可能什么都不做"的情况很常见，直接 upgrade 反而会不必要地阻塞其他本可以并发的读者。

我们的代码实际上选择了第三种折中方案：既不用 `state.read()`→drop→`state.write()` 裸奔（会有竞态），也不用 `upgradable_read`，而是引入了单独的 `state_lock: Mutex<()>` 来把"结构性更新"这一类操作整体串行化，把 `state` 这把 `RwLock` 完全留给"纯粹的指针快照切换"，两者职责分开。这样既避免了竞态，又不需要为每次读都去竞争一把排他的 upgradable 锁。
