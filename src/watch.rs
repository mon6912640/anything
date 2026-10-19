//! 实时监视：每个根目录一个线程，ReadDirectoryChangesExW（递归、扩展信息）。
//! 扩展信息里直接带有属性、大小、修改时间，不需要再去查询文件。
//! 不支持扩展信息的文件系统（FAT32 等）自动退回 ReadDirectoryChangesW。

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NOTIFY_ENUM_DIR, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_EXTENDED_INFORMATION, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadDirectoryChangesExW, ReadDirectoryChangesW, ReadDirectoryNotifyExtendedInformation,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};

use crate::scan::Raw;
use crate::util::long_path;

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub attr: u32,
    pub tag: u32,
    pub size: u64,
    pub mtime: u64,
}

impl From<Raw> for Info {
    fn from(r: Raw) -> Info {
        Info { attr: r.attr, tag: r.tag, size: r.size, mtime: r.mtime }
    }
}

/// 文件系统事件。rel 为相对根目录的路径。
#[derive(Clone, Debug)]
pub enum Ev {
    Add { root: Arc<str>, rel: String, info: Option<Info> },
    Del { root: Arc<str>, rel: String },
    Mod { root: Arc<str>, rel: String, info: Option<Info> },
    Ren { root: Arc<str>, old: String, new: String, info: Option<Info> },
    /// 缓冲区溢出，丢了事件，需要重扫
    Overflow { root: Arc<str> },
}

impl Ev {
    pub fn root(&self) -> &str {
        match self {
            Ev::Add { root, .. } | Ev::Del { root, .. } | Ev::Mod { root, .. } | Ev::Ren { root, .. } | Ev::Overflow { root } => root,
        }
    }
}

#[derive(Clone, Copy)]
struct SendHandle(isize);
unsafe impl Send for SendHandle {}

pub struct Watchers {
    stop: SendHandle,
    threads: Vec<JoinHandle<()>>,
}

impl Watchers {
    pub fn start(roots: &[String], tx: Sender<Ev>) -> Watchers {
        let stop = unsafe { CreateEventW(null(), 1, 0, null()) };
        let stop = SendHandle(stop as isize);
        let threads = roots
            .iter()
            .map(|r| {
                let root: Arc<str> = Arc::from(r.as_str());
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name(format!("anything-watch-{r}"))
                    .spawn(move || watch_root(root, tx, stop))
                    .expect("创建监视线程失败")
            })
            .collect();
        Watchers { stop, threads }
    }

    pub fn stop(self) {
        unsafe { SetEvent(self.stop.0 as HANDLE) };
        for t in self.threads {
            let _ = t.join();
        }
        unsafe { CloseHandle(self.stop.0 as HANDLE) };
    }
}

fn name_from(ptr: *const u16, bytes: u32) -> String {
    let s = unsafe { std::slice::from_raw_parts(ptr, bytes as usize / 2) };
    String::from_utf16_lossy(s)
}

fn watch_root(root: Arc<str>, tx: Sender<Ev>, stop: SendHandle) {
    let w = long_path(&root);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
    let mut buf = vec![0u64; 512 * 1024 / 8];
    let buf_bytes = (buf.len() * 8) as u32;
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE;
    let mut extended = true;
    let mut pending_old: Option<String> = None;
    loop {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev;
        unsafe { ResetEvent(ev) };
        let ok = unsafe {
            if extended {
                ReadDirectoryChangesExW(h, buf.as_mut_ptr() as *mut c_void, buf_bytes, 1, filter, null_mut(), &mut ov, None, ReadDirectoryNotifyExtendedInformation)
            } else {
                ReadDirectoryChangesW(h, buf.as_mut_ptr() as *mut c_void, buf_bytes, 1, filter, null_mut(), &mut ov, None)
            }
        };
        if ok == 0 {
            if extended {
                extended = false;
                continue;
            }
            break;
        }
        let handles = [ev, stop.0 as HANDLE];
        let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if r != WAIT_OBJECT_0 {
            unsafe {
                CancelIoEx(h, &ov);
                let mut b = 0u32;
                GetOverlappedResult(h, &ov, &mut b, 1);
            }
            break;
        }
        let mut bytes = 0u32;
        if unsafe { GetOverlappedResult(h, &ov, &mut bytes, 0) } == 0 {
            if unsafe { GetLastError() } == ERROR_NOTIFY_ENUM_DIR {
                let _ = tx.send(Ev::Overflow { root: root.clone() });
                continue;
            }
            break; // 根目录不可用了（如移动硬盘拔出）
        }
        if bytes == 0 {
            let _ = tx.send(Ev::Overflow { root: root.clone() });
            continue;
        }
        let base = buf.as_ptr() as *const u8;
        let mut p = 0usize;
        loop {
            let (next, action, name, info) = unsafe {
                if extended {
                    let fi = base.add(p) as *const FILE_NOTIFY_EXTENDED_INFORMATION;
                    let name = name_from(std::ptr::addr_of!((*fi).FileName) as *const u16, (*fi).FileNameLength);
                    let info = Info {
                        attr: (*fi).FileAttributes,
                        tag: (*fi).Anonymous.ReparsePointTag,
                        size: (*fi).FileSize as u64,
                        mtime: (*fi).LastModificationTime as u64,
                    };
                    ((*fi).NextEntryOffset, (*fi).Action, name, Some(info))
                } else {
                    let fi = base.add(p) as *const FILE_NOTIFY_INFORMATION;
                    let name = name_from(std::ptr::addr_of!((*fi).FileName) as *const u16, (*fi).FileNameLength);
                    ((*fi).NextEntryOffset, (*fi).Action, name, None)
                }
            };
            let root = root.clone();
            let ev = match action {
                FILE_ACTION_ADDED => Some(Ev::Add { root, rel: name, info }),
                FILE_ACTION_REMOVED => Some(Ev::Del { root, rel: name }),
                FILE_ACTION_MODIFIED => Some(Ev::Mod { root, rel: name, info }),
                FILE_ACTION_RENAMED_OLD_NAME => {
                    pending_old = Some(name);
                    None
                }
                FILE_ACTION_RENAMED_NEW_NAME => match pending_old.take() {
                    Some(old) => Some(Ev::Ren { root, old, new: name, info }),
                    None => Some(Ev::Add { root, rel: name, info }),
                },
                _ => None,
            };
            if let Some(e) = ev {
                if tx.send(e).is_err() {
                    break;
                }
            }
            if next == 0 {
                break;
            }
            p += next as usize;
        }
    }
    unsafe {
        CloseHandle(ev);
        CloseHandle(h);
    }
}
