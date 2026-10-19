//! 命名管道 IPC：anything 主程序作为服务端，ae.exe 等第三方程序作为客户端。
//! 管道名 \\.\pipe\anything-<用户名>，只接受本机连接，不需要管理员权限。
//!
//! 请求（UTF-8，每行 "键\t值"，空行结束）：
//!   cmd    search（默认）| status | rescan
//!   q      搜索词
//!   sort   name | path | size | date | none
//!   desc   0 | 1
//!   offset 跳过前 N 条
//!   max    最多返回 N 条（0 = 只要总数）
//! 响应：
//!   OK\t<总数>\t<耗时毫秒>\n  然后每条结果一行：<标志>\t<大小>\t<修改时间FILETIME>\t<完整路径>\n
//!   标志 d = 文件夹, f = 文件
//!   出错：ERR\t<信息>\n

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};

use crate::engine::Engine;
use crate::index::Index;
use crate::query::SortCol;
use crate::util::{group_digits, pipe_name, wide};

pub struct Pipe {
    h: HANDLE,
    server: bool,
}

unsafe impl Send for Pipe {}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(1 << 20) as u32;
        if unsafe { ReadFile(self.h, buf.as_mut_ptr(), len, &mut n, null_mut()) } == 0 {
            // 对方关闭管道视为读到结尾
            return Ok(0);
        }
        Ok(n as usize)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n = 0u32;
        let len = buf.len().min(1 << 20) as u32;
        if unsafe { WriteFile(self.h, buf.as_ptr(), len, &mut n, null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            if self.server {
                FlushFileBuffers(self.h);
                DisconnectNamedPipe(self.h);
            }
            CloseHandle(self.h);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub cmd: String,
    pub q: String,
    pub sort: SortCol,
    pub desc: bool,
    pub offset: usize,
    pub max: usize,
}

impl Default for Request {
    fn default() -> Self {
        Request { cmd: "search".into(), q: String::new(), sort: SortCol::Name, desc: false, offset: 0, max: usize::MAX }
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

impl Request {
    pub fn encode(&self) -> String {
        let sort = match self.sort {
            SortCol::Name => "name",
            SortCol::Path => "path",
            SortCol::Size => "size",
            SortCol::Date => "date",
            SortCol::None => "none",
        };
        format!(
            "cmd\t{}\nq\t{}\nsort\t{}\ndesc\t{}\noffset\t{}\nmax\t{}\n\n",
            self.cmd,
            clean(&self.q),
            sort,
            self.desc as u8,
            self.offset,
            self.max
        )
    }

    fn decode<R: BufRead>(r: &mut R) -> io::Result<Request> {
        let mut req = Request::default();
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                break;
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                break;
            }
            let (k, v) = l.split_once('\t').unwrap_or((l, ""));
            match k {
                "cmd" => req.cmd = v.to_string(),
                "q" => req.q = v.to_string(),
                "sort" => req.sort = SortCol::parse(v).unwrap_or(SortCol::Name),
                "desc" => req.desc = v == "1",
                "offset" => req.offset = v.parse().unwrap_or(0),
                "max" => req.max = v.parse().unwrap_or(usize::MAX),
                _ => {}
            }
        }
        Ok(req)
    }
}

/// 把结果写成协议格式（服务端和离线模式共用）
pub fn write_records<W: Write>(w: &mut W, ix: &Index, ids: &[u32], offset: usize, max: usize) -> io::Result<()> {
    let end = offset.saturating_add(max).min(ids.len());
    if offset >= end {
        return Ok(());
    }
    for &id in &ids[offset..end] {
        let i = id as usize;
        let kind = if ix.is_dir(id) { 'd' } else { 'f' };
        writeln!(w, "{}\t{}\t{}\t{}", kind, ix.size[i], ix.mtime[i], ix.full_path(id))?;
    }
    Ok(())
}

/// 启动服务端（后台线程）
pub fn serve(engine: Arc<Engine>) {
    std::thread::Builder::new()
        .name("anything-ipc".into())
        .spawn(move || {
            let name = wide(&pipe_name());
            loop {
                let h = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                        PIPE_UNLIMITED_INSTANCES,
                        1 << 16,
                        1 << 16,
                        0,
                        null(),
                    )
                };
                if h == INVALID_HANDLE_VALUE {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                let ok = unsafe { ConnectNamedPipe(h, null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                let pipe = Pipe { h, server: true };
                if !ok {
                    continue;
                }
                let e = engine.clone();
                std::thread::spawn(move || {
                    let _ = handle_client(&e, pipe);
                });
            }
        })
        .unwrap();
}

fn handle_client(engine: &Arc<Engine>, pipe: Pipe) -> io::Result<()> {
    let mut r = BufReader::new(pipe);
    let req = Request::decode(&mut r)?;
    let mut w = BufWriter::with_capacity(1 << 16, r.into_inner());
    match req.cmd.as_str() {
        "status" => {
            let ix = engine.index.read().unwrap();
            writeln!(
                w,
                "OK\t{}\t{}\t{}\t{}",
                ix.alive(),
                engine.scanning.load(SeqCst) as u8,
                engine.scan_progress.load(SeqCst),
                clean(&engine.status())
            )?;
        }
        "rescan" => {
            engine.rescan();
            writeln!(w, "OK\t0\t0")?;
        }
        _ => match engine.search(&req.q, req.sort, req.desc, &|| false) {
            Ok(Some(h)) => {
                writeln!(w, "OK\t{}\t{:.1}", h.ids.len(), h.millis)?;
                // 分块格式化：每块只短暂持有读锁，客户端读得慢也不会挡住实时更新
                let end = req.offset.saturating_add(req.max).min(h.ids.len());
                let mut pos = req.offset.min(end);
                let mut buf = Vec::with_capacity(1 << 20);
                while pos < end {
                    let stop = (pos + 20_000).min(end);
                    buf.clear();
                    {
                        let ix = engine.index.read().unwrap();
                        if engine.generation.load(SeqCst) != h.generation {
                            break; // 索引刚被整体替换，结果编号已失效
                        }
                        write_records(&mut buf, &ix, &h.ids[pos..stop], 0, usize::MAX)?;
                    }
                    w.write_all(&buf)?;
                    pos = stop;
                }
            }
            Ok(None) => writeln!(w, "ERR\t已取消")?,
            Err(e) => writeln!(w, "ERR\t{}", clean(&e))?,
        },
    }
    w.flush()
}

/// 客户端：连接正在运行的 anything。没有运行时返回 None。
pub fn connect() -> Option<Pipe> {
    let name = wide(&pipe_name());
    for _ in 0..20 {
        let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, null_mut()) };
        if h != INVALID_HANDLE_VALUE {
            return Some(Pipe { h, server: false });
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
            return None;
        }
        unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
    }
    None
}

/// 客户端发请求，返回读取响应的流
pub fn request(req: &Request) -> Option<io::Result<BufReader<Pipe>>> {
    let mut p = connect()?;
    Some(p.write_all(req.encode().as_bytes()).map(|_| BufReader::with_capacity(1 << 16, p)))
}

pub fn describe_status(line: &str) -> String {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() >= 5 && f[0] == "OK" {
        let n: u64 = f[1].parse().unwrap_or(0);
        let mut s = format!("已索引 {} 个对象", group_digits(n));
        if f[2] == "1" {
            s.push_str(&format!("，正在重扫（已发现 {}）", group_digits(f[3].parse().unwrap_or(0))));
        }
        if !f[4].is_empty() {
            s.push_str("\n");
            s.push_str(f[4]);
        }
        s
    } else {
        line.to_string()
    }
}
