//! 原生 Win32 设置窗口。只有用户点击“保存”才调用回调写入配置。

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};

use anything::config::{Config, Rescan};
use anything::util::wide;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, HBRUSH, HFONT, UpdateWindow};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const ROOTS: usize = 5001;
pub const EXCLUDES: usize = 5002;
pub const HOTKEY: usize = 5003;
pub const RESCAN: usize = 5004;
pub const THREADS: usize = 5005;
pub const HDD_THREADS: usize = 5006;
pub const PINYIN: usize = 5007;
pub const AUTOSTART: usize = 5008;
pub const CONTEXT_MENU: usize = 5009;
const BROWSE_ROOTS: usize = 5010;
const BROWSE_EXCLUDES: usize = 5011;
pub const SAVE: usize = 5012;
pub const CANCEL: usize = 5013;
pub const DEFAULTS: usize = 5014;

const CLASS: &str = "AnythingSettingsWindow";
static WINDOW: AtomicIsize = AtomicIsize::new(0);

pub struct Preferences {
    pub config: Config,
    pub autostart: bool,
    pub context_menu: bool,
}

struct SettingsWindow {
    hwnd: HWND,
    owner: HWND,
    font: HFONT,
    dpi: u32,
    roots: HWND,
    excludes: HWND,
    hotkey: HWND,
    rescan: HWND,
    threads: HWND,
    hdd_threads: HWND,
    pinyin: HWND,
    autostart: HWND,
    context_menu: HWND,
    initial: Option<(Config, bool, bool)>,
    on_save: Box<dyn FnMut(Preferences) -> Result<(), String>>,
}

fn scale(dpi: u32, v: i32) -> i32 {
    v * dpi as i32 / 96
}

unsafe fn text(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

unsafe fn set_text(hwnd: HWND, value: &str) {
    unsafe { SetWindowTextW(hwnd, wide(value).as_ptr()) };
}

fn lines(value: String) -> Vec<String> {
    value.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

impl SettingsWindow {
    unsafe fn control(&self, class: &str, value: &str, style: u32, ex: u32, id: usize, x: i32, y: i32, w: i32, h: i32) -> HWND {
        unsafe {
            let hwnd = CreateWindowExW(
                ex,
                wide(class).as_ptr(),
                wide(value).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                scale(self.dpi, x),
                scale(self.dpi, y),
                scale(self.dpi, w),
                scale(self.dpi, h),
                self.hwnd,
                id as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
            hwnd
        }
    }

    unsafe fn label(&self, value: &str, x: i32, y: i32, w: i32) {
        unsafe { self.control("STATIC", value, 0, 0, 0, x, y, w, 22) };
    }

    unsafe fn edit(&self, value: &str, id: usize, x: i32, y: i32, w: i32, h: i32, multiline: bool) -> HWND {
        let style = WS_TABSTOP | WS_VSCROLL
            | if multiline { ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32 } else { ES_AUTOHSCROLL as u32 };
        unsafe { self.control("EDIT", value, style, WS_EX_CLIENTEDGE, id, x, y, w, h) }
    }

    unsafe fn button(&self, value: &str, id: usize, x: i32, y: i32, w: i32, h: i32, style: i32) -> HWND {
        unsafe { self.control("BUTTON", value, WS_TABSTOP | style as u32, 0, id, x, y, w, h) }
    }

    unsafe fn create_controls(&mut self, config: &Config, autostart: bool, context_menu: bool) {
        unsafe {
            self.control("BUTTON", "常规", BS_GROUPBOX as u32, 0, 0, 16, 12, 568, 138);
            self.label("全局呼出快捷键：", 32, 40, 126);
            self.hotkey = self.edit(&config.hotkey, HOTKEY, 160, 36, 180, 25, false);
            self.label("例如 Ctrl+Alt+F、Alt+Space", 352, 40, 210);
            self.pinyin = self.button("拼音首字母搜索", PINYIN, 32, 72, 170, 24, BS_AUTOCHECKBOX);
            self.autostart = self.button("开机自动启动", AUTOSTART, 224, 72, 160, 24, BS_AUTOCHECKBOX);
            self.context_menu = self.button("资源管理器右键菜单", CONTEXT_MENU, 400, 72, 170, 24, BS_AUTOCHECKBOX);
            self.label("快捷键立即生效；其他选项按下方说明应用。", 32, 108, 500);

            self.control("BUTTON", "索引", BS_GROUPBOX as u32, 0, 0, 16, 160, 568, 270);
            self.label("索引目录（每行一个；留空表示所有本地固定磁盘）：", 32, 183, 500);
            self.roots = self.edit(&config.roots.join("\r\n"), ROOTS, 32, 207, 520, 62, true);
            self.button("添加目录…", BROWSE_ROOTS, 452, 275, 100, 26, BS_PUSHBUTTON);
            self.label("排除规则（每行一个完整目录或目录名）：", 32, 309, 500);
            self.excludes = self.edit(&config.excludes.join("\r\n"), EXCLUDES, 32, 333, 520, 58, true);
            self.button("添加排除目录…", BROWSE_EXCLUDES, 422, 397, 130, 26, BS_PUSHBUTTON);

            self.control("BUTTON", "性能", BS_GROUPBOX as u32, 0, 0, 16, 440, 568, 108);
            self.label("启动时重扫：", 32, 470, 126);
            self.rescan = self.control("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL, 0, RESCAN, 160, 466, 160, 200);
            for item in ["不重扫", "仅固态硬盘", "所有目录"] {
                SendMessageW(self.rescan, CB_ADDSTRING, 0, wide(item).as_ptr() as isize);
            }
            self.label("固态盘线程：", 32, 508, 100);
            self.threads = self.edit(&config.threads.to_string(), THREADS, 136, 504, 60, 25, false);
            self.label("机械盘线程：", 328, 508, 100);
            self.hdd_threads = self.edit(&config.hdd_threads.to_string(), HDD_THREADS, 432, 504, 60, 25, false);

            self.button("恢复默认", DEFAULTS, 24, 566, 100, 28, BS_PUSHBUTTON);
            self.button("保存", SAVE, 370, 566, 90, 28, BS_DEFPUSHBUTTON);
            self.button("取消", CANCEL, 474, 566, 90, 28, BS_PUSHBUTTON);
            self.set_config(config);
            SendMessageW(self.pinyin, BM_SETCHECK, if config.pinyin { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
            SendMessageW(self.autostart, BM_SETCHECK, if autostart { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
            SendMessageW(self.context_menu, BM_SETCHECK, if context_menu { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
        }
    }

    unsafe fn set_config(&self, config: &Config) {
        unsafe {
            set_text(self.roots, &config.roots.join("\r\n"));
            set_text(self.excludes, &config.excludes.join("\r\n"));
            set_text(self.hotkey, &config.hotkey);
            set_text(self.threads, &config.threads.to_string());
            set_text(self.hdd_threads, &config.hdd_threads.to_string());
            SendMessageW(self.rescan, CB_SETCURSEL, match config.rescan_on_start { Rescan::Off => 0, Rescan::Ssd => 1, Rescan::All => 2 }, 0);
            SendMessageW(self.pinyin, BM_SETCHECK, if config.pinyin { BST_CHECKED } else { BST_UNCHECKED } as usize, 0);
        }
    }

    unsafe fn read(&self) -> Result<Preferences, String> {
        unsafe {
            let mut config = Config::default();
            config.set_roots(lines(text(self.roots)));
            config.set_excludes(lines(text(self.excludes)));
            config.hotkey = text(self.hotkey).trim().to_string();
            config.rescan_on_start = match SendMessageW(self.rescan, CB_GETCURSEL, 0, 0) {
                0 => Rescan::Off,
                2 => Rescan::All,
                _ => Rescan::Ssd,
            };
            config.threads = text(self.threads).trim().parse::<usize>().ok().filter(|n| (1..=64).contains(n)).ok_or("固态盘线程数必须是 1～64 的整数")?;
            config.hdd_threads = text(self.hdd_threads).trim().parse::<usize>().ok().filter(|n| (1..=64).contains(n)).ok_or("机械盘线程数必须是 1～64 的整数")?;
            config.pinyin = SendMessageW(self.pinyin, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            Ok(Preferences {
                config,
                autostart: SendMessageW(self.autostart, BM_GETCHECK, 0, 0) == BST_CHECKED as isize,
                context_menu: SendMessageW(self.context_menu, BM_GETCHECK, 0, 0) == BST_CHECKED as isize,
            })
        }
    }

    unsafe fn browse(&self, title: &str, target: HWND) {
        unsafe {
            let title = wide(title);
            let mut bi: BROWSEINFOW = std::mem::zeroed();
            bi.hwndOwner = self.hwnd;
            bi.lpszTitle = title.as_ptr();
            bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
            let pidl = SHBrowseForFolderW(&bi);
            if pidl.is_null() {
                return;
            }
            let mut path = vec![0u16; 32768];
            let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
            CoTaskMemFree(pidl.cast());
            if ok != 0 {
                let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
                let path = String::from_utf16_lossy(&path[..len]);
                let old = text(target);
                let value = if old.trim().is_empty() { path } else { format!("{}\r\n{}", old.trim_end(), path) };
                set_text(target, &value);
            }
        }
    }

    unsafe fn command(&mut self, id: usize) {
        unsafe {
            match id {
                SAVE => {
                    let result = self.read().and_then(|p| (self.on_save)(p));
                    match result {
                        Ok(()) => {
                            DestroyWindow(self.hwnd);
                        }
                        Err(e) => {
                            MessageBoxW(self.hwnd, wide(&e).as_ptr(), wide("无法保存设置").as_ptr(), MB_OK | MB_ICONWARNING);
                        }
                    }
                }
                CANCEL => {
                    DestroyWindow(self.hwnd);
                }
                DEFAULTS => self.set_config(&Config::default()),
                BROWSE_ROOTS => self.browse("选择要添加的索引目录", self.roots),
                BROWSE_EXCLUDES => self.browse("选择要排除的目录", self.excludes),
                _ => {}
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp as *const CREATESTRUCTW);
                let state = cs.lpCreateParams as *mut SettingsWindow;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                (*state).hwnd = hwnd;
                (*state).dpi = GetDpiForWindow(hwnd);
                let (config, autostart, context_menu) = (*state).initial.take().unwrap();
                (*state).create_controls(&config, autostart, context_menu);
                WINDOW.store(hwnd as isize, Ordering::Release);
                return 0;
            }
            WM_COMMAND => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindow;
                if !state.is_null() {
                    (*state).command(wp & 0xffff);
                }
                return 0;
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                return 0;
            }
            WM_NCDESTROY => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsWindow;
                if !state.is_null() {
                    let state = Box::from_raw(state);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    EnableWindow(state.owner, 1);
                    SetForegroundWindow(state.owner);
                }
                WINDOW.store(0, Ordering::Release);
                return 0;
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// 打开唯一的设置窗口。保存回调返回错误时窗口保留并显示原因。
pub fn show(
    owner: HWND,
    config: Config,
    autostart: bool,
    context_menu: bool,
    font: HFONT,
    on_save: impl FnMut(Preferences) -> Result<(), String> + 'static,
) {
    unsafe {
        let existing = WINDOW.load(Ordering::Acquire) as HWND;
        if !existing.is_null() && IsWindow(existing) != 0 {
            SetForegroundWindow(existing);
            return;
        }
        let instance = GetModuleHandleW(null());
        let class = wide(CLASS);
        let atom = RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        });
        if atom == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            MessageBoxW(owner, wide("无法注册设置窗口").as_ptr(), wide("anything").as_ptr(), MB_OK | MB_ICONERROR);
            return;
        }
        let font = if font.is_null() { GetStockObject(DEFAULT_GUI_FONT) as HFONT } else { font };
        let dpi = GetDpiForWindow(owner).max(96);
        let state = Box::new(SettingsWindow {
            hwnd: null_mut(),
            owner,
            font,
            dpi,
            roots: null_mut(),
            excludes: null_mut(),
            hotkey: null_mut(),
            rescan: null_mut(),
            threads: null_mut(),
            hdd_threads: null_mut(),
            pinyin: null_mut(),
            autostart: null_mut(),
            context_menu: null_mut(),
            initial: Some((config, autostart, context_menu)),
            on_save: Box::new(on_save),
        });
        let raw = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            wide("anything 设置").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(dpi, 620),
            scale(dpi, 640),
            owner,
            null_mut(),
            instance,
            raw.cast(),
        );
        if hwnd.is_null() {
            drop(Box::from_raw(raw));
            MessageBoxW(owner, wide("无法创建设置窗口").as_ptr(), wide("anything").as_ptr(), MB_OK | MB_ICONERROR);
            return;
        }
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }
}

/// 让设置窗口支持 Tab、Enter、Esc 等标准对话框键盘导航。
pub fn handle_message(msg: &mut MSG) -> bool {
    unsafe {
        let hwnd = WINDOW.load(Ordering::Acquire) as HWND;
        !hwnd.is_null() && IsDialogMessageW(hwnd, msg) != 0
    }
}
