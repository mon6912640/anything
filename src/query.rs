//! 搜索语法解析与执行。
//!
//! 语法（与 Everything 接近）：
//!   abc def        同时包含 abc 和 def（空格 = 并且）
//!   a|b            包含 a 或 b
//!   !abc           不包含 abc
//!   *.psd  a?c     通配符，匹配完整文件名
//!   含 \ 的词       在完整路径中匹配，如 work\ 或 \src\*.rs
//!   path:abc       在完整路径中匹配
//!   regex:^a.*\.rs$  正则（匹配文件名；regex:path:... 匹配完整路径）
//!   ext:png;jpg    扩展名
//!   file: folder:  只要文件 / 只要文件夹
//!   size:>10mb  size:1mb..1gb
//!   dm:today  dm:yesterday  dm:week  dm:month  dm:year  dm:2026-10-01  dm:>2026-01-01  dm:2026-01..2026-06
//!   "a b"          引号包住含空格的词
//! 汉字文件名可用拼音首字母搜索：xmwj -> 项目文件

use std::collections::HashMap;

use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};

use crate::index::{F_DEL, F_DIR, Index, NONE};
use crate::util::{FT_DAY, fold, local_to_filetime, today_start};

pub enum Pat {
    Sub(Finder<'static>),
    Glob(Vec<u8>),
    Re(Regex),
}

impl Pat {
    #[inline]
    fn is_match(&self, s: &[u8]) -> bool {
        match self {
            Pat::Sub(f) => f.find(s).is_some(),
            Pat::Glob(p) => glob_match(p, s),
            Pat::Re(r) => r.is_match(s),
        }
    }
    fn is_ascii(&self) -> bool {
        match self {
            Pat::Sub(f) => f.needle().is_ascii(),
            Pat::Glob(p) => p.is_ascii(),
            Pat::Re(_) => false,
        }
    }
}

pub struct Term {
    pub neg: bool,
    /// 匹配完整路径而不是文件名
    pub path: bool,
    /// 多个候选之间是“或”
    pub alts: Vec<Pat>,
    /// 可尝试拼音首字母匹配
    pub py: bool,
}

#[derive(Default)]
pub struct Query {
    pub terms: Vec<Term>,
    pub exts: Option<Vec<Vec<u8>>>,
    pub files_only: bool,
    pub dirs_only: bool,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub min_time: Option<u64>,
    pub max_time: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortCol {
    Name,
    Path,
    Size,
    Date,
    /// 不排序（索引顺序，最快）
    None,
}

impl SortCol {
    pub fn parse(s: &str) -> Option<SortCol> {
        Some(match s.to_ascii_lowercase().as_str() {
            "name" | "n" => SortCol::Name,
            "path" | "p" => SortCol::Path,
            "size" | "s" => SortCol::Size,
            "date" | "dm" | "d" | "date-modified" => SortCol::Date,
            "none" => SortCol::None,
            _ => return None,
        })
    }
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for ch in s.chars() {
        if ch == '"' {
            in_q = !in_q;
            continue;
        }
        if !in_q && ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    if s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

fn parse_size(v: &str) -> Result<u64, String> {
    let t = v.trim().to_ascii_lowercase();
    let t = t.strip_suffix('b').unwrap_or(&t).to_string();
    let (num, mul) = if let Some(x) = t.strip_suffix('k') {
        (x.to_string(), 1u64 << 10)
    } else if let Some(x) = t.strip_suffix('m') {
        (x.to_string(), 1 << 20)
    } else if let Some(x) = t.strip_suffix('g') {
        (x.to_string(), 1 << 30)
    } else if let Some(x) = t.strip_suffix('t') {
        (x.to_string(), 1 << 40)
    } else {
        (t.clone(), 1)
    };
    let n: f64 = num.trim().parse().map_err(|_| format!("无法识别的大小: {v}"))?;
    Ok((n * mul as f64) as u64)
}

/// 返回该日期描述覆盖的 [起, 止) 区间
fn parse_date(v: &str) -> Result<(u64, u64), String> {
    let v = v.trim().to_ascii_lowercase();
    let today = today_start();
    let now_end = today + FT_DAY;
    let r = match v.as_str() {
        "today" | "今天" => (today, now_end),
        "yesterday" | "昨天" => (today - FT_DAY, today),
        "week" | "thisweek" | "lastweek" | "7d" => (today - 6 * FT_DAY, now_end),
        "month" | "thismonth" | "30d" => (today - 29 * FT_DAY, now_end),
        "year" | "thisyear" | "365d" => (today - 364 * FT_DAY, now_end),
        _ => {
            let parts: Vec<u16> = v
                .split(['-', '/', '.'])
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<u16>().map_err(|_| format!("无法识别的日期: {v}")))
                .collect::<Result<_, _>>()?;
            let bad = || format!("无法识别的日期: {v}");
            match parts.as_slice() {
                [y] => (local_to_filetime(*y, 1, 1, 0, 0).ok_or_else(bad)?, local_to_filetime(*y + 1, 1, 1, 0, 0).ok_or_else(bad)?),
                [y, m] => {
                    let (ny, nm) = if *m >= 12 { (*y + 1, 1) } else { (*y, *m + 1) };
                    (local_to_filetime(*y, *m, 1, 0, 0).ok_or_else(bad)?, local_to_filetime(ny, nm, 1, 0, 0).ok_or_else(bad)?)
                }
                [y, m, d] => {
                    let s = local_to_filetime(*y, *m, *d, 0, 0).ok_or_else(bad)?;
                    (s, s + FT_DAY)
                }
                _ => return Err(bad()),
            }
        }
    };
    Ok(r)
}

/// 解析 ">x" "<x" ">=x" "<=x" "a..b" "x" 形式的区间
fn parse_range<T: Copy>(v: &str, one: impl Fn(&str) -> Result<(T, T), String>) -> Result<(Option<T>, Option<T>), String> {
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { None } else { Some(one(a)?.0) };
        let hi = if b.is_empty() { None } else { Some(one(b)?.1) };
        return Ok((lo, hi));
    }
    if let Some(x) = v.strip_prefix(">=") {
        return Ok((Some(one(x)?.0), None));
    }
    if let Some(x) = v.strip_prefix("<=") {
        return Ok((None, Some(one(x)?.1)));
    }
    if let Some(x) = v.strip_prefix('>') {
        return Ok((Some(one(x)?.1), None));
    }
    if let Some(x) = v.strip_prefix('<') {
        return Ok((None, Some(one(x)?.0)));
    }
    let (a, b) = one(v.strip_prefix('=').unwrap_or(v))?;
    Ok((Some(a), Some(b)))
}

fn make_pat(body: &str, regex: bool) -> Result<Pat, String> {
    if regex {
        let r = RegexBuilder::new(body)
            .case_insensitive(true)
            .unicode(true)
            .size_limit(1 << 24)
            .build()
            .map_err(|e| format!("正则错误: {e}"))?;
        return Ok(Pat::Re(r));
    }
    let f = fold(body);
    if f.contains(&b'*') || f.contains(&b'?') {
        Ok(Pat::Glob(f))
    } else {
        Ok(Pat::Sub(Finder::new(&f).into_owned()))
    }
}

impl Query {
    pub fn parse(text: &str, pinyin: bool) -> Result<Query, String> {
        let mut q = Query::default();
        for tok in tokenize(text) {
            let (neg, tok) = match tok.strip_prefix('!') {
                Some(rest) if !rest.is_empty() => (true, rest.to_string()),
                _ => (false, tok),
            };
            let t = tok.as_str();
            let lower = t.to_ascii_lowercase();
            if lower == "file:" || lower == "files:" {
                q.files_only = true;
                continue;
            }
            if lower == "folder:" || lower == "dir:" || lower == "folders:" {
                q.dirs_only = true;
                continue;
            }
            if let Some(v) = strip_prefix_ci(t, "ext:") {
                let list: Vec<Vec<u8>> = v.split([';', ',', '|']).map(|e| fold(e.trim().trim_start_matches('.'))).filter(|e| !e.is_empty()).collect();
                q.exts = Some(list);
                continue;
            }
            if let Some(v) = strip_prefix_ci(t, "size:") {
                // 区间统一按 [起, 止) 表示，止 = 值 + 1
                let (lo, hi) = parse_range(v, |s| parse_size(s).map(|x| (x, x.saturating_add(1))))?;
                q.min_size = lo;
                q.max_size = hi.map(|h| h.saturating_sub(1));
                continue;
            }
            if let Some(v) = strip_prefix_ci(t, "dm:").or_else(|| strip_prefix_ci(t, "datemodified:")) {
                let (lo, hi) = parse_range(v, parse_date)?;
                q.min_time = lo;
                q.max_time = hi.map(|h| h.saturating_sub(1));
                continue;
            }
            let (mut body, mut regex, mut path) = (t, false, false);
            loop {
                if let Some(v) = strip_prefix_ci(body, "regex:") {
                    body = v;
                    regex = true;
                } else if let Some(v) = strip_prefix_ci(body, "path:") {
                    body = v;
                    path = true;
                } else {
                    break;
                }
            }
            if body.is_empty() {
                continue;
            }
            let alts_src: Vec<&str> = if regex { vec![body] } else { body.split('|').filter(|s| !s.is_empty()).collect() };
            if !regex && body.contains('\\') {
                path = true;
            }
            let alts = alts_src.into_iter().map(|a| make_pat(a, regex)).collect::<Result<Vec<_>, _>>()?;
            if alts.is_empty() {
                continue;
            }
            let py = pinyin && !path && alts.iter().all(|p| p.is_ascii());
            q.terms.push(Term { neg, path, alts, py });
        }
        // 文件名条件先算，路径条件（需要拼路径）放后面
        q.terms.sort_by_key(|t| t.path);
        Ok(q)
    }

    pub fn is_match_all(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_none()
            && !self.files_only
            && !self.dirs_only
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.min_time.is_none()
            && self.max_time.is_none()
    }
}

#[inline]
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// 通配符匹配（* 任意串，? 任意一个字符），整串匹配
pub fn glob_match(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star_p, mut star_s) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            si += utf8_len(s[si]);
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = pi;
            pi += 1;
            star_s = si;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_s += utf8_len(s[star_s]);
            si = star_s;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// 每个线程一份：目录完整路径缓存（折叠后）
#[derive(Default)]
struct PathCache {
    dirs: HashMap<u32, Vec<u8>>,
    buf: Vec<u8>,
}

fn dir_path<'c>(ix: &Index, d: u32, dirs: &'c mut HashMap<u32, Vec<u8>>) -> &'c [u8] {
    if !dirs.contains_key(&d) {
        let mut chain = Vec::new();
        let mut x = d;
        while x != NONE && !dirs.contains_key(&x) && chain.len() < 4096 {
            chain.push(x);
            x = ix.parent[x as usize];
        }
        let mut base: Vec<u8> = if x == NONE { Vec::new() } else { dirs.get(&x).cloned().unwrap_or_default() };
        for &c in chain.iter().rev() {
            if !base.is_empty() {
                base.push(b'\\');
            }
            base.extend_from_slice(ix.folded_name(c));
            dirs.insert(c, base.clone());
        }
    }
    dirs.get(&d).map(|v| v.as_slice()).unwrap_or(&[])
}

impl PathCache {
    fn full(&mut self, ix: &Index, id: u32) -> &[u8] {
        let p = ix.parent[id as usize];
        self.buf.clear();
        if p == NONE {
            self.buf.extend_from_slice(ix.folded_name(id));
            self.buf.push(b'\\');
        } else {
            let d = dir_path(ix, p, &mut self.dirs);
            self.buf.extend_from_slice(d);
            self.buf.push(b'\\');
            self.buf.extend_from_slice(ix.folded_name(id));
        }
        &self.buf
    }
}

#[inline]
fn term_match(ix: &Index, t: &Term, id: u32, pc: &mut PathCache) -> bool {
    if t.path {
        let full = pc.full(ix, id);
        return t.alts.iter().any(|p| p.is_match(full));
    }
    let name = ix.folded_name(id);
    if t.alts.iter().any(|p| p.is_match(name)) {
        return true;
    }
    if t.py {
        if let Some(py) = ix.pinyin_of(id) {
            return t.alts.iter().any(|p| p.is_match(py));
        }
    }
    false
}

#[inline]
fn test(ix: &Index, q: &Query, id: u32, skip: usize, pc: &mut PathCache) -> bool {
    let i = id as usize;
    let f = ix.flags[i];
    if f & F_DEL != 0 {
        return false;
    }
    let dir = f & F_DIR != 0;
    if (q.files_only && dir) || (q.dirs_only && !dir) {
        return false;
    }
    if let Some(exts) = &q.exts {
        if dir {
            return false;
        }
        let n = ix.folded_name(id);
        match memchr::memrchr(b'.', n) {
            Some(p) if exts.iter().any(|e| &n[p + 1..] == e.as_slice()) => {}
            _ => return false,
        }
    }
    if q.min_size.is_some() || q.max_size.is_some() {
        if dir {
            return false;
        }
        let s = ix.size[i];
        if q.min_size.is_some_and(|m| s < m) || q.max_size.is_some_and(|m| s > m) {
            return false;
        }
    }
    let t = ix.mtime[i];
    if q.min_time.is_some_and(|m| t < m) || q.max_time.is_some_and(|m| t > m) {
        return false;
    }
    for (k, term) in q.terms.iter().enumerate() {
        if k != skip && term_match(ix, term, id, pc) == term.neg {
            return false;
        }
    }
    true
}

/// 在 0 分隔的大缓冲区里找所有包含 needle 的段，返回段号（递增）
fn find_segments(hay: &[u8], off: &[u32], finder: &Finder, cancel: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
    let n = off.len().saturating_sub(1);
    if n == 0 {
        return Some(Vec::new());
    }
    const CH: usize = 1 << 15;
    let chunks = n.div_ceil(CH);
    let parts: Vec<Vec<u32>> = (0..chunks)
        .into_par_iter()
        .map(|c| {
            let mut out = Vec::new();
            if cancel() {
                return out;
            }
            let a = c * CH;
            let b = (a + CH).min(n);
            let end = off[b] as usize;
            let mut pos = off[a] as usize;
            let mut e = a;
            while pos < end {
                let Some(k) = finder.find(&hay[pos..end]) else { break };
                let abs = pos + k;
                e += off[e + 1..=b].partition_point(|&o| (o as usize) <= abs);
                out.push(e as u32);
                e += 1;
                pos = off[e] as usize;
            }
            out
        })
        .collect();
    if cancel() {
        return None;
    }
    let total = parts.iter().map(Vec::len).sum();
    let mut v = Vec::with_capacity(total);
    for p in parts {
        v.extend(p);
    }
    Some(v)
}

fn merge_sorted(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// 执行搜索，返回匹配条目（索引顺序）。被取消时返回 None。
pub fn search(ix: &Index, q: &Query, cancel: &(dyn Fn() -> bool + Sync)) -> Option<Vec<u32>> {
    let n = ix.len();
    if q.is_match_all() {
        let v: Vec<u32> = (0..n as u32).into_par_iter().filter(|&i| ix.flags[i as usize] & F_DEL == 0).collect();
        return if cancel() { None } else { Some(v) };
    }

    // 选一个“驱动词”：普通子串、非取反、只匹配文件名，越长越有选择性
    let driver = q
        .terms
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.neg && !t.path && t.alts.len() == 1 && matches!(t.alts[0], Pat::Sub(_)))
        .max_by_key(|(_, t)| match &t.alts[0] {
            Pat::Sub(f) => f.needle().len(),
            _ => 0,
        })
        .map(|(k, _)| k);

    const FCH: usize = 1 << 14;
    let filtered: Vec<u32> = if let Some(k) = driver {
        let t = &q.terms[k];
        let Pat::Sub(finder) = &t.alts[0] else { unreachable!() };
        let mut cands = find_segments(&ix.folded, &ix.off, finder, cancel)?;
        if t.py && !ix.py_owner.is_empty() {
            let segs = find_segments(&ix.py_buf, &ix.py_off, finder, cancel)?;
            let owners: Vec<u32> = segs.into_iter().map(|s| ix.py_owner[s as usize]).collect();
            cands = merge_sorted(cands, owners);
        }
        cands
            .par_chunks(FCH)
            .map(|chunk| {
                let mut pc = PathCache::default();
                if cancel() {
                    return Vec::new();
                }
                chunk.iter().copied().filter(|&id| test(ix, q, id, k, &mut pc)).collect::<Vec<u32>>()
            })
            .flatten_iter()
            .collect()
    } else {
        // 没有可用的驱动词：全表过滤。路径条件需要拼路径，块大一点以复用缓存
        let ch = if q.terms.iter().any(|t| t.path) { (n / rayon::current_num_threads().max(1)).max(FCH) } else { FCH * 4 };
        (0..n.div_ceil(ch.max(1)))
            .into_par_iter()
            .map(|c| {
                let mut pc = PathCache::default();
                if cancel() {
                    return Vec::new();
                }
                let a = c * ch;
                let b = (a + ch).min(n);
                (a as u32..b as u32).filter(|&id| test(ix, q, id, usize::MAX, &mut pc)).collect::<Vec<u32>>()
            })
            .flatten_iter()
            .collect()
    };
    if cancel() { None } else { Some(filtered) }
}

/// 结果排序
pub fn sort(ix: &Index, ids: &mut Vec<u32>, col: SortCol, desc: bool) {
    if ids.len() > 1 {
        match col {
            SortCol::None => {}
            SortCol::Name | SortCol::Path => {
                let rank = if col == SortCol::Name { &ix.rank_name } else { &ix.rank_path };
                if rank.is_empty() {
                    // 名次还没算好（刚建完索引）：直接比较名字
                    ids.par_sort_unstable_by(|&a, &b| ix.folded_name(a).cmp(ix.folded_name(b)).then(a.cmp(&b)));
                } else {
                    let rl = rank.len() as u32;
                    let mut keyed: Vec<u64> = ids
                        .par_iter()
                        .map(|&i| {
                            let r = if i < rl { rank[i as usize] } else { u32::MAX };
                            ((r as u64) << 32) | i as u64
                        })
                        .collect();
                    keyed.par_sort_unstable();
                    let tail = keyed.partition_point(|&k| (k >> 32) as u32 != u32::MAX);
                    for (d, k) in ids.iter_mut().zip(keyed.iter()) {
                        *d = *k as u32;
                    }
                    // 尚未排名的新条目按名字排在末尾
                    ids[tail..].sort_unstable_by(|&a, &b| ix.folded_name(a).cmp(ix.folded_name(b)));
                }
            }
            SortCol::Size | SortCol::Date => {
                let src = if col == SortCol::Size { &ix.size } else { &ix.mtime };
                let mut keyed: Vec<(u64, u32)> = ids.par_iter().map(|&i| (src[i as usize], i)).collect();
                keyed.par_sort_unstable();
                for (d, k) in ids.iter_mut().zip(keyed.iter()) {
                    *d = k.1;
                }
            }
        }
    }
    if desc {
        ids.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(glob_match(b"*.psd", b"a.psd"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"abbc"));
        assert!(glob_match("*文件?.txt".as_bytes(), "我的文件1.txt".as_bytes()));
        assert!(glob_match("项?".as_bytes(), "项目".as_bytes()));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"*.rs", b"main.rsx"));
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn size_ranges() {
        let q = Query::parse("size:>10kb", false).unwrap();
        assert_eq!((q.min_size, q.max_size), (Some(10 * 1024 + 1), None));
        let q = Query::parse("size:<=1mb", false).unwrap();
        assert_eq!((q.min_size, q.max_size), (None, Some(1 << 20)));
        let q = Query::parse("size:1kb..2kb", false).unwrap();
        assert_eq!((q.min_size, q.max_size), (Some(1024), Some(2048)));
        let q = Query::parse("size:100", false).unwrap();
        assert_eq!((q.min_size, q.max_size), (Some(100), Some(100)));
        assert!(Query::parse("size:>abc", false).is_err());
    }

    #[test]
    fn terms() {
        let q = Query::parse(r#"abc "x y" !tmp a|b work\ ext:png;JPG file:"#, true).unwrap();
        assert!(q.files_only);
        assert_eq!(q.exts.as_ref().unwrap(), &vec![b"png".to_vec(), b"jpg".to_vec()]);
        assert_eq!(q.terms.len(), 5);
        assert!(q.terms.last().unwrap().path, "路径条件排在最后");
        assert!(q.terms.iter().any(|t| t.neg));
        assert!(q.terms.iter().any(|t| t.alts.len() == 2));
        assert!(Query::parse("dm:today dm:2026-01..2026-06", false).is_ok());
        assert!(Query::parse("regex:(", false).is_err());
    }
}
