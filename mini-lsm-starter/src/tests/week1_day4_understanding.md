# Week 1 Day 4: SST — Test Your Understanding

结合我们当前 `mini-lsm-starter` 里的实现（`src/table.rs`、`src/table/builder.rs`、`src/table/iterator.rs`、`src/block/iterator.rs`）回答官方文档的 Test Your Understanding。

原文链接：<https://skyzh.github.io/mini-lsm/week1-04-sst.html#test-your-understanding>

## 1. What is the time complexity of seeking a key in the SST?

两段查找组成：

1. `SsTable::find_block_idx` 用 `partition_point` 在 `block_meta`（按 `first_key` 升序排列）上做二分查找，定位 key 可能所在的 block，复杂度 `O(log N)`，`N` 为 block 数量。
2. 定位到 block 后，`BlockIterator::seek_to_key` 内部是从头线性扫描（`seek_to_first` + 循环 `next` 直到 `key >= target`），复杂度 `O(K)`，`K` 为该 block 内的 entry 数（block 大小固定，通常是常数量级，如默认约 4KB 一个 block）。

所以总体是 `O(log N + K)`。因为 block 大小固定，`K` 可视为常数，近似 `O(log N)`（`N` = SST 内的 block 数）。如果要进一步优化 block 内的查找，可以把线性扫描换成基于 offset 数组的二分查找，把 `K` 也降到 `O(log K)`。

## 2. Where does the cursor stop when you seek a non-existent key in your implementation?

`create_and_seek_to_key` / `seek_to_key` 语义是"找到第一个 `>= key` 的位置"（lower bound），不是精确匹配：

- 如果 `key` 落在某个 block 的 key 范围内，会停在该 block 中第一个 `>= key` 的位置。
- 如果 `key` 比当前 block 的所有 key 都大（超出该 block 范围但没超出整个 SST），我们在 `seek_to_key`/`create_and_seek_to_key` 里做了处理：`BlockIterator` seek 完变成 invalid 后，会自动把 `blk_idx += 1` 并跳到下一个 block 的开头（见 `src/table/iterator.rs`）。
- 如果 `key` 比整个 SST 的最后一个 key 还大，最终会越过最后一个 block，`blk_idx >= num_of_blocks()`，此时不再重新加载 block，迭代器的 `is_valid()` 返回 `false`（`BlockIterator::key` 为空）。此时若调用者不检查 `is_valid()` 就直接调 `key()/value()`，会读到一个空的 `KeyVec`（我们目前没有加 panic 保护，调用方需要自行判空）。

## 3. Is it possible (or necessary) to do in-place updates of SST files?

不可行，也没有必要：

- **不必要**：LSM 的核心假设是 SST 一旦通过 `SsTableBuilder::build` 写盘就是**不可变**的。更新/删除靠新写入一条带更高版本的记录（或 tombstone），通过 compaction 合并旧 SST 来体现"更新"，而不是去改已有文件。
- **不可行**：SST 里 key/value 是变长编码（`key_len + key + value_len + value`），一旦某个 value 变长/变短，后面所有 block 内 offset、`block_meta` 里的 `offset`/`block_meta_offset` 都要级联重算，等价于重写整个文件；而且原地改写还破坏了"顺序写、随机读"的磁盘友好模式，也让并发读（其他线程正拿着这个不可变文件的 `Arc<SsTable>` 读）变得不安全，需要额外加锁/MVCC 保证。所以设计上就是"不可变 + 追加新文件 + compaction 回收旧文件"。

## 4. Does your implementation allocate enough space for your SST builder in advance?

**没有**。当前 `SsTableBuilder`（`src/table/builder.rs`）里：

```rust
data: Vec::new(),
meta: Vec::new(),
```

`self.data` 是随着 `finish_block()` 里 `self.data.extend(encoded)` 不断增长的普通 `Vec`，没有根据目标 SST 大小（比如常见的 256MB）提前 `reserve`/`with_capacity`。这意味着随着数据增多，`Vec` 会按照标准的倍增策略反复扩容拷贝，对于大 SST 会有明显的多余内存拷贝开销。

（唯一做了预分配的地方是 `Block::encode`：`Vec::with_capacity(self.data.len() + self.data.len() * 2 + 2)`，但那是 block 级别的，不是整个 SST 级别的。）

**可以优化的方向**：在 `SsTableBuilder::new` 时按 `block_size * 预估 block 数`（或直接传入目标 SST 大小）做 `Vec::with_capacity`，减少扩容拷贝次数。

## 5. Looking at the `moka` block cache, why does it return `Arc<Error>` instead of the original `Error`?

因为 `try_get_with` 支持**同一个 key 的并发去重**：如果多个线程同时请求同一个未命中缓存的 `(sst_id, block_idx)`，moka 只会真正执行一次 `read_block` 初始化闭包，然后把结果（无论成功还是失败）广播给所有等待的调用者。

如果初始化失败，这个错误需要被**多个调用者共享**，而普通的 `anyhow::Error`（或大多数 `Error` 类型）并不是 `Clone` 的，没法直接复制给每个等待者。用 `Arc<Error>` 包一层，就可以零拷贝地把同一个错误实例的引用分发给所有并发调用者，这也是我们 `read_block_cached` 里要 `.map_err(...)` 把 `Arc<anyhow::Error>` 转换成普通 `anyhow::Error`（用 `{e}` 格式化，因为 `anyhow::Error` 本身不直接 `impl std::error::Error` 给 `anyhow!` 宏套娃）的原因。

## 6. Does the usage of a block cache guarantee a fixed max number of blocks in memory?

不能严格保证。我们的初始化方式：

```rust
block_cache: Arc::new(BlockCache::new(1024))
```

`moka::sync::Cache::new(1024)` 里的 `1024` 是**条目数量上限**（entry count capacity），不是按字节配的内存上限。如果 block 大小不是严格固定的 4KB（比如某个 key/value 特别大，导致某个 block 编码后超过 target block size；`BlockBuilder` 只是"尽量不超过"，最后一条记录即使超限也会被塞进去），那么"1024 个 block"占用的实际内存字节数就会超过 `1024 * 4KB`。

另外 moka 的淘汰是**近似 LRU + 异步/批量维护**的（内部基于多个 segment 和后台清理线程），并不是每次插入立刻精确淘汰，短时间内实际驻留的条目数可能略微超过配置的 capacity，属于最终一致的淘汰策略，不是硬性上限。

如果要精确控制内存字节数，应该用 moka 的 `weigher`（按每个 block 实际字节数加权）+ `max_capacity`（按字节数设置），而不是用条目数量做 capacity。

## 7. Is it possible to store columnar data in an LSM engine? Is the current SST format still a good choice?

**技术上可以，但当前这种「行式」SST 格式不适合**：

- 当前 block/SST 格式是标准的行存（row-oriented）KV 格式：一条 entry = 一个 key + 一整条 value（value 里如果塞了 100 列，就是整行数据的序列化）。这对点查/短范围扫描（OLTP 场景）很合适。
- 但如果是分析型查询（比如只需要 100 列中的 3 列做聚合），行存必须把整行读出来再丢弃 97 列的数据，I/O 和反序列化都严重浪费，不是好选择。
- 想支持列存分析场景，更好的做法是：value 部分按列拆分成独立的列 block/独立的 SST（类似 Parquet 的 column chunk），只有被查询用到的列才需要读取对应 block；或者在 LSM 之上再叠一层列式存储格式作为 value 的编码方式。也可以考虑把每一列作为独立的 LSM tree/column family。
- 结论：LSM 引擎本身（LevelDB/RocksDB 式的合并结构）可以作为列存的底层存储机制，但**当前这套「整行塞进一个 value」的 SST 格式**不是列存场景下的好选择,需要重新设计 block/value 编码。

## 8. LSM engine built on object store (e.g. S3) — how to adjust SST format/block cache?

主要矛盾：S3 是高延迟、按请求收费、没有 `pread` 式随机小读优势的存储。调整方向：

- **加大 block size**：从 4KB 级别调大到几百 KB～几 MB，减少 GET 请求次数，摊薄每次请求的固定延迟和费用成本。
- **减少小请求次数**：比如把 `block_meta`/index 单独存成一个可以一次性整体拉取的对象（一次 GET 拿到整份索引），而不是像本地文件系统那样按需 seek 读取多段。
- **更激进/更大的 block cache**：网络往返成本远高于本地磁盘，缓存未命中代价大得多，应该尽量把热数据长期留在内存里，甚至加一层本地磁盘缓存（tiered cache：内存 → 本地 SSD → S3）来弥补内存容量不够的情况。
- **预取（prefetch）**：顺序扫描时可以提前批量拉取后续 block，隐藏网络延迟。
- **强校验**：网络传输/对象存储的数据完整性保证和本地磁盘不同，建议给每个 block/SST 加 checksum（这也是官方教程后面 `week1-07` 会补的内容），读回来先校验再用。
- **不可变性天然契合**：SST 一旦写完不再修改，正好匹配对象存储"改写=重新整体 PUT"的特性，不需要为了适配 S3 改变 LSM 不可变文件的设计，反而是天然契合。

## 9. Estimate max database size supportable with 16GB reserved for indexes

粗略估算，基于当前的 `BlockMeta` 结构（`src/table.rs`）：

```rust
pub struct BlockMeta {
    pub offset: usize,        // 8 bytes
    pub first_key: KeyBytes,  // Bytes 结构体开销 + key 内容
    pub last_key: KeyBytes,   // 同上
}
```

假设：
- 平均 key 长度 16 字节，`Bytes`（`bytes` crate）本身有约 24 字节的结构体开销（指针+len+capacity/refcount 相关字段，视具体实现，这里取近似值）。
- 每个 `BlockMeta` ≈ `8(offset) + (24+16)(first_key) + (24+16)(last_key)` ≈ **88 字节**。
- `block_size` 取默认量级 4KB（4096 字节数据块）。

那么：

- 16GB 内存能装的 block meta 数量 ≈ `16 * 1024^3 / 88` ≈ **1.95 亿个 block**。
- 对应可支持的数据总量 ≈ `1.95 亿 * 4096 字节` ≈ **798 GB**，也就是**约 700～800 GB** 左右的数据库大小（数量级上是 TB 以内、大约小一个数量级）。

这只是数量级估算，实际会受这些因素影响：
- key 越长，`BlockMeta` 越大，能支持的数据总量就越小（近似反比）。
- `block_size` 调大（比如改成 16KB/64KB），单位内存能覆盖的数据量会线性增大，但会牺牲点查时 block 内线性扫描的开销和读放大。
- 如果之后加上 Bloom filter（week1-07 会做）也会占用额外内存，需要一起计入这 16GB 预算。
- 这也解释了为什么"把索引全放内存"这种简单设计在数据量到 TB 级别后会成为瓶颈——这正是官方教程后续章节要引入分层索引/更紧凑索引结构的动机。
