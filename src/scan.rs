//! 目录扫描。没有管理员权限读不了 MFT，只能逐层遍历目录：
//! 用 GetFileInformationByHandleEx(FileFullDirectoryInfo) 一次取 64KB 目录项，
//! rayon 工作窃取线程池并行遍历。

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileFullDirectoryInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    GetFileInformationByHandleEx, OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
    StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};

use crate::config::Config;
use crate::index::{F_DIR, F_NOREC, Index, NONE};
use crate::util::{fold, from_wide, long_path, pinyin_initials, wide};

const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const TAG_SYMLINK: u32 = 0xA000_000C;

pub struct Raw {
    pub name: String,
    pub attr: u32,
    pub tag: u32,
    pub size: u64,
    pub mtime: u64,
}

impl Raw {
    pub fn is_dir(&self) -> bool {
        self.attr & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// 目录是否应当递归进入：跳过符号链接和挂载点（防止循环），
/// OneDrive 等云文件占位符仍然进入。
pub fn should_recurse(attr: u32, tag: u32) -> bool {
    attr & FILE_ATTRIBUTE_DIRECTORY != 0
        && !(attr & FILE_ATTRIBUTE_REPARSE_POINT != 0 && (tag == TAG_MOUNT_POINT || tag == TAG_SYMLINK))
}

thread_local! {
    static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; 8192]); // 64KB，8 字节对齐
}

/// 列出一个目录的直接子项。打不开（无权限、已删除）返回 false。
pub fn enum_dir(path: &str, out: &mut Vec<Raw>) -> bool {
    let w = long_path(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    BUF.with(|b| {
        let mut b = b.borrow_mut();
        let bytes = (b.len() * 8) as u32;
        loop {
            let ok = unsafe { GetFileInformationByHandleEx(h, FileFullDirectoryInfo, b.as_mut_ptr() as *mut c_void, bytes) };
            if ok == 0 {
                break; // ERROR_NO_MORE_FILES 或出错
            }
            let base = b.as_ptr() as *const u8;
            let mut p = 0usize;
            loop {
                unsafe {
                    let info = base.add(p) as *const FILE_FULL_DIR_INFO;
                    let nlen = (*info).FileNameLength as usize / 2;
                    let np = std::ptr::addr_of!((*info).FileName) as *const u16;
                    let name16 = std::slice::from_raw_parts(np, nlen);
                    let dot = (nlen == 1 && name16[0] == 46) || (nlen == 2 && name16[0] == 46 && name16[1] == 46);
                    if !dot {
                        out.push(Raw {
                            name: String::from_utf16_lossy(name16),
                            attr: (*info).FileAttributes,
                            tag: (*info).EaSize, // 重解析点时 EaSize 字段里是重解析标记
                            size: (*info).EndOfFile as u64,
                            mtime: (*info).LastWriteTime as u64,
                        });
                    }
                    let next = (*info).NextEntryOffset;
                    if next == 0 {
                        break;
                    }
                    p += next as usize;
                }
            }
        }
    });
    unsafe { CloseHandle(h) };
    true
}

/// 查询单个文件/目录的信息
pub fn stat(path: &str) -> Option<Raw> {
    let w = long_path(path);
    unsafe {
        let mut fd: WIN32_FIND_DATAW = std::mem::zeroed();
        let h = FindFirstFileExW(w.as_ptr(), FindExInfoBasic, &mut fd as *mut _ as *mut c_void, FindExSearchNameMatch, null(), 0);
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        FindClose(h);
        Some(Raw {
            name: from_wide(&fd.cFileName),
            attr: fd.dwFileAttributes,
            tag: fd.dwReserved0,
            size: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
            mtime: ((fd.ftLastWriteTime.dwHighDateTime as u64) << 32) | fd.ftLastWriteTime.dwLowDateTime as u64,
        })
    }
}

pub fn join(dir: &str, name: &str) -> String {
    let mut s = String::with_capacity(dir.len() + name.len() + 1);
    s.push_str(dir);
    if !dir.ends_with('\\') {
        s.push('\\');
    }
    s.push_str(name);
    s
}

/// 在锁外准备好的条目（折叠名、拼音都提前算好，缩短持锁时间）
struct Prep {
    raw: Raw,
    folded: Vec<u8>,
    py: Option<Vec<u8>>,
    child: Option<String>,
}

fn prepare(dir_path: &str, raws: Vec<Raw>, cfg: &Config) -> Vec<Prep> {
    raws.into_iter()
        .map(|r| {
            let folded = fold(&r.name);
            let py = if cfg.pinyin { pinyin_initials(&r.name) } else { None };
            let mut child = None;
            if should_recurse(r.attr, r.tag) {
                let cp = join(dir_path, &r.name);
                if !cfg.is_excluded_dir(&cp, &folded) {
                    child = Some(cp);
                }
            }
            Prep { raw: r, folded, py, child }
        })
        .collect()
}

fn entry_flags(p: &Prep) -> u8 {
    if p.raw.is_dir() {
        if p.child.is_some() { F_DIR } else { F_DIR | F_NOREC }
    } else {
        0
    }
}

struct Ctx<'a> {
    idx: Mutex<Index>,
    cfg: &'a Config,
    progress: &'a AtomicUsize,
    cancel: &'a AtomicBool,
}

/// 判断根目录所在磁盘是否为固态硬盘（查询“寻道延迟”属性，无需管理员权限）。
/// 查询失败（网络盘、虚拟盘等）返回 None。
pub fn is_ssd(root: &str) -> Option<bool> {
    let b = root.as_bytes();
    if b.len() < 2 || b[1] != b':' {
        return None;
    }
    let dev = wide(&format!(r"\\.\{}:", b[0] as char));
    unsafe {
        let h = CreateFileW(dev.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut());
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut q: STORAGE_PROPERTY_QUERY = std::mem::zeroed();
        q.PropertyId = StorageDeviceSeekPenaltyProperty;
        q.QueryType = PropertyStandardQuery;
        let mut d: DEVICE_SEEK_PENALTY_DESCRIPTOR = std::mem::zeroed();
        let mut got = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const _ as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut d as *mut _ as *mut c_void,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut got,
            null_mut(),
        );
        CloseHandle(h);
        if ok == 0 || got < std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32 {
            return None;
        }
        Some(!d.IncursSeekPenalty)
    }
}

/// 该根目录用几个扫描线程：机械硬盘少用线程，避免磁头来回跳
pub fn threads_for(root: &str, cfg: &Config) -> usize {
    match is_ssd(root) {
        Some(false) => cfg.hdd_threads,
        _ => cfg.threads,
    }
    .max(1)
}

/// 扫描一个根目录，返回只含这个根的索引
pub fn scan_root(root: &str, cfg: &Config, threads: usize, progress: &AtomicUsize, cancel: &AtomicBool, cap_hint: (usize, usize)) -> Index {
    let ctx = Ctx { idx: Mutex::new(Index::with_capacity(cap_hint.0, cap_hint.1)), cfg, progress, cancel };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("anything-scan-{i}"))
        .start_handler(|_| unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        })
        .build()
        .expect("创建扫描线程池失败");
    let rid = ctx.idx.lock().unwrap().push_nomap(root, &fold(root), None, NONE, F_DIR, 0, 0);
    pool.scope(|s| {
        let ctx = &ctx;
        let path = root.to_string();
        s.spawn(move |s| scan_dir(s, ctx, rid, path));
    });
    let mut ix = ctx.idx.into_inner().unwrap();
    ix.shrink();
    // 扫描时不维护查找表（避免持锁扩容），扫完一次性并行重建
    ix.rebuild_map();
    ix
}

/// 并行扫描多个根目录并合并（各盘各用自己的线程池）
pub fn scan_roots(roots: &[String], cfg: &Config, progress: &AtomicUsize, cancel: &AtomicBool) -> Index {
    let parts: Vec<Index> = std::thread::scope(|sc| {
        let hs: Vec<_> = roots.iter().map(|r| sc.spawn(move || scan_root(r, cfg, threads_for(r, cfg), progress, cancel, (0, 0)))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut it = parts.into_iter();
    let mut ix = it.next().unwrap_or_default();
    for p in it {
        ix.append(&p);
    }
    if ix.off.is_empty() {
        ix = Index::new();
    }
    ix.rebuild_map();
    ix
}

fn scan_dir<'s, 'a: 's>(s: &rayon::Scope<'s>, ctx: &'s Ctx<'a>, dir: u32, path: String) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return;
    }
    let mut raws = Vec::new();
    if !enum_dir(&path, &mut raws) {
        ctx.idx.lock().unwrap().flags[dir as usize] |= F_NOREC;
        return;
    }
    let n = raws.len();
    let mut preps = prepare(&path, raws, ctx.cfg);
    let mut subs = Vec::new();
    {
        let mut ix = ctx.idx.lock().unwrap();
        for p in &mut preps {
            let fl = entry_flags(p);
            let size = if p.raw.is_dir() { 0 } else { p.raw.size };
            let id = ix.push_nomap(&p.raw.name, &p.folded, p.py.as_deref(), dir, fl, size, p.raw.mtime);
            if let Some(c) = p.child.take() {
                subs.push((id, c));
            }
        }
    }
    ctx.progress.fetch_add(n, Ordering::Relaxed);
    for (id, c) in subs {
        s.spawn(move |s| scan_dir(s, ctx, id, c));
    }
}

/// 实时更新时新出现的目录（例如拷进来的文件夹）：在锁外扫描整棵子树
pub struct SubEntry {
    /// 在本次结果中的父条目序号，NONE 表示挂在子树根下
    pub parent_local: u32,
    pub name: String,
    pub folded: Vec<u8>,
    pub py: Option<Vec<u8>>,
    pub flags: u8,
    pub size: u64,
    pub mtime: u64,
}

pub fn scan_subtree(path: &str, cfg: &Config) -> Vec<SubEntry> {
    let mut out: Vec<SubEntry> = Vec::new();
    let mut stack: Vec<(u32, String)> = vec![(NONE, path.to_string())];
    while let Some((pl, p)) = stack.pop() {
        let mut raws = Vec::new();
        if !enum_dir(&p, &mut raws) {
            continue;
        }
        for mut pr in prepare(&p, raws, cfg) {
            let id = out.len() as u32;
            let flags = entry_flags(&pr);
            if let Some(c) = pr.child.take() {
                stack.push((id, c));
            }
            out.push(SubEntry {
                parent_local: pl,
                size: if pr.raw.is_dir() { 0 } else { pr.raw.size },
                mtime: pr.raw.mtime,
                name: pr.raw.name,
                folded: pr.folded,
                py: pr.py,
                flags,
            });
        }
    }
    out
}

/// 把 scan_subtree 的结果挂到 root_id 下（已存在的条目只更新属性）
pub fn insert_subtree(ix: &mut Index, root_id: u32, subs: Vec<SubEntry>) {
    let mut ids: Vec<u32> = Vec::with_capacity(subs.len());
    for e in subs {
        let parent = if e.parent_local == NONE { root_id } else { ids[e.parent_local as usize] };
        let id = match ix.lookup(parent, &e.folded) {
            Some(x) => {
                ix.size[x as usize] = e.size;
                ix.mtime[x as usize] = e.mtime;
                x
            }
            None => ix.push(&e.name, &e.folded, e.py.as_deref(), parent, e.flags, e.size, e.mtime),
        };
        ids.push(id);
    }
}
