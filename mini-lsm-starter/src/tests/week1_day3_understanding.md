# Week 1 Day 3: Block — Test Your Understanding

结合当前实现（`src/block.rs`、`src/block/builder.rs`、`src/block/iterator.rs`）回答官方文档的 Test Your Understanding。

原文链接：<https://skyzh.github.io/mini-lsm/week1-03-block.html#test-your-understanding>

## 1. What is the time complexity of seeking a key in the block?

`BlockIterator::seek_to_key`：

```rust
pub fn seek_to_key(&mut self, key: KeySlice) {
    self.seek_to_first();
    while self.is_valid() && self.key() < key {
        self.next();
    }
}
```

是从头开始**线性扫描**，逐条 `next()` 直到 `key >= target`，复杂度 `O(K)`，`K` 是该 block 内的 entry 数。因为 block 本身有一个固定的目标大小（`block_size`，默认量级 KB），`K` 在实践中是一个较小的常数，所以近似可以看成 `O(1)`（相对整个 SST 而言），但严格来说是 `O(K)`，不是二分查找的 `O(log K)`。

理论上可以优化：既然 block 内的 entry 已经按 key 有序排列，且 `offsets` 数组本身是随机可访问的（`Vec<u16>`），完全可以在 `offsets` 上做二分查找、每次跳到某个 offset 读出对应 key 来比较，把复杂度降到 `O(log K)`。当前实现为了简单直接选择了线性扫描。

## 2. Where does the cursor stop when you seek a non-existent key in your implementation?

同样是"找第一个 `>= key` 的位置"（lower bound）语义，不要求精确匹配：

- 如果 `key` 比 block 内所有 key 都大，循环会一路 `next()` 到底，最终 `is_valid()` 变 `false`（`next()` 在越界时把 `self.key.clear()`，见 `iterator.rs` 的 `next()`），游标停在"末尾之后"的无效状态。
- 如果 `key` 落在两个已有 key 之间（不存在但在范围内），循环会在第一个 `>= key` 的已有 key 处停下，此时 `is_valid()` 为 `true`，`key()` 返回的是这个"比目标稍大"的邻近 key，而不是目标 key 本身。

调用方（`SsTableIterator`）需要自己通过 `is_valid()`/比较 `key()` 是否真的等于目标 key，来判断这是"精确命中"还是"落在了某个更大 key 上"。

## 3. Can `Block` be changed to use `Bytes` and `Arc<[u16]>`, returning `Bytes` instead of `&[u8]`? Pros/cons?

目前 `Block` 是：

```rust
pub struct Block {
    pub(crate) data: Vec<u8>,
    pub(crate) offsets: Vec<u16>,
}
```

改成 `data: Bytes` + `offsets: Arc<[u16]>`、并让 `key()`/`value()` 返回 `Bytes`（通过 `Bytes::slice` 零拷贝切片）是可行的，而且是常见的优化方向：

**优点**：
- `Bytes::clone()` 只是增加引用计数，不拷贝底层数据；`Block` 本身已经是 `Arc<Block>` 被多处共享（比如 block cache），如果 `key()`/`value()` 也能零拷贝地把内部切片"借出去"变成一个独立、可自由传递（甚至跨线程/跨迭代器存活更久）的 `Bytes`，比现在返回 `&[u8]`（生命周期绑定在 `&self` 上，用完就必须归还）更灵活——比如可以直接存进一个 `Vec<(Bytes, Bytes)>` 里而不需要立刻拷贝。
- `offsets: Arc<[u16]>` 同理，可以在多个 `Block` 的"视图"/clone 之间共享，不必每次深拷贝整个 offsets 数组。

**缺点**：
- `Bytes` 本身比 `&[u8]` 多了一层引用计数（`Arc`-like）的开销，每次 clone 有原子操作的成本（虽然很小），如果调用方本来就只是"读一下就扔"（像我们现在 `key()` 返回的 `&[u8]` 大多数场景都是这种短生命周期用法），额外引入引用计数反而是不必要的开销。
- API 从"借用"变成"事实上的共享所有权"，容易让调用方误以为拿到的 `Bytes` 是独立数据而长期持有，从而间接让整个底层 `data` buffer 因为被一小段引用钉住而无法释放（`Bytes::slice` 底层默认仍然引用整个原始 buffer），造成内存"假泄漏"（一小块数据被引用导致一大块 buffer 无法回收）。
- 改动面较大：所有实现 `StorageIterator` 的迭代器接口都要跟着从 `&[u8]` 换成 `Bytes`，是一个贯穿全部代码的破坏性变更。

总体是"更灵活的所有权/共享语义" vs "更小的运行时开销和更简单的生命周期心智模型"之间的权衡，具体选哪个取决于上层调用模式（是不是经常需要长期持有 key/value 切片）。

## 4. What is the endian of the numbers written into the blocks in your implementation?

**小端序（little-endian）**，并且从 builder 到 iterator 到 `Block::encode`/`decode` 全程保持一致：

```rust
// block/builder.rs
self.data.extend_from_slice(&(key.len() as u16).to_le_bytes());
...
self.data.extend_from_slice(&(value.len() as u16).to_le_bytes());
```

```rust
// block/iterator.rs
let key_len = u16::from_le_bytes([data[0], data[1]]) as usize;
...
let value_len = u16::from_le_bytes([data[key_end], data[key_end + 1]]) as usize;
```

```rust
// block.rs Block::encode / decode
buf.extend_from_slice(&off.to_le_bytes());
...
let num = u16::from_le_bytes([data[num_start], data[num_start + 1]]) as usize;
```

（顺带一提：`table.rs` 里 `BlockMeta::encode_block_meta`/`decode_block_meta` 之前也统一改成了 `_le` 变体，是我们在做 Task 3 时修的一个字节序不一致的 bug——block 内部一直是小端序，但 SST 级别的 meta 编解码之前一个用小端写、一个用大端读，导致解析出错误的 offset/长度。）

## 5. Is your implementation prune to a maliciously-built block? Will there be invalid memory access or OOMs?

**不完全安全，存在被恶意/损坏数据触发 panic 的风险，但不会有真正的内存不安全（UB）或无限制 OOM**，原因：

- `Block::decode` 一上来就做 `data.len() - 2` 来读元素个数（`num`），如果攻击者构造一个长度小于 2 字节的 `data`，这里在 debug 模式下会直接 `usize` 下溢 panic（release 模式下 wrapping 减法会得到一个巨大的 `num_start`，后续切片会越界 panic）。这是当前实现没有做校验的地方。
- `offsets`/`key_len`/`value_len` 都是 `u16`，天然上限 65535，不会因为读到一个"声称超大"的长度字段就去分配 GB 级别的内存，所以**不会有 OOM 风险**。
- 但如果 `offsets` 数组里的某个 offset 值和实际 `data` 长度不匹配（比如声称的 offset 超出 `data` 实际范围），`block/iterator.rs` 里直接用 `data[offset]`、`data[key_start..key_end]` 这种裸索引/切片访问，会直接 **panic（index out of bounds）**，而不是优雅地返回错误。
- 所有内存访问都是安全 Rust 里的边界检查数组访问（没有 `unsafe`），所以最坏情况是**程序 panic 崩溃（可用性问题/DoS）**，不会出现真正的越界读写、悬垂指针等内存安全问题（UB）。

如果要在生产环境里对抗恶意/损坏的 SST 文件（这也是官方教程后续 `week1-07`/checksum 部分要处理的方向），需要：在 `decode` 之前先校验长度、给每个 block 加 checksum 并在读取时校验、把裸索引访问替换成 `get()`/`checked_sub` 等返回 `Option`/`Result` 的安全访问方式，逐层转换成可恢复的错误而不是 panic。

## 6. Can a block contain duplicated keys?

**可以，`BlockBuilder::add` 没有做任何去重/唯一性校验**：

```rust
pub fn add(&mut self, key: KeySlice, value: &[u8]) -> bool {
    ...
    self.offsets.push(self.data.len() as u16);
    // 直接按 key 原样写入，不检查是否和之前的 key 相同
    ...
}
```

只要调用方连续 `add` 两次相同的 key，两条记录都会被原样写进 `data`，各自有独立的 offset。这在 week 1（无 MVCC）阶段属于调用方需要自己保证"不重复调用相同 key"的隐含约定（比如从 memtable flush 出来的 key 已经是唯一的，因为 skiplist `insert` 本身就是覆盖写）。到了 week 3 引入 `key+ts` 版本控制之后，"同一个用户 key、不同 ts"在物理编码上就是不同的 `KeySlice`，天然不会冲突，但从"用户 key"的角度看，一个 block 里出现多条相同用户 key 不同版本，也是完全合法且预期内的场景。

需要注意：如果真的写入了完全相同的 key（连同版本一起相同），`seek_to_key` 会停在第一条匹配的记录上（线性扫描碰到第一个 `>= key` 就停），后面重复的那条不会被自动跳过或去重，属于未定义的业务语义（调用方不应该制造这种情况）。

## 7. What happens if the user adds a key larger than the target block size?

`BlockBuilder::add` 的判断逻辑：

```rust
let now_size = self.data.len() + self.offsets.len() * 2 + 2;
let added_entry_size = 2 + key.len() + 2 + value.len();
if now_size + added_entry_size + 2 > self.block_size && !self.first_key.is_empty() {
    return false;
}
```

关键在 `&& !self.first_key.is_empty()` 这个条件：**只有当 block 里已经至少有一条记录时，才会因为"超出 block_size"而拒绝新记录**。如果这是 block 里的第一条记录（`first_key` 为空），即使这一条记录自身编码后就已经超过 `block_size`，也会被强制接受（`add` 返回 `true`）。

也就是说：一个超大的单条 key/value，会独占一整个 block，这个 block 的实际大小会**超过配置的目标 `block_size`**，但不会报错、不会丢数据、不会死循环。这是一个常见且合理的设计——`block_size` 只是"尽量控制在这个大小"的软性目标，不是硬性上限，保证任何合法的 key/value 都总能被写进去。

## 8. LSM engine built on object store (S3) — how to adjust block format/parameters?

（这一题在 Day 4 SST 篇的理解题里也出现过类似问题，这里从 block 这一层的角度回答）

- **调大 `block_size`**：默认几 KB 级别的 block 对 S3 这种按请求计费、有网络往返延迟的存储来说太小了，一次 GET 摊到的数据太少，请求次数和延迟都会成为瓶颈。应该调大到几百 KB 甚至更大，让一次网络请求能拿到足够多的数据。
- **给每个 block 加 checksum**：本地文件系统下磁盘位错概率低，但走网络/对象存储传输链路更长，出错概率相对更高，读回 block 后应该先校验 checksum 再解码使用（这也是官方教程后续章节的内容）。
- **减少"小碎读"**：当前 `read_block`/`read_block_cached` 是按需读取单个 block（对应 S3 上就是一次 range GET），如果扫描是顺序的，应该考虑批量预取相邻多个 block，减少请求次数；`offsets` 编码本身是紧凑的变长格式，也可以考虑允许一次性把多个逻辑上相邻的 block 打包成一个更大的对象读取单元。
- **block cache 更激进**：网络延迟远高于本地磁盘，一次 cache miss 的代价大得多，应该尽量提高 block cache 的命中率（更大容量、更智能的淘汰策略，甚至加一层本地磁盘缓存做二级缓存），减少回源到 S3 的次数。

## 9. Do you love bubble tea? Why or why not?

（这是原教程里的一道"轻松/闲聊"题，不涉及具体实现，纯属个人喜好，这里就不代答了——如果你想让我按你的口味写一句，告诉我就行。）
