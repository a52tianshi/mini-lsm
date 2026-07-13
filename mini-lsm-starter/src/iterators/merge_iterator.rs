// Copyright (c) 2022-2025 Alex Chi Z
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use std::cmp::{self};
use std::collections::BinaryHeap;

use crate::key::KeySlice;
use anyhow::Result;

use super::StorageIterator;

struct HeapWrapper<I: StorageIterator>(pub usize, pub Box<I>);

impl<I: StorageIterator> PartialEq for HeapWrapper<I> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == cmp::Ordering::Equal
    }
}

impl<I: StorageIterator> Eq for HeapWrapper<I> {}

impl<I: StorageIterator> PartialOrd for HeapWrapper<I> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<I: StorageIterator> Ord for HeapWrapper<I> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.1
            .key()
            .cmp(&other.1.key())
            .then(self.0.cmp(&other.0))
            .reverse()
    }
}

/// Merge multiple iterators of the same type. If the same key occurs multiple times in some
/// iterators, prefer the one with smaller index.
pub struct MergeIterator<I: StorageIterator> {
    iters: BinaryHeap<HeapWrapper<I>>,
    current: Option<HeapWrapper<I>>,
}

impl<I: StorageIterator> MergeIterator<I> {
    pub fn create(iters: Vec<Box<I>>) -> Self {
        //day2 cq
        let mut heap = BinaryHeap::with_capacity(iters.len());

        for (idx, iter) in iters.into_iter().enumerate() {
            if iter.is_valid() {
                heap.push(HeapWrapper(idx, iter));
            }
        }

        let current = heap.pop();

        MergeIterator {
            iters: heap,
            current,
        }
    }
}

impl<I: 'static + for<'a> StorageIterator<KeyType<'a>=KeySlice<'a>>> StorageIterator
for MergeIterator<I>
{
    type KeyType<'a> = KeySlice<'a>;

    fn key(&self) -> KeySlice<'_> {
        self.current.as_ref().unwrap().1.key()
    }

    fn value(&self) -> &[u8] {
        self.current.as_ref().unwrap().1.value()
    }

    fn is_valid(&self) -> bool {
        let key = self.current.as_ref();
        match key { 
            Some(key) => key.1.is_valid(),
            None => false,
        }
    }

    fn next(&mut self) -> Result<()> {
        // self.current.take();
        // Ok(())

        // current 是目前正在被外部读取的那个迭代器(key 最小、idx 最小)。
        // 堆 self.iters 里存的是"除 current 以外"的所有迭代器。
        let current = self.current.as_mut().unwrap();

        // 第一步:清掉堆里所有跟 current 拥有相同 key 的迭代器。
        // 因为语义是"同一个 key 只暴露 idx 最小的那份数据",所以其余持有
        // 相同 key 的迭代器要在这里被"预先推进一次",把这条重复的 key 吞掉,
        // 不能留到下一轮 next() 才处理,否则外部会看到重复的 key。
        while let Some(mut top) = self.iters.pop() {
            // pop() 拿到的一定是堆里当前最小的那个(即最可能和 current 重复的)。
            if top.1.key() == current.1.key() {
                // key 相同 -> 这是一条需要被跳过的重复数据,把它的迭代器往前推一格。
                top.1.next()?;
                if top.1.is_valid() {
                    // 推进后还有效,说明它后面还有别的 key,放回堆里参与之后的排序。
                    self.iters.push(top);
                }
                // 如果推进后失效了(数据读完了),就不再放回堆,相当于直接丢弃。
            } else {
                // key 不同,说明堆里已经没有跟 current 重复的元素了。
                // 注意:因为 pop() 每次拿的都是当前最小的元素,一旦这次不相等,
                // 后面剩下的只会更大,所以可以直接放回去然后退出循环。
                self.iters.push(top);
                break;
            }
        }

        // 第二步:把 current 自己往前推进一格,离开它当前指向的 key。
        current.1.next()?;

        // 第三步:如果 current 被推进后失效了(说明它这一路的数据读完了),
        // 就需要从堆里取出新的最小元素来顶替它,成为新的 current。
        if !current.1.is_valid() {
            if let Some(iter) = self.iters.pop() {
                *current = iter;
            }
            // 如果堆也空了,current 会保持 invalid,is_valid() 之后会返回 false,
            // 表示整个 MergeIterator 已经遍历完毕。
            return Ok(());
        }

        // 第四步:current 仍然有效,但它推进之后的新 key 不一定还是全局最小的了,
        // 需要跟堆顶比较一下,谁小谁才应该继续留在 current 位置。
        // 这里用 peek_mut 而不是 pop+push,是为了在"不需要交换"的情况下
        // 省去一次多余的 push(peek_mut 在 drop 时会自动帮你把堆重新排好序)。
        if let Some(mut inner_iter) = self.iters.peek_mut() {
            // HeapWrapper 的 Ord 是特意 reverse() 过的(方便用大顶堆模拟小顶堆), //important
            // 所以这里"真正更小"对应的比较结果是 Greater,即要用 `>` 而不是 `<`。!!!!!!!!!!!!!!!!
            if *inner_iter > *current {
                // 堆顶其实比 current 更小 -> 交换,让更小的那个继续留在 current。
                std::mem::swap(&mut *inner_iter, current);
            }
        }

        Ok(())
    }
}
