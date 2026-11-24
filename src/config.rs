//! 配置默认值、文本存取和便携数据目录。
//!
//! 程序启动只读取配置，不写用户文件；设置窗口保存时才生成 settings.ini。
//! 旧配置缺少的新选项始终使用这里的内置默认值，无需升级时合并文本。

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::util::{fold, wide};
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

const DRIVE_FIXED: u32 = 3;
const SETTINGS_FILE: &str = "settings.ini";
const PORTABLE_FLAG: &str = "portable.flag";
const DEFAULT_EXCLUDES: &[&str] = &[r"C:\Windows\WinSxS", "$Recycle.Bin", "System Volume Information"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rescan {
    Off,
    Ssd,
    All,
}

impl Rescan {
    pub fn as_str(self) -> &'static str {
        match self {
            Rescan::Off => "0",
            Rescan::Ssd => "ssd",
            Rescan::All => "all",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub roots: Vec<String>,
    /// 用户输入的排除规则，保留原始大小写供设置界面编辑和保存。
    pub excludes: Vec<String>,
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
    fn default() -> Config {
        let mut c = Config {
            roots: vec![],
            excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
            ex_paths: vec![],
            ex_names: HashSet::new(),
            hotkey: "Ctrl+Alt+F".into(),
            rescan_on_start: Rescan::Ssd,
            threads: 12,
            hdd_threads: 4,
            pinyin: true,
        };
        c.rebuild_excludes();
        c
    }
}

/// 数据目录（设置和索引）：
/// 1. 环境变量 ANYTHING_DATA_DIR；
/// 2. 便携模式：exe 同目录有 settings.ini（旧包）或 portable.flag（新包）；
/// 3. 默认 %LOCALAPPDATA%\anything。
fn is_portable_dir(dir: &Path) -> bool {
    dir.join(SETTINGS_FILE).is_file() || dir.join(PORTABLE_FLAG).is_file()
}

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ANYTHING_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        if is_portable_dir(&dir) {
            return dir;
        }
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir());
    base.join("anything")
}

pub fn settings_path() -> PathBuf {
    data_dir().join(SETTINGS_FILE)
}

pub fn index_path() -> PathBuf {
    data_dir().join("index.bin")
}

fn norm_root(p: &str) -> String {
    p.trim().trim_end_matches('\\').to_string()
}

impl Config {
    pub fn load() -> Config {
        Self::load_from(&data_dir())
    }

    fn load_from(dir: &Path) -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(dir.join(SETTINGS_FILE)) else { return c };
        c.parse(&text);
        c
    }

    pub fn set_roots(&mut self, roots: Vec<String>) {
        self.roots = roots.iter().map(|s| norm_root(s)).filter(|s| !s.is_empty()).collect();
    }

    pub fn set_excludes(&mut self, excludes: Vec<String>) {
        self.excludes = excludes;
        self.rebuild_excludes();
    }

    fn rebuild_excludes(&mut self) {
        self.excludes = self
            .excludes
            .iter()
            .map(|s| s.trim().trim_end_matches(['\\', '/']).to_string())
            .filter(|s| !s.is_empty())
            .collect();
        self.ex_paths.clear();
        self.ex_names.clear();
        for it in &self.excludes {
            if it.contains('\\') || it.ends_with(':') {
                self.ex_paths.push(fold(it));
            } else {
                self.ex_names.insert(fold(it));
            }
        }
    }

    /// 简洁、稳定地保存当前设置；注释和新增选项的说明由设置界面承担。
    pub fn to_ini(&self) -> String {
        format!(
            "roots={}\nexcludes={}\nhotkey={}\nrescan_on_start={}\nthreads={}\nhdd_threads={}\npinyin={}\n",
            self.roots.join("|"),
            self.excludes.join("|"),
            self.hotkey,
            self.rescan_on_start.as_str(),
            self.threads,
            self.hdd_threads,
            if self.pinyin { 1 } else { 0 },
        )
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&data_dir())
    }

    fn save_to(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(SETTINGS_FILE);
        let tmp = dir.join(format!("settings.ini.{}.tmp", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(self.to_ini().as_bytes())?;
            f.sync_all()?;
        }
        if path.exists() {
            std::fs::copy(&path, dir.join("settings.ini.bak"))?;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
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
                "roots" => self.set_roots(items().map(str::to_string).collect()),
                "excludes" => self.set_excludes(items().map(str::to_string).collect()),
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
        roots.into_iter().filter(|r| std::path::Path::new(&format!("{}\\", r)).is_dir()).collect()
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

    /// 实时监视目录变化时，排除整个目录及其子项
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("anything_config_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_uses_defaults_without_creating_user_config() {
        let dir = test_dir("missing");
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg.hotkey, "Ctrl+Alt+F");
        assert_eq!(cfg.excludes, DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(!dir.join(SETTINGS_FILE).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_partial_config_keeps_new_defaults_and_original_excludes() {
        let dir = test_dir("partial");
        let original = "roots=D:\\Work\\\nexcludes=Node_Modules|C:\\Temp\\\nthreads=8\n";
        std::fs::write(dir.join(SETTINGS_FILE), original).unwrap();
        let cfg = Config::load_from(&dir);
        assert_eq!(cfg.roots, vec!["D:\\Work"]);
        assert_eq!(cfg.excludes, vec!["Node_Modules", "C:\\Temp"]);
        assert_eq!(cfg.hotkey, "Ctrl+Alt+F");
        assert_eq!(cfg.hdd_threads, 4);
        assert_eq!(cfg.threads, 8);
        assert_eq!(std::fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap(), original);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_roundtrip_and_backup_preserve_settings() {
        let dir = test_dir("save");
        let mut cfg = Config::default();
        cfg.set_roots(vec!["C:\\".into(), "D:\\Work\\".into()]);
        cfg.set_excludes(vec!["Node_Modules".into(), "C:\\Temp\\".into()]);
        cfg.hotkey = "Ctrl+Alt+Space".into();
        cfg.rescan_on_start = Rescan::Off;
        cfg.threads = 7;
        cfg.hdd_threads = 2;
        cfg.pinyin = false;
        cfg.save_to(&dir).unwrap();
        let first = std::fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap();

        let loaded = Config::load_from(&dir);
        assert_eq!(loaded.roots, cfg.roots);
        assert_eq!(loaded.excludes, cfg.excludes);
        assert_eq!(loaded.hotkey, cfg.hotkey);
        assert_eq!(loaded.rescan_on_start, Rescan::Off);
        assert_eq!(loaded.threads, 7);
        assert_eq!(loaded.hdd_threads, 2);
        assert!(!loaded.pinyin);

        let mut changed = loaded;
        changed.threads = 9;
        changed.save_to(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("settings.ini.bak")).unwrap(), first);
        assert_eq!(Config::load_from(&dir).threads, 9);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn portable_marker_and_legacy_settings_both_enable_portable_mode() {
        let marker = test_dir("marker");
        assert!(!is_portable_dir(&marker));
        std::fs::write(marker.join(PORTABLE_FLAG), []).unwrap();
        assert!(is_portable_dir(&marker));
        std::fs::remove_file(marker.join(PORTABLE_FLAG)).unwrap();
        std::fs::write(marker.join(SETTINGS_FILE), "pinyin=0\n").unwrap();
        assert!(is_portable_dir(&marker));
        std::fs::remove_dir_all(&marker).unwrap();
    }
}
