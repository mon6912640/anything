//! 配置文件 %LOCALAPPDATA%\anything\settings.ini

use std::collections::HashSet;
use std::path::PathBuf;

use crate::util::{fold, wide};
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

const DRIVE_FIXED: u32 = 3;

const DEFAULT_INI: &str = "\u{feff}# anything 设置文件（修改后：菜单“文件 → 重新加载设置”或重启生效）
#
# roots: 要索引的目录，多个用 | 分隔；留空 = 所有本地固定磁盘
#   例: roots=C:\\|D:\\Work
roots=
#
# excludes: 不索引的目录，多个用 | 分隔
#   写完整路径（如 C:\\Windows\\WinSxS）只排除该目录；
#   只写文件夹名（如 node_modules）则任何位置的同名文件夹都排除
excludes=C:\\Windows\\WinSxS|$Recycle.Bin|System Volume Information
#
# hotkey: 全局呼出快捷键，例如 Ctrl+Alt+F、Alt+Space、Win+Shift+S、Ctrl+`
hotkey=Ctrl+Alt+F
#
# rescan_on_start: 启动时后台重扫哪些盘，用来发现程序没运行期间的文件变化
#   ssd = 只重扫固态硬盘（默认；机械硬盘没有管理员权限时扫描很慢，靠实时监视保持更新，需要时按 F5）
#   all = 所有盘    0 = 不重扫
rescan_on_start=ssd
#
# threads: 固态硬盘上的扫描线程数
threads=12
#
# hdd_threads: 机械硬盘上的扫描线程数（线程太多磁头来回跳反而慢）
hdd_threads=4
#
# pinyin: 是否支持拼音首字母搜索（1/0），如输入 xmwj 可搜到“项目文件”
pinyin=1
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rescan {
    Off,
    Ssd,
    All,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub roots: Vec<String>,
    /// 折叠后的完整路径，不带结尾反斜杠
    pub ex_paths: Vec<Vec<u8>>,
    /// 折叠后的文件夹名
    pub ex_names: HashSet<Vec<u8>>,
    pub hotkey: String,
    pub rescan_on_start: Rescan,
    pub threads: usize,
    pub hdd_threads: usize,
    pub pinyin: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec![],
            ex_paths: vec![],
            ex_names: HashSet::new(),
            hotkey: "Ctrl+Alt+F".into(),
            rescan_on_start: Rescan::Ssd,
            threads: 12,
            hdd_threads: 4,
            pinyin: true,
        }
    }
}

/// 数据目录（设置和索引）：
/// 1. 环境变量 ANYTHING_DATA_DIR；
/// 2. 便携模式：exe 同目录下有 settings.ini 时就用 exe 所在目录（适合放 U 盘）；
/// 3. 默认 %LOCALAPPDATA%\anything
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ANYTHING_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        if dir.join("settings.ini").is_file() {
            return dir;
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("anything")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.ini")
}

pub fn index_path() -> PathBuf {
    data_dir().join("index.bin")
}

fn norm_root(p: &str) -> String {
    let p = p.trim().trim_end_matches('\\');
    p.to_string()
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        let path = settings_path();
        let _ = std::fs::create_dir_all(data_dir());
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_INI);
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_INI.to_string());
        c.parse(&text);
        c
    }

    fn parse(&mut self, text: &str) {
        for raw in text.lines() {
            let line = raw.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            let items = || v.split('|').map(str::trim).filter(|s| !s.is_empty());
            match k.as_str() {
                "roots" => self.roots = items().map(norm_root).filter(|s| !s.is_empty()).collect(),
                "excludes" => {
                    self.ex_paths.clear();
                    self.ex_names.clear();
                    for it in items() {
                        let it = it.trim_end_matches('\\');
                        if it.contains('\\') || it.ends_with(':') {
                            self.ex_paths.push(fold(it));
                        } else {
                            self.ex_names.insert(fold(it));
                        }
                    }
                }
                "hotkey" => self.hotkey = v.to_string(),
                "rescan_on_start" => {
                    self.rescan_on_start = match v.to_ascii_lowercase().as_str() {
                        "0" | "off" | "no" | "false" => Rescan::Off,
                        "1" | "all" | "yes" | "true" => Rescan::All,
                        _ => Rescan::Ssd,
                    }
                }
                "threads" => {
                    if let Ok(t) = v.parse::<usize>() {
                        self.threads = t.clamp(1, 64);
                    }
                }
                "hdd_threads" => {
                    if let Ok(t) = v.parse::<usize>() {
                        self.hdd_threads = t.clamp(1, 64);
                    }
                }
                "pinyin" => self.pinyin = v != "0",
                _ => {}
            }
        }
    }

    /// 实际要索引的根目录（"C:" 这种形式，不带结尾反斜杠）
    pub fn effective_roots(&self) -> Vec<String> {
        let roots: Vec<String> = if !self.roots.is_empty() {
            self.roots.clone()
        } else {
            let mask = unsafe { GetLogicalDrives() };
            (0..26u8)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| format!("{}:", (b'A' + i) as char))
                .filter(|d| unsafe { GetDriveTypeW(wide(&format!("{}\\", d)).as_ptr()) } == DRIVE_FIXED)
                .collect()
        };
        roots
            .into_iter()
            .filter(|r| std::path::Path::new(&format!("{}\\", r)).is_dir())
            .collect()
    }

    /// 扫描时判断子目录是否排除（父目录已检查过，只需看自身）
    pub fn is_excluded_dir(&self, full_path: &str, folded_name: &[u8]) -> bool {
        if self.ex_names.contains(folded_name) {
            return true;
        }
        if self.ex_paths.is_empty() {
            return false;
        }
        let f = fold(full_path);
        self.ex_paths.iter().any(|ex| f == *ex)
    }

    /// 实时更新时判断某个路径是否落在排除范围内（检查所有层级）
    pub fn is_excluded_path(&self, full_path: &str) -> bool {
        let f = fold(full_path);
        if self.ex_paths.iter().any(|ex| f.starts_with(ex) && (f.len() == ex.len() || f[ex.len()] == b'\\')) {
            return true;
        }
        if self.ex_names.is_empty() {
            return false;
        }
        f.split(|&b| b == b'\\').any(|comp| self.ex_names.contains(comp))
    }
}
