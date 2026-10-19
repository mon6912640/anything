//! 索引文件读写。数组原样整块写盘，读回时整块拷贝，几百万条目也在 1 秒内加载完。
//! 保存时顺带清理已删除条目（重新编号）。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use rayon::prelude::*;

use crate::index::{F_DEL, Index, NONE};

const MAGIC: &[u8; 8] = b"ANYIDX01";
const VERSION: u32 = 1;

fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    // 仅用于 u32/u64 等无填充的整数数组
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub fn save(ix: &Index, path: &Path) -> io::Result<()> {
    let n = ix.len();
    let alive = ix.alive_mask();
    let mut newid = vec![NONE; n];
    let mut m = 0u32;
    for i in 0..n {
        if alive[i] {
            newid[i] = m;
            m += 1;
        }
    }
    let m = m as usize;

    let mut off = Vec::with_capacity(m + 1);
    let mut parent = Vec::with_capacity(m);
    let mut flags = Vec::with_capacity(m);
    let mut size = Vec::with_capacity(m);
    let mut mtime = Vec::with_capacity(m);
    let mut o = 0u32;
    off.push(0u32);
    for i in 0..n {
        if !alive[i] {
            continue;
        }
        o += ix.off[i + 1] - ix.off[i];
        off.push(o);
        let p = ix.parent[i];
        parent.push(if p == NONE { NONE } else { newid[p as usize] });
        flags.push(ix.flags[i] & !F_DEL);
        size.push(ix.size[i]);
        mtime.push(ix.mtime[i]);
    }
    let name_bytes = o as usize;

    let mut py_owner = Vec::new();
    let mut py_off = vec![0u32];
    let mut py_len = 0u32;
    for (k, &own) in ix.py_owner.iter().enumerate() {
        if alive[own as usize] {
            py_owner.push(newid[own as usize]);
            py_len += ix.py_off[k + 1] - ix.py_off[k];
            py_off.push(py_len);
        }
    }

    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        for v in [m as u64, name_bytes as u64, py_owner.len() as u64, py_len as u64] {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(as_bytes(&off))?;
        w.write_all(as_bytes(&parent))?;
        w.write_all(&flags)?;
        w.write_all(as_bytes(&size))?;
        w.write_all(as_bytes(&mtime))?;
        for buf in [&ix.names, &ix.folded] {
            for i in 0..n {
                if alive[i] {
                    w.write_all(&buf[ix.off[i] as usize..ix.off[i + 1] as usize])?;
                }
            }
        }
        w.write_all(as_bytes(&py_off))?;
        w.write_all(as_bytes(&py_owner))?;
        for (k, &own) in ix.py_owner.iter().enumerate() {
            if alive[own as usize] {
                w.write_all(&ix.py_buf[ix.py_off[k] as usize..ix.py_off[k + 1] as usize])?;
            }
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("索引文件损坏: {msg}"))
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or_else(|| bad("长度溢出"))?;
        if end > self.d.len() {
            return Err(bad("文件被截断"));
        }
        let s = &self.d[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn vec_u32(&mut self, n: usize) -> io::Result<Vec<u32>> {
        let b = self.bytes(n.checked_mul(4).ok_or_else(|| bad("长度溢出"))?)?;
        Ok(b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
    }
    fn vec_u64(&mut self, n: usize) -> io::Result<Vec<u64>> {
        let b = self.bytes(n.checked_mul(8).ok_or_else(|| bad("长度溢出"))?)?;
        Ok(b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
    }
}

/// 检查偏移表：从 0 开始、单调递增、以总长结束，且每段都以 0 结尾
fn check_segments(off: &[u32], buf: &[u8]) -> io::Result<()> {
    if off.first() != Some(&0) || *off.last().unwrap() as usize != buf.len() {
        return Err(bad("偏移表"));
    }
    let ok = off.par_windows(2).all(|w| w[1] > w[0] && buf[w[1] as usize - 1] == 0);
    if !ok {
        return Err(bad("偏移表"));
    }
    Ok(())
}

pub fn load(path: &Path) -> io::Result<Index> {
    let data = std::fs::read(path)?;
    let mut r = Reader { d: &data, p: 0 };
    if r.bytes(8)? != MAGIC || r.u32()? != VERSION {
        return Err(bad("版本不符"));
    }
    let n = r.u64()? as usize;
    let nb = r.u64()? as usize;
    let pm = r.u64()? as usize;
    let pb = r.u64()? as usize;
    if n >= u32::MAX as usize || nb >= u32::MAX as usize || pb >= u32::MAX as usize {
        return Err(bad("数量"));
    }
    let mut ix = Index::default();
    ix.off = r.vec_u32(n + 1)?;
    ix.parent = r.vec_u32(n)?;
    ix.flags = r.bytes(n)?.to_vec();
    ix.size = r.vec_u64(n)?;
    ix.mtime = r.vec_u64(n)?;
    ix.names = r.bytes(nb)?.to_vec();
    ix.folded = r.bytes(nb)?.to_vec();
    ix.py_off = r.vec_u32(pm + 1)?;
    ix.py_owner = r.vec_u32(pm)?;
    ix.py_buf = r.bytes(pb)?.to_vec();

    // 校验，保证后续按偏移切片取字符串是安全的
    check_segments(&ix.off, &ix.names)?;
    check_segments(&ix.off, &ix.folded)?;
    check_segments(&ix.py_off, &ix.py_buf)?;
    if std::str::from_utf8(&ix.names).is_err() || std::str::from_utf8(&ix.folded).is_err() || std::str::from_utf8(&ix.py_buf).is_err() {
        return Err(bad("文件名编码"));
    }
    if !ix.parent.par_iter().all(|&p| p == NONE || (p as usize) < n) {
        return Err(bad("父目录"));
    }
    if !ix.py_owner.windows(2).all(|w| w[0] < w[1]) || ix.py_owner.last().is_some_and(|&x| x as usize >= n) {
        return Err(bad("拼音表"));
    }
    ix.rebuild_map();
    Ok(ix)
}
