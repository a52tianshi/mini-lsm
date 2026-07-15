# Week 1 Day 2: Merge Iterator — Test Your Understanding

结合当前实现（`src/iterators/merge_iterator.rs`、`src/mem_table.rs`、`src/lsm_iterator.rs`）回答官方文档的 Test Your Understanding。

原文链接：<https://skyzh.github.io/mini-lsm/week1-02-merge-iterator.html#test-your-understanding>

## 1. What is the time/space complexity of using your merge iterator?

`MergeIterator::create`：把 `N` 个输入迭代器逐个 `push` 进 `BinaryHeap`（不是用 `BinaryHeap::from(vec)` 一次性 heapify），每次 `push` 是 `O(log k)`（`k` 是当前堆大小），所以构造整体是 **`O(N log N)`**。

`next()`：每推进一步，先要把堆顶所有和 `current` key 相同的重复项都 pop 出来推进掉（`while let Some(mut top) = self.iters.pop()`），再决定要不要用新的堆顶替换 `current`（`peek_mut` + `swap`）。每次 `pop`/`push` 都是 `O(log N)`。如果整个归并过程一共产出 `M` 条去重后的记录、原始输入总条目数是 `M'`（`M' >= M`，因为重复 key 也要被"吞掉"一次），那么总时间是 **`O(M' log N)`**。

空间：堆里最多同时保存 `N` 个 `HeapWrapper<I>`（每个装一个 `Box<I>`），加上 `current` 一个，所以合并迭代器自身额外占用 **`O(N)`** 空间（不含底层各个子迭代器自己的数据）。

## 2. Why do we need a self-referential structure for memtable iterator?

`crossbeam_skiplist::map::Range<'a, ...>`（也就是 `SkipMapRangeIter<'a>`）是一个**借用**了 `SkipMap` 的迭代器，它的生命周期 `'a` 绑定在被借用的 `SkipMap` 上。而我们希望 `MemTableIterator` 是一个**可以独立拥有、可以到处传递（比如塞进 `Vec<Box<dyn StorageIterator>>` 参与 `MergeIterator`）的对象**，不希望它带一个额外的生命周期参数（比如 `MemTableIterator<'a>`）去牵连到 `MemTable`/`LsmStorageInner` 的生命周期上——那样一来所有用到迭代器的地方都要传播这个生命周期，非常麻烦，甚至没法直接放进要求 `'static` 的 trait object 里。

所以我们让 `MemTableIterator` 自己同时持有"被借用者"（`Arc<SkipMap<...>>`）和"借用它的迭代器"（`Range<'this>`）：

```rust
#[self_referencing]
pub struct MemTableIterator {
    map: Arc<SkipMap<Bytes, Bytes>>,
    #[borrows(map)]
    #[not_covariant]
    iter: SkipMapRangeIter<'this>,
    item: (Bytes, Bytes),
}
```

这种"结构体的一个字段借用另一个字段"的写法在安全 Rust 里编译器是不允许的（借用检查器无法证明字段搬家时引用还有效），所以要用 `ouroboros::self_referencing` 生成的 unsafe 胶水代码来实现，最终得到一个**没有额外生命周期参数、可以自由 move/装箱**的迭代器类型。

## 3. If a key is removed (there is a delete tombstone), do you need to return it to the user? Where did you handle this logic?

不需要返回给用户。删除是用空 value 表示的（见 day1 的第 1 题），这个逻辑在 `src/lsm_iterator.rs` 的 `LsmIterator` 里统一处理：

```rust
fn skip_deleted(&mut self) -> Result<()> {
    while self.inner.is_valid() && self.inner.value().is_empty() {
        self.inner.next()?;
    }
    Ok(())
}
```

`LsmIterator::new` 构造时先调用一次 `skip_deleted`，`next()` 每次推进后也会调用一次，保证外部用户永远不会看到 value 为空（即被删除）的 key——它们在 `LsmIterator` 这一层就被过滤掉了，`MergeIterator` 本身并不关心"空 value 是删除标记"这件事，它只负责按 key 排序去重，语义上的"删除"是上一层 `LsmIterator` 的职责。

## 4. If a key has multiple versions, will the user see all of them? Where did you handle this logic?

不会，用户只会看到"最优先"的那一个版本。这个逻辑在 `MergeIterator::next()` 里：

```rust
while let Some(mut top) = self.iters.pop() {
    if top.1.key() == current.1.key() {
        top.1.next()?;                 // 相同 key 的其它迭代器直接被跳过（吞掉）
        if top.1.is_valid() { self.iters.push(top); }
    } else {
        self.iters.push(top);
        break;
    }
}
```

`HeapWrapper` 的排序规则是"先比 key，再比 idx"（`self.1.key().cmp(&other.1.key()).then(self.0.cmp(&other.0)).reverse()`），也就是**同一个 key 出现在多个迭代器里时，idx 更小（构造 `MergeIterator::create` 时在 `iters` 参数里更靠前）的那个优先**。当前 `current` 展示的正是最小 idx 的那份数据，其余带相同 key 的迭代器会在 `next()` 里被直接推进一格丢弃掉，用户永远只看到一个版本。这个"idx 越小优先级越高"的约定，正是上层调用者（比如 `scan()` 里 `memtable` 排最前面、更旧的 `imm_memtables` 排在后面）用来实现"新版本覆盖旧版本"语义的关键。

## 5. If we get rid of self-referential structure and add an explicit lifetime, is it still possible to implement `scan`?

`MemTable::scan` 本身没问题，仍然可以返回一个 `MemTableIterator<'a>`。但麻烦在于往上传播：`LsmStorageInner::scan` 需要把 `memtable.scan(..)` 和若干个 `imm_memtable.scan(..)` 一起塞进 `MergeIterator`（进而是 `Box<dyn StorageIterator>` 或者具体的泛型迭代器类型），如果 `MemTableIterator` 带有生命周期 `'a`，那么它借用的 `'a` 必须能覆盖住整个 `scan()` 调用期间——但 `scan()` 里我们是先 `self.state.read()` 拿到一个 `Arc<LsmStorageState>` 快照（只在函数体内存活的局部变量），memtable 是从这个快照里取出来的 `Arc<MemTable>`，它本身的生命周期只在函数调用栈上，不是 `LsmStorageInner` 那种更长的生命周期。

也就是说：理论上可以做，只要把这个生命周期一路标注穿透 `scan()` 返回值、`LsmIterator`、`FusedIterator` 等所有相关类型，但这会让类型签名非常臃肿，而且很难/无法把迭代器装进要求 `'static` 的容器或 trait object 里（比如 `MergeIterator` 内部用 `Box<I>` 存迭代器，也依赖 `I: 'static`，参见第 11 题）。所以工程上选择用 `ouroboros` 的自引用结构规避这个问题，本质上是"用 unsafe 换掉繁琐的生命周期标注"，而不是说这件事纯理论上做不到。

## 6. What happens if (1) create iterator on skiplist memtable (2) someone inserts new keys (3) will the iterator see the new key?

不会看到插入范围之前的新 key（这取决于新 key 插入的位置相对当前游标的位置）。`crossbeam_skiplist::SkipMap` 是并发安全的（无锁），允许在有迭代器存在时继续插入，但 `Range` 迭代器只会按照跳表当前的链接顺序**往前走**，它不会"回头"去看游标已经走过的位置发生了什么变化。具体分两种情况：

- 如果新插入的 key 落在迭代器**尚未遍历到**的范围内（比如迭代器当前在中间，新 key 排序上在后面），迭代器很可能会看到它（因为底层链表节点已经链入，后续遍历会自然扫过去）——这是一种"弱一致性"的行为，取决于具体插入时机和跳表内部指针状态，不保证一定能看到。
- 如果新 key 排序上落在迭代器**已经走过**的位置之前，迭代器不会倒回去看到它。

我们代码里的用法（`self.map.range((lower, upper))`）没有对这种并发插入做任何额外快照隔离，属于跳表本身提供的"弱快照"语义：不保证严格的一致性快照（不像 B-tree 加锁那种强隔离），但保证不会崩溃/不会产生非法内存访问，这也是选择无锁跳表的一个需要权衡的代价。

## 7. What happens if your key comparator cannot give the binary heap implementation a stable order?

`HeapWrapper` 的 `Ord` 实现是 `key().cmp(...).then(idx.cmp(...))`——先比 key 再比 idx 作为 tie-breaker，这保证了任意两个不同的 `HeapWrapper` 之间一定能给出一个确定、稳定的大小关系（不会出现"比较结果不一致"的情况，因为 `(key, idx)` 这个复合键在给定输入下是唯一的）。

如果 comparator **不稳定**（比如两次比较同一对元素给出不同结果，或者违反传递性），`BinaryHeap` 的内部堆结构假设会被破坏：可能导致 `pop()` 拿出来的不是真正的最小/最大元素，进而 `MergeIterator` 输出的顺序错乱（比如本该严格递增的 key 序列出现乱序或漏掉某些重复 key 的去重判断失效）。更严重的是，`BinaryHeap` 内部用比较结果做元素上浮/下沉的位置调整，不稳定的比较器理论上还可能导致 `sift up/down` 逻辑访问到错误的索引位置（不过 Rust 标准库的 `BinaryHeap` 实现本身是内存安全的，不会造成越界，只会产生"逻辑错误的堆序"，不会 UB/panic）。所以关键是：`key` 的比较必须是一个满足全序（total order）关系的、确定性的实现，我们特意加上 `idx` 兜底正是为了避免"key 相同但没有次级排序键导致比较结果退化成无序"的情况。

## 8. Why do we need to ensure the merge iterator returns data in the iterator construction order?

"construction order" 指的是 `MergeIterator::create(iters: Vec<Box<I>>)` 里传入的 `Vec` 顺序，也就是每个迭代器对应的 `idx`。这个顺序天然地编码了"数据新旧优先级"——调用方（`lsm_storage.rs` 的 `scan()`）会按照"当前 memtable 排第一（最新）→ 更早冻结的 imm_memtable 依次往后（越来越旧）"的顺序把迭代器塞进 `Vec`：

```rust
mem_iters.push(Box::new(snapshot.memtable.scan(...)));
for imm in snapshot.imm_memtables.iter() {
    mem_iters.push(Box::new(imm.scan(...)));
}
```

`HeapWrapper` 里"key 相同时 idx 更小优先"的规则，本质就是"在原始传入顺序里排得更靠前的迭代器，代表更新的数据，优先展示"。如果不保证按构造顺序来决定优先级（比如随便谁先谁后），同一个 key 在多个来源里出现不同 value 时，就没法保证返回的是"最新写入的那个值"，会破坏"后写覆盖先写"这个 LSM 最基本的正确性语义。

## 9. Is it possible to implement a Rust-style iterator (`next(&self) -> (Key, Value)`) for LSM iterators? Pros/cons?

技术上可以实现一个返回 `Option<(Key, Value)>` 并且消费/推进自身的标准 `Iterator` 风格接口，但目前 `StorageIterator` trait 故意设计成"`key()`/`value()`只读当前位置 + 单独的 `next()` 来推进，且 `key()` 借用 `&self`"（`fn key(&self) -> Self::KeyType<'_>`），这是为了配合 `KeyType<'a>` 这种**借用当前内部 buffer、避免每次拷贝一份 key/value** 的设计——如果改成标准 `Iterator` 的 `next(&mut self) -> Option<(Key, Value)>`，通常意味着每次推进都要把 key/value **拷贝**一份返回出来（因为返回值不能借用 `&mut self` 内部数据同时又要归还所有权给调用者），这在 LSM 场景下每次 scan 都会有额外的内存分配/拷贝开销，对性能不友好。

优点（如果做标准 `Iterator`）：能直接用上 Rust 生态里所有基于 `Iterator` trait 的组合子（`map`/`filter`/`for` 循环 语法糖等），使用更符合习惯。
缺点：额外的拷贝开销；`Iterator::Item` 是固定类型，不像我们现在 `type KeyType<'a>`（GAT，泛型关联类型）那样可以精确表达"借用生命周期跟随每次调用变化"，会更难支持"零拷贝返回当前内部指针切片"这种设计。当前 `StorageIterator` 的设计权衡是牺牲一点点使用上的语法糖，换取避免每次 `next` 都拷贝 key/value 的性能。

## 10. How to make `scan(lower: Bound<&[u8]>, upper: Bound<&[u8]>)` compatible with Rust-style range (`key_a..key_b`)?

思路是给 `scan` 增加一个重载/包装函数，接受实现了 `std::ops::RangeBounds<[u8]>`（或 `RangeBounds<&[u8]>`）的参数，然后在内部通过 `RangeBounds::start_bound()`/`end_bound()` 转换成 `Bound<&[u8]>` 传给现有实现，比如：

```rust
pub fn scan_range(&self, range: impl std::ops::RangeBounds<[u8]>) -> ... {
    self.scan(range.start_bound(), range.end_bound())
}
```

这样调用方就可以写 `storage.scan_range(key_a..key_b)`（对应 `Included(key_a)..Excluded(key_b)`）这种更符合 Rust 习惯的写法。

如果传入的是全范围 `..`（`RangeFull`），`start_bound()`/`end_bound()` 都会是 `Bound::Unbounded`，等价于我们现有代码里 `_lower`/`_upper` 都传 `Bound::Unbounded` 的情况——`map_bound` 会把它们都转成 `Bound::Unbounded`，`SkipMap::range((Unbounded, Unbounded))` 就是遍历整个跳表，行为完全正常，不会出错，只是需要注意"全表扫描"这种边界情况在性能上（一次性扫全部数据）是否是调用方真正想要的。

## 11. The starter code stores `Box<I>` instead of `I` in the merge iterator interface. Why?

主要原因是**统一大小 + 支持异构底层类型/动态分发**：

- `MergeIterator<I>` 是对同一种具体迭代器类型 `I` 做归并（比如多个 `MemTableIterator`），但 `I` 本身可能是一个没有固定大小、或者内部包含较大/不定长状态的类型（比如自引用结构体、包含其它 Boxed 迭代器的组合类型）。存到 `BinaryHeap<HeapWrapper<I>>` 里，如果不 `Box`，每次堆内部做元素交换（sift up/down）都要 `memcpy` 整个 `I` 的内容，代价可能很大；用 `Box<I>` 之后堆里交换的只是一个指针大小的值，实际数据留在堆分配的内存里不动，移动开销从 `O(size_of::<I>())` 降到 `O(1)`。
- 另外，`Box<I>` 也让上层可以更灵活地把 `MergeIterator` 本身再嵌套进更高层的组合迭代器（比如 `MergeIterator<Box<dyn StorageIterator>>` 这种用 trait object 抹平不同底层迭代器具体类型的场景），如果不用 `Box` 包一层，处理"多种不同来源、不同具体类型的迭代器需要放进同一个容器"这件事会困难得多。

简言之：`Box<I>` 既降低了堆内部频繁移动元素的拷贝开销，又为后续把不同来源的迭代器（memtable、SST 等）统一放进同一个 `MergeIterator` 打下基础。
