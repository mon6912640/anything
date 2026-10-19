//! 内存索引：按列存储（struct of arrays），所有文件名拼在一块连续内存里，
//! 搜索时可以直接对整块内存做 SIMD 子串查找。

use rayon::prelude::*;

use crate::util::{fold, key_hash};

pub const NONE: u32 = u32::MAX;
pub const F_DIR: u8 = 1;
/// 已删除（条目保留，下次保存/重建时清理）
pub const F_DEL: u8 = 2;
/// 目录但未递归进入（符号链接/挂载点/排除/无权限）
pub const F_NOREC: u8 = 4;

const MAX_DEPTH: usize = 4096;

/// 查找表空槽 / 已删除槽
const EMPTY: u32 = u32::MAX;
const TOMB: u32 = u32::MAX - 1;

/// 哈希值映射到 [0, cap)，不要求 cap 是 2 的幂
#[inline]
fn bucket(h: u64, cap: usize) -> usize {
    ((h as u128 * cap as u128) >> 64) as usize
}

#[derive(Default)]
pub struct Index {
    /// 原始文件名（UTF-8），每个后面跟一个 0
    pub names: Vec<u8>,
    /// 小写折叠后的文件名，与 names 逐字节对齐
    pub folded: Vec<u8>,
    /// off[i] 为第 i 个名字的起始位置，长度 n+1
    pub off: Vec<u32>,
    pub parent: Vec<u32>,
    pub flags: Vec<u8>,
    pub size: Vec<u64>,
    /// 修改时间（FILETIME, UTC）
    pub mtime: Vec<u64>,
    /// 拼音首字母串（只为含汉字的名字保存），0 分隔
    pub py_buf: Vec<u8>,
    pub py_off: Vec<u32>,
    /// 每段拼音所属条目，递增
    pub py_owner: Vec<u32>,
    /// 查找表：(父目录, 折叠名) -> 条目，开放寻址，槽里只存条目编号（4 字节），
    /// 键现场由条目算出，比标准 HashMap 省三分之二以上内存。只在实时更新时使用。
    slots: Vec<u32>,
    slot_used: usize,
    slot_tomb: usize,
    /// 预先算好的排序名次（可能比条目数短：新增条目尚未参与排名）
    pub rank_name: Vec<u32>,
    pub rank_path: Vec<u32>,
    pub deleted: usize,
}

impl Index {
    pub fn new() -> Index {
        Index::with_capacity(0, 0)
    }

    pub fn with_capacity(n: usize, name_bytes: usize) -> Index {
        let mut ix = Index::default();
        ix.names.reserve(name_bytes);
        ix.folded.reserve(name_bytes);
        ix.off.reserve(n + 1);
        ix.off.push(0);
        ix.py_off.push(0);
        ix.parent.reserve(n);
        ix.flags.reserve(n);
        ix.size.reserve(n);
        ix.mtime.reserve(n);
        ix
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn alive(&self) -> usize {
        self.len() - self.deleted
    }

    #[inline]
    pub fn span(&self, i: u32) -> (usize, usize) {
        let i = i as usize;
        (self.off[i] as usize, self.off[i + 1] as usize - 1)
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        let (a, b) = self.span(i);
        // names 中只写入过合法 UTF-8
        unsafe { std::str::from_utf8_unchecked(&self.names[a..b]) }
    }

    #[inline]
    pub fn folded_name(&self, i: u32) -> &[u8] {
        let (a, b) = self.span(i);
        &self.folded[a..b]
    }

    #[inline]
    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & F_DIR != 0
    }

    #[inline]
    pub fn is_deleted(&self, i: u32) -> bool {
        self.flags[i as usize] & F_DEL != 0
    }

    /// 追加条目，不登记查找表（批量加载时用，之后统一 rebuild_map）
    pub fn push_nomap(&mut self, name: &str, folded: &[u8], py: Option<&[u8]>, parent: u32, flags: u8, size: u64, mtime: u64) -> u32 {
        debug_assert_eq!(name.len(), folded.len());
        let id = self.parent.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        self.names.push(0);
        self.folded.extend_from_slice(folded);
        self.folded.push(0);
        self.off.push(self.names.len() as u32);
        self.parent.push(parent);
        self.flags.push(flags);
        self.size.push(size);
        self.mtime.push(mtime);
        if let Some(p) = py {
            self.py_buf.extend_from_slice(p);
            self.py_buf.push(0);
            self.py_off.push(self.py_buf.len() as u32);
            self.py_owner.push(id);
        }
        id
    }

    pub fn push(&mut self, name: &str, folded: &[u8], py: Option<&[u8]>, parent: u32, flags: u8, size: u64, mtime: u64) -> u32 {
        let id = self.push_nomap(name, folded, py, parent, flags, size, mtime);
        self.table_insert(key_hash(parent, folded), id);
        id
    }

    #[inline]
    fn entry_hash(&self, id: u32) -> u64 {
        key_hash(self.parent[id as usize], self.folded_name(id))
    }

    fn table_insert(&mut self, h: u64, id: u32) {
        if (self.slot_used + self.slot_tomb + 1) * 100 > self.slots.len() * 85 {
            self.table_resize((self.slot_used + 1) * 3 / 2 + 1024);
        }
        let cap = self.slots.len();
        let mut i = bucket(h, cap);
        loop {
            let s = self.slots[i];
            if s == EMPTY || s == TOMB {
                if s == TOMB {
                    self.slot_tomb -= 1;
                }
                self.slots[i] = id;
                self.slot_used += 1;
                return;
            }
            i += 1;
            if i == cap {
                i = 0;
            }
        }
    }

    fn table_resize(&mut self, cap: usize) {
        let old = std::mem::replace(&mut self.slots, vec![EMPTY; cap]);
        self.slot_used = 0;
        self.slot_tomb = 0;
        for id in old {
            if id != EMPTY && id != TOMB {
                let h = self.entry_hash(id);
                self.table_insert(h, id);
            }
        }
    }

    /// 重建查找表（加载索引、批量扫描后调用）
    pub fn rebuild_map(&mut self) {
        let n = self.len();
        let hashes: Vec<u64> = (0..n as u32).into_par_iter().map(|i| self.entry_hash(i)).collect();
        let cap = (self.alive() * 4 / 3).max(1024);
        self.slots = vec![EMPTY; cap];
        self.slot_used = 0;
        self.slot_tomb = 0;
        for (i, h) in hashes.into_iter().enumerate() {
            if self.flags[i] & F_DEL == 0 {
                self.table_insert(h, i as u32);
            }
        }
    }

    pub fn lookup(&self, parent: u32, folded: &[u8]) -> Option<u32> {
        let cap = self.slots.len();
        if cap == 0 {
            return None;
        }
        let mut i = bucket(key_hash(parent, folded), cap);
        loop {
            let s = self.slots[i];
            if s == EMPTY {
                return None;
            }
            if s != TOMB && self.parent[s as usize] == parent && self.folded_name(s) == folded {
                return Some(s);
            }
            i += 1;
            if i == cap {
                i = 0;
            }
        }
    }

    fn unmap(&mut self, id: u32) {
        let cap = self.slots.len();
        if cap == 0 {
            return;
        }
        let mut i = bucket(self.entry_hash(id), cap);
        loop {
            let s = self.slots[i];
            if s == EMPTY {
                return;
            }
            if s == id {
                self.slots[i] = TOMB;
                self.slot_used -= 1;
                self.slot_tomb += 1;
                return;
            }
            i += 1;
            if i == cap {
                i = 0;
            }
        }
    }

    pub fn mark_deleted(&mut self, id: u32) {
        if !self.is_deleted(id) {
            self.unmap(id);
            self.flags[id as usize] |= F_DEL;
            self.deleted += 1;
        }
    }

    /// 根目录 + 相对目录 -> 目录条目
    pub fn resolve(&self, root: &str, rel_dir: &str) -> Option<u32> {
        let mut cur = self.lookup(NONE, &fold(root))?;
        for comp in rel_dir.split('\\').filter(|c| !c.is_empty()) {
            let c = self.lookup(cur, &fold(comp))?;
            if !self.is_dir(c) {
                return None;
            }
            cur = c;
        }
        Some(cur)
    }

    /// 目录改名/移动后，把它的直接子项挂到新条目下
    pub fn repoint_children(&mut self, old: u32, new: u32) {
        let kids: Vec<u32> = self
            .parent
            .par_iter()
            .enumerate()
            .filter(|&(i, &p)| p == old && self.flags[i] & F_DEL == 0)
            .map(|(i, _)| i as u32)
            .collect();
        for k in kids {
            self.unmap(k);
            self.parent[k as usize] = new;
            let h = self.entry_hash(k);
            self.table_insert(h, k);
        }
    }

    /// 每个条目是否“活着”：自身及所有祖先都未删除
    pub fn alive_mask(&self) -> Vec<bool> {
        let n = self.len();
        // 0 未知, 1 活, 2 死
        let mut state = vec![0u8; n];
        let mut chain: Vec<usize> = Vec::new();
        for i in 0..n {
            if state[i] != 0 {
                continue;
            }
            chain.clear();
            let mut x = i as u32;
            let verdict = loop {
                if x == NONE {
                    break 1;
                }
                let xi = x as usize;
                if state[xi] != 0 {
                    break state[xi];
                }
                if self.flags[xi] & F_DEL != 0 {
                    state[xi] = 2;
                    break 2;
                }
                chain.push(xi);
                if chain.len() > MAX_DEPTH {
                    break 2;
                }
                x = self.parent[xi];
            };
            for &c in &chain {
                state[c] = verdict;
            }
        }
        state.into_iter().map(|s| s == 1).collect()
    }

    /// 每个条目所属的根目录条目（自身或祖先已删除则为 NONE）
    pub fn root_ids(&self) -> Vec<u32> {
        const UNK: u32 = u32::MAX - 1;
        let n = self.len();
        let mut root = vec![UNK; n];
        let mut chain: Vec<usize> = Vec::new();
        for i in 0..n {
            if root[i] != UNK {
                continue;
            }
            chain.clear();
            let mut x = i as u32;
            let r = loop {
                let xi = x as usize;
                if root[xi] != UNK {
                    break root[xi];
                }
                if self.flags[xi] & F_DEL != 0 {
                    root[xi] = NONE;
                    break NONE;
                }
                chain.push(xi);
                let p = self.parent[xi];
                if p == NONE {
                    break x;
                }
                if chain.len() > MAX_DEPTH {
                    break NONE;
                }
                x = p;
            };
            for &c in &chain {
                root[c] = r;
            }
        }
        root
    }

    /// 复制出指定根目录下的所有有效条目（同时清理已删除条目）
    pub fn extract(&self, keep_root: impl Fn(&str) -> bool) -> Index {
        let n = self.len();
        let roots = self.root_ids();
        let keep: Vec<bool> = (0..n as u32).map(|i| self.parent[i as usize] == NONE && !self.is_deleted(i) && keep_root(self.name(i))).collect();
        let mut newid = vec![NONE; n];
        let mut m = 0u32;
        let mut bytes = 0usize;
        for i in 0..n {
            let r = roots[i];
            if r != NONE && keep[r as usize] {
                newid[i] = m;
                m += 1;
                bytes += (self.off[i + 1] - self.off[i]) as usize;
            }
        }
        let mut out = Index::with_capacity(m as usize, bytes);
        let mut k = 0usize;
        for i in 0..n {
            while k < self.py_owner.len() && (self.py_owner[k] as usize) < i {
                k += 1;
            }
            if newid[i] == NONE {
                continue;
            }
            let py = if k < self.py_owner.len() && self.py_owner[k] as usize == i {
                Some(&self.py_buf[self.py_off[k] as usize..self.py_off[k + 1] as usize - 1])
            } else {
                None
            };
            let p = self.parent[i];
            let np = if p == NONE { NONE } else { newid[p as usize] };
            out.push_nomap(self.name(i as u32), self.folded_name(i as u32), py, np, self.flags[i], self.size[i], self.mtime[i]);
        }
        out.rebuild_map();
        out
    }

    /// 把另一个索引整体追加进来（之后需调用 rebuild_map）
    pub fn append(&mut self, o: &Index) {
        let base = self.len() as u32;
        let base_bytes = self.names.len() as u32;
        let base_py = self.py_buf.len() as u32;
        self.names.extend_from_slice(&o.names);
        self.folded.extend_from_slice(&o.folded);
        self.off.extend(o.off[1..].iter().map(|&x| x + base_bytes));
        self.parent.extend(o.parent.iter().map(|&p| if p == NONE { NONE } else { p + base }));
        self.flags.extend_from_slice(&o.flags);
        self.size.extend_from_slice(&o.size);
        self.mtime.extend_from_slice(&o.mtime);
        self.py_buf.extend_from_slice(&o.py_buf);
        self.py_off.extend(o.py_off[1..].iter().map(|&x| x + base_py));
        self.py_owner.extend(o.py_owner.iter().map(|&x| x + base));
        self.deleted += o.deleted;
        self.rank_name.clear();
        self.rank_path.clear();
    }

    /// 目录被删除后，把它下面的所有条目也标记为删除
    pub fn propagate_deletes(&mut self) {
        let mask = self.alive_mask();
        for (i, alive) in mask.into_iter().enumerate() {
            if !alive {
                self.mark_deleted(i as u32);
            }
        }
    }

    pub fn full_path(&self, id: u32) -> String {
        let mut parts = Vec::with_capacity(16);
        let mut x = id;
        while x != NONE && parts.len() < MAX_DEPTH {
            parts.push(x);
            x = self.parent[x as usize];
        }
        let mut s = String::with_capacity(parts.len() * 16);
        for (k, &p) in parts.iter().rev().enumerate() {
            if k > 0 {
                s.push('\\');
            }
            s.push_str(self.name(p));
        }
        if parts.len() == 1 {
            s.push('\\');
        }
        s
    }

    /// 所在目录（列表“路径”列）
    pub fn dir_display(&self, id: u32) -> String {
        match self.parent[id as usize] {
            NONE => String::new(),
            p => self.full_path(p),
        }
    }

    pub fn pinyin_of(&self, id: u32) -> Option<&[u8]> {
        let k = self.py_owner.binary_search(&id).ok()?;
        Some(&self.py_buf[self.py_off[k] as usize..self.py_off[k + 1] as usize - 1])
    }

    pub fn ranked(&self) -> bool {
        !self.rank_name.is_empty() || self.is_empty()
    }

    pub fn unranked(&self) -> usize {
        self.len() - self.rank_name.len()
    }

    /// 计算名称、路径两种排序名次（只读，可在读锁下后台进行）
    pub fn compute_ranks(&self) -> (Vec<u32>, Vec<u32>) {
        let n = self.len();
        let mut ids: Vec<u32> = (0..n as u32).collect();
        ids.par_sort_unstable_by(|&a, &b| self.folded_name(a).cmp(self.folded_name(b)).then(a.cmp(&b)));
        let mut rank_name = vec![0u32; n];
        for (r, &i) in ids.iter().enumerate() {
            rank_name[i as usize] = r as u32;
        }

        // 所有目录的完整路径排序
        let dirs: Vec<u32> = (0..n as u32).filter(|&i| self.flags[i as usize] & F_DIR != 0).collect();
        let mut slot = vec![NONE; n];
        for (k, &d) in dirs.iter().enumerate() {
            slot[d as usize] = k as u32;
        }
        let mut paths: Vec<Vec<u8>> = vec![Vec::new(); dirs.len()];
        let mut done = vec![false; dirs.len()];
        let mut stack: Vec<u32> = Vec::new();
        for k in 0..dirs.len() {
            if done[k] {
                continue;
            }
            stack.clear();
            let mut x = dirs[k];
            loop {
                let s = slot[x as usize];
                if s == NONE || done[s as usize] || stack.len() > MAX_DEPTH {
                    break;
                }
                stack.push(s);
                let p = self.parent[x as usize];
                if p == NONE {
                    break;
                }
                x = p;
            }
            for &s in stack.iter().rev() {
                let d = dirs[s as usize];
                let p = self.parent[d as usize];
                let mut v = if p != NONE && slot[p as usize] != NONE { paths[slot[p as usize] as usize].clone() } else { Vec::new() };
                if !v.is_empty() {
                    v.push(b'\\');
                }
                v.extend_from_slice(self.folded_name(d));
                paths[s as usize] = v;
                done[s as usize] = true;
            }
        }
        let mut order: Vec<u32> = (0..dirs.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| paths[a as usize].cmp(&paths[b as usize]));
        drop(paths);
        let mut dir_rank = vec![0u32; dirs.len()];
        for (r, &s) in order.iter().enumerate() {
            dir_rank[s as usize] = r as u32;
        }
        let keys: Vec<u64> = (0..n)
            .into_par_iter()
            .map(|i| {
                let p = self.parent[i];
                let hi = if p == NONE || slot[p as usize] == NONE { 0 } else { dir_rank[slot[p as usize] as usize] as u64 + 1 };
                (hi << 32) | rank_name[i] as u64
            })
            .collect();
        ids.par_sort_unstable_by_key(|&i| keys[i as usize]);
        let mut rank_path = vec![0u32; n];
        for (r, &i) in ids.iter().enumerate() {
            rank_path[i as usize] = r as u32;
        }
        (rank_name, rank_path)
    }

    /// 释放扩容时多预留的空间（扫描完成后调用）
    pub fn shrink(&mut self) {
        self.names.shrink_to_fit();
        self.folded.shrink_to_fit();
        self.off.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.py_buf.shrink_to_fit();
        self.py_off.shrink_to_fit();
        self.py_owner.shrink_to_fit();
    }

    /// 估算内存占用（字节）
    pub fn mem_bytes(&self) -> usize {
        self.names.capacity()
            + self.folded.capacity()
            + self.off.capacity() * 4
            + self.parent.capacity() * 4
            + self.flags.capacity()
            + self.size.capacity() * 8
            + self.mtime.capacity() * 8
            + self.py_buf.capacity()
            + self.py_off.capacity() * 4
            + self.py_owner.capacity() * 4
            + self.slots.capacity() * 4
            + self.rank_name.capacity() * 4
            + self.rank_path.capacity() * 4
    }
}
