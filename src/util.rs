//! 通用小工具：宽字符串、大小写折叠、拼音首字母、哈希、时间换算。

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    TzSpecificLocalTimeToSystemTime,
};

/// 转成以 0 结尾的 UTF-16，用于调用 Win32 API。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把带 0 结尾（或不带）的 UTF-16 缓冲区转回字符串。
pub fn from_wide(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// 长度不变的小写折叠：只在小写形式的 UTF-8 字节数与原字符相同时才替换，
/// 这样折叠后的缓冲区与原文件名逐字节对齐，可以共用一套偏移表。
pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase() as u8);
            continue;
        }
        let mut lower = ch.to_lowercase();
        let first = lower.next().unwrap_or(ch);
        let c = if lower.next().is_none() && first.len_utf8() == ch.len_utf8() { first } else { ch };
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

pub fn fold(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len());
    fold_into(s, &mut v);
    v
}

/// 拼音首字母串：汉字换成拼音首字母，其余字符按折叠后原样保留。
/// 例如 "项目文件A.txt" -> "xmwja.txt"。不含汉字时返回 None（不占内存）。
pub fn pinyin_initials(name: &str) -> Option<Vec<u8>> {
    use pinyin::ToPinyin;
    if name.is_ascii() {
        return None;
    }
    let mut out = Vec::with_capacity(name.len());
    let mut any = false;
    let mut tmp = [0u8; 4];
    for ch in name.chars() {
        if let Some(p) = ch.to_pinyin() {
            out.extend_from_slice(p.first_letter().as_bytes());
            any = true;
        } else {
            fold_into(ch.encode_utf8(&mut tmp), &mut out);
        }
    }
    if any { Some(out) } else { None }
}

/// (父目录编号, 折叠后的名字) 的 64 位哈希，用作路径查找表的键。
pub fn key_hash(parent: u32, folded: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (parent as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for &b in folded {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}


/// FILETIME(UTC, 100ns) -> 本地时间 SYSTEMTIME
pub fn filetime_to_local(ft: u64) -> Option<SYSTEMTIME> {
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 {
            return None;
        }
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
        Some(local)
    }
}

/// 格式化为 "2026/10/19 20:15"
pub fn format_filetime(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    match filetime_to_local(ft) {
        Some(t) => format!("{:04}/{:02}/{:02} {:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute),
        None => String::new(),
    }
}

/// 本地时间的年月日时分 -> FILETIME(UTC)
pub fn local_to_filetime(y: u16, mo: u16, d: u16, h: u16, mi: u16) -> Option<u64> {
    unsafe {
        let mut local: SYSTEMTIME = std::mem::zeroed();
        local.wYear = y;
        local.wMonth = mo;
        local.wDay = d;
        local.wHour = h;
        local.wMinute = mi;
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        if TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) == 0 {
            return None;
        }
        let mut f = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        if SystemTimeToFileTime(&utc, &mut f) == 0 {
            return None;
        }
        Some(((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64)
    }
}

/// 今天本地 0 点对应的 FILETIME
pub fn today_start() -> u64 {
    unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        local_to_filetime(t.wYear, t.wMonth, t.wDay, 0, 0).unwrap_or(0)
    }
}

pub const FT_DAY: u64 = 864_000_000_000;

/// 文件大小显示，和资源管理器一致：按 KB 向上取整
pub fn format_size_kb(size: u64) -> String {
    let kb = size.div_ceil(1024);
    let s = kb.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.push_str(" KB");
    out
}

/// 千分位格式化
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Win32 长路径形式：C:\a -> \\?\C:\a，\\srv\share -> \\?\UNC\srv\share；盘符根目录补上反斜杠。
pub fn long_path(p: &str) -> Vec<u16> {
    let mut s = if p.starts_with(r"\\?\") {
        p.to_string()
    } else if let Some(rest) = p.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{}", rest)
    } else {
        format!(r"\\?\{}", p)
    };
    if s.ends_with(':') {
        s.push('\\');
    }
    wide(&s)
}

/// 拆成 (所在目录, 名字)
pub fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('\\') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// 管道名：每个用户一个，互不干扰
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let user: String = user.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\anything-{}", user)
}
