//! anything 图形界面：原生 Win32（搜索框 + 虚拟列表 + 状态栏 + 托盘 + 全局快捷键）

#![windows_subsystem = "windows"]

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anything::config::{self, Config};
use anything::engine::{Engine, Hits, Notice};
use anything::ipc;
use anything::query::SortCol;
use anything::util::{format_filetime, format_size_kb, group_digits, wide};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::GUID;

const CLASS: &str = "AnythingMainWindow";
const TITLE: &str = "anything";

const WM_APP_RESULT: u32 = WM_APP + 1;
const WM_APP_INDEX: u32 = WM_APP + 2;
const WM_APP_TRAY: u32 = WM_APP + 3;
const WM_APP_SHOW: u32 = WM_APP + 4;

const TIMER_SEARCH: usize = 1;
const TIMER_UI: usize = 2;
const HOTKEY_ID: i32 = 1;

const IDM_RESCAN: usize = 1001;
const IDM_SETTINGS: usize = 1002;
const IDM_RELOAD: usize = 1003;
const IDM_AUTOSTART: usize = 1004;
const IDM_DATADIR: usize = 1005;
const IDM_EXIT: usize = 1006;
const IDM_HELP: usize = 1010;
const IDM_CLI: usize = 1011;
const IDM_ABOUT: usize = 1012;

const IDC_OPEN: usize = 2001;
const IDC_FOLDER: usize = 2002;
const IDC_COPYPATH: usize = 2003;
const IDC_COPYNAME: usize = 2004;
const IDC_COPYFILE: usize = 2005;
const IDC_PROPS: usize = 2006;

const IDT_SHOW: usize = 3001;
const IDT_RESCAN: usize = 3002;
const IDT_EXIT: usize = 3003;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "anything";

const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);

const HELP_TEXT: &str = "搜索语法（与 Everything 接近）：

  abc def          同时包含 abc 和 def（空格 = 并且）
  a|b              包含 a 或 b
  !abc             不包含 abc
  *.psd   a?c      通配符，匹配完整文件名
  xmwj             拼音首字母，可搜到“项目文件”
  work\\  \\src\\ *.rs   含 \\ 的词在完整路径中匹配
  path:abc         在完整路径中匹配
  regex:^a.*\\.rs$   正则表达式
  ext:png;jpg      按扩展名
  file:  folder:   只要文件 / 只要文件夹
  size:>10mb       大小（> < >= <=，或 1mb..1gb）
  dm:today         修改时间：today yesterday week month year
  dm:2026-10-01    也可写日期、年月，或 >2026-01-01、2026-01..2026-06
  \"a b\"            引号包住含空格的词

快捷键：
  全局呼出/隐藏     见设置文件 hotkey（默认 Ctrl+Alt+F）
  ↓               从搜索框进入结果列表
  Enter           打开        Ctrl+Enter   打开所在文件夹
  Ctrl+C          复制完整路径  Ctrl+Shift+C 复制文件（可在资源管理器粘贴）
  Esc             清空搜索 / 隐藏窗口
  F5              立即重建索引
  结果可以直接拖到资源管理器或其他程序中";

const CLI_TEXT: &str = "命令行工具 ae.exe（类似 Everything 的 es.exe），供脚本和第三方程序调用：

  ae [选项] 搜索词...

  -n <数量>        最多输出多少条          -o <数量>  跳过前多少条
  -s <name|path|size|date>  排序（默认 name）   -desc  倒序
  -count           只输出匹配数量
  -size -dm        附加输出大小 / 修改时间
  -csv -tsv -json  输出格式（默认每行一个完整路径）
  -0               以 \\0 分隔（配合 xargs -0）
  -out <文件>       写入文件（UTF-8）
  -ansi            按系统代码页（GBK）输出，兼容老程序
  -offline         不连接 anything，直接读索引文件
  -status          显示索引状态     -rescan  让 anything 重建索引

例：ae -n 20 -s date -desc ext:log dm:today
    ae -json -size *.psd > psd.json

anything 运行时 ae 通过命名管道查询（毫秒级、实时）；
未运行时自动读取上次保存的索引文件。";

struct Pending {
    seq: u64,
    keep: bool,
    res: Result<Hits, String>,
}

struct App {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    status: HWND,
    menu: HMENU,
    font: HFONT,
    font_big: HFONT,
    dpi: u32,
    icon_small: HICON,
    engine: Arc<Engine>,
    results: Vec<u32>,
    res_gen: u64,
    seq: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<Pending>>>,
    sort_col: SortCol,
    sort_desc: bool,
    last_changes: u64,
    last_refresh: Instant,
    refresh_wanted: bool,
    bufs: [Vec<u16>; 4],
    buf_i: usize,
    ext_icons: HashMap<Vec<u8>, i32>,
    file_icons: HashMap<u32, i32>,
    last_focus: HWND,
    count_text: String,
    state_text: String,
    taskbar_created: u32,
    quitting: bool,
    hinted: bool,
}

static APP: AtomicPtr<App> = AtomicPtr::new(null_mut());

#[allow(clippy::mut_from_ref)]
fn app() -> &'static mut App {
    // 只在 UI 线程使用
    unsafe { &mut *APP.load(Relaxed) }
}

fn scale(v: i32) -> i32 {
    let p = APP.load(Relaxed);
    let dpi = if p.is_null() { 96 } else { unsafe { (*p).dpi } };
    v * dpi as i32 / 96
}

fn make_int_resource(id: u16) -> *const u16 {
    id as usize as *const u16
}

unsafe fn send(h: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe { SendMessageW(h, msg, wp, lp) }
}

fn get_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

fn make_font(pt: i32, dpi: u32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    unsafe {
        CreateFontW(
            -(pt * dpi as i32 / 72),
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            face.as_ptr(),
        )
    }
}

fn message_box(text: &str, title: &str, flags: u32) -> i32 {
    let h = if APP.load(Relaxed).is_null() { null_mut() } else { app().hwnd };
    unsafe { MessageBoxW(h, wide(text).as_ptr(), wide(title).as_ptr(), flags) }
}

// ---------------------------------------------------------------- 主程序

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a.eq_ignore_ascii_case("/min") || a.eq_ignore_ascii_case("--min"));
    unsafe {
        // 单实例：已在运行则唤出已有窗口
        let _mutex = CreateMutexW(null(), 1, wide(r"Local\anything-single-instance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let h = FindWindowW(wide(CLASS).as_ptr(), null());
            if !h.is_null() && !start_hidden {
                PostMessageW(h, WM_APP_SHOW, 0, 0);
            }
            return;
        }
        OleInitialize(null_mut());
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES };
        InitCommonControlsEx(&icc);
    }

    let engine = Engine::new(Config::load());
    let hinst = unsafe { GetModuleHandleW(null()) };
    let dpi = unsafe { GetDpiForSystem() };
    let (icon, icon_small) = unsafe {
        let big = LoadImageW(hinst, make_int_resource(1), IMAGE_ICON, GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON), 0) as HICON;
        let small = LoadImageW(hinst, make_int_resource(1), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), 0) as HICON;
        let fallback = LoadIconW(null_mut(), IDI_APPLICATION);
        (if big.is_null() { fallback } else { big }, if small.is_null() { fallback } else { small })
    };

    let app_box = Box::new(App {
        hwnd: null_mut(),
        edit: null_mut(),
        list: null_mut(),
        status: null_mut(),
        menu: null_mut(),
        font: make_font(9, dpi),
        font_big: make_font(11, dpi),
        dpi,
        icon_small,
        engine: engine.clone(),
        results: Vec::new(),
        res_gen: 0,
        seq: Arc::new(AtomicU64::new(0)),
        pending: Arc::new(Mutex::new(None)),
        sort_col: SortCol::Name,
        sort_desc: false,
        last_changes: 0,
        last_refresh: Instant::now(),
        refresh_wanted: false,
        bufs: Default::default(),
        buf_i: 0,
        ext_icons: HashMap::new(),
        file_icons: HashMap::new(),
        last_focus: null_mut(),
        count_text: String::new(),
        state_text: String::new(),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        quitting: false,
        hinted: false,
    });
    APP.store(Box::into_raw(app_box), SeqCst);

    let class = wide(CLASS);
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: icon_small,
        };
        RegisterClassExW(&wc);
        let menu = build_menu();
        app().menu = menu;
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide(TITLE).as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100 * dpi as i32 / 96,
            700 * dpi as i32 / 96,
            null_mut(),
            menu,
            hinst,
            null(),
        );
        if hwnd.is_null() {
            message_box("创建窗口失败", TITLE, MB_ICONERROR);
            return;
        }
        let h = hwnd as isize;
        engine.set_notify(Box::new(move |n| {
            PostMessageW(h as HWND, WM_APP_INDEX, n as usize, 0);
        }));
        engine.start();
        ipc::serve(engine.clone());
        add_tray();
        register_hotkey();
        if !start_hidden {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetFocus(app().edit);
        }
        SetTimer(hwnd, TIMER_UI, 500, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn build_menu() -> HMENU {
    unsafe {
        let bar = CreateMenu();
        let file = CreatePopupMenu();
        let add = |m: HMENU, id: usize, s: &str| {
            AppendMenuW(m, MF_STRING, id, wide(s).as_ptr());
        };
        add(file, IDM_RESCAN, "立即重建索引(&R)\tF5");
        add(file, IDM_SETTINGS, "编辑设置文件(&S)…");
        add(file, IDM_RELOAD, "重新加载设置并重建索引(&L)");
        add(file, IDM_AUTOSTART, "开机自动启动(&A)");
        add(file, IDM_DATADIR, "打开数据目录(&D)");
        AppendMenuW(file, MF_SEPARATOR, 0, null());
        add(file, IDM_EXIT, "退出(&X)");
        if autostart_get() {
            CheckMenuItem(file, IDM_AUTOSTART as u32, MF_CHECKED);
        }
        let help = CreatePopupMenu();
        add(help, IDM_HELP, "搜索语法与快捷键(&H)\tF1");
        add(help, IDM_CLI, "命令行工具 ae.exe(&C)");
        add(help, IDM_ABOUT, "关于(&A)");
        AppendMenuW(bar, MF_POPUP, file as usize, wide("文件(&F)").as_ptr());
        AppendMenuW(bar, MF_POPUP, help as usize, wide("帮助(&H)").as_ptr());
        bar
    }
}

// ---------------------------------------------------------------- 窗口过程

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let a = app();
        match msg {
            WM_CREATE => {
                a.hwnd = hwnd;
                a.dpi = GetDpiForWindow(hwnd);
                create_children(hwnd);
                return 0;
            }
            WM_SIZE => {
                layout();
                return 0;
            }
            WM_GETMINMAXINFO => {
                let mi = &mut *(lp as *mut MINMAXINFO);
                mi.ptMinTrackSize.x = scale(480);
                mi.ptMinTrackSize.y = scale(300);
                return 0;
            }
            WM_DPICHANGED => {
                let old = a.dpi;
                a.dpi = (wp & 0xffff) as u32;
                DeleteObject(a.font as HGDIOBJ);
                DeleteObject(a.font_big as HGDIOBJ);
                a.font = make_font(9, a.dpi);
                a.font_big = make_font(11, a.dpi);
                send(a.edit, WM_SETFONT, a.font_big as usize, 1);
                send(a.list, WM_SETFONT, a.font as usize, 1);
                send(a.status, WM_SETFONT, a.font as usize, 1);
                for c in 0..4 {
                    let w = send(a.list, LVM_GETCOLUMNWIDTH, c, 0) as i32;
                    send(a.list, LVM_SETCOLUMNWIDTH, c, (w * a.dpi as i32 / old as i32) as isize);
                }
                let r = &*(lp as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                return 0;
            }
            WM_ACTIVATE => {
                if (wp & 0xffff) as u32 == WA_INACTIVE {
                    a.last_focus = GetFocus();
                } else {
                    let f = if a.last_focus == a.list { a.list } else { a.edit };
                    SetFocus(f);
                }
                return 0;
            }
            WM_SETFOCUS => {
                SetFocus(a.edit);
                return 0;
            }
            WM_COMMAND => {
                let id = wp & 0xffff;
                let code = (wp >> 16) & 0xffff;
                if lp as HWND == a.edit && code == EN_CHANGE as usize {
                    SetTimer(hwnd, TIMER_SEARCH, 80, None);
                    return 0;
                }
                on_command(id);
                return 0;
            }
            WM_TIMER => {
                match wp {
                    TIMER_SEARCH => {
                        KillTimer(hwnd, TIMER_SEARCH);
                        start_search(false);
                    }
                    TIMER_UI => on_ui_timer(),
                    _ => {}
                }
                return 0;
            }
            WM_NOTIFY => return on_notify(&*(lp as *const NMHDR), lp),
            WM_APP_RESULT => {
                on_result();
                return 0;
            }
            WM_APP_INDEX => {
                if wp == Notice::Replaced as usize {
                    start_search(true);
                }
                return 0;
            }
            WM_APP_SHOW => {
                show_main();
                return 0;
            }
            WM_APP_TRAY => {
                match (lp & 0xffff) as u32 {
                    WM_LBUTTONUP => toggle_main(),
                    WM_RBUTTONUP => tray_menu(),
                    _ => {}
                }
                return 0;
            }
            WM_HOTKEY => {
                if wp as i32 == HOTKEY_ID {
                    toggle_main();
                }
                return 0;
            }
            WM_CLOSE => {
                if a.quitting {
                    DestroyWindow(hwnd);
                } else {
                    ShowWindow(hwnd, SW_HIDE);
                    if !a.hinted {
                        a.hinted = true;
                        tray_balloon("anything 仍在后台运行", "按全局快捷键或单击托盘图标可再次打开；退出请用托盘菜单。");
                    }
                }
                return 0;
            }
            WM_QUERYENDSESSION => return 1,
            WM_ENDSESSION => {
                if wp != 0 {
                    a.engine.shutdown();
                }
                return 0;
            }
            WM_DESTROY => {
                remove_tray();
                UnregisterHotKey(hwnd, HOTKEY_ID);
                a.engine.shutdown();
                PostQuitMessage(0);
                return 0;
            }
            _ => {
                if msg == a.taskbar_created && msg != 0 {
                    add_tray();
                    return 0;
                }
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

unsafe fn create_children(hwnd: HWND) {
    unsafe {
        let a = app();
        let hinst = GetModuleHandleW(null());
        a.edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            hwnd,
            100 as HMENU,
            hinst,
            null(),
        );
        send(a.edit, WM_SETFONT, a.font_big as usize, 1);
        let cue = wide("搜索文件名：支持拼音首字母、通配符、ext: size: dm: 等（F1 查看语法）");
        send(a.edit, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
        SetWindowSubclass(a.edit, Some(edit_proc), 1, 0);

        a.list = CreateWindowExW(
            0,
            WC_LISTVIEWW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS),
            0,
            0,
            0,
            0,
            hwnd,
            101 as HMENU,
            hinst,
            null(),
        );
        SetWindowTheme(a.list, wide("Explorer").as_ptr(), null());
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP | LVS_EX_HEADERDRAGDROP;
        send(a.list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
        send(a.list, WM_SETFONT, a.font as usize, 1);
        let cols: [(&str, i32, i32); 4] = [("名称", 300, LVCFMT_LEFT), ("路径", 480, LVCFMT_LEFT), ("大小", 100, LVCFMT_RIGHT), ("修改时间", 140, LVCFMT_LEFT)];
        for (i, (t, w, fmt)) in cols.iter().enumerate() {
            let mut txt = wide(t);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                fmt: *fmt,
                cx: scale(*w),
                pszText: txt.as_mut_ptr(),
                cchTextMax: 0,
                iSubItem: i as i32,
                iImage: 0,
                iOrder: 0,
                cxMin: 0,
                cxDefault: 0,
                cxIdeal: 0,
            };
            send(a.list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
        // 系统图标列表（按扩展名取图标，和资源管理器一致）
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let himl = SHGetFileInfoW(wide("C:\\").as_ptr(), 0, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON);
        send(a.list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, himl as isize);
        update_sort_arrows();

        a.status = CreateWindowExW(0, STATUSCLASSNAMEW, null(), WS_CHILD | WS_VISIBLE | SBARS_SIZEGRIP, 0, 0, 0, 0, hwnd, 102 as HMENU, hinst, null());
        send(a.status, WM_SETFONT, a.font as usize, 1);
    }
}

unsafe fn layout() {
    unsafe {
        let a = app();
        if a.edit.is_null() {
            return;
        }
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(a.hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);
        send(a.status, WM_SIZE, 0, 0);
        let mut sr: RECT = std::mem::zeroed();
        GetWindowRect(a.status, &mut sr);
        let sh = sr.bottom - sr.top;
        let parts = [scale(300), -1];
        send(a.status, SB_SETPARTS, 2, parts.as_ptr() as isize);
        let m = scale(6);
        let eh = scale(30);
        MoveWindow(a.edit, m, m, w - 2 * m, eh, 1);
        let top = eh + 2 * m;
        MoveWindow(a.list, 0, top, w, (h - top - sh).max(0), 1);
    }
}

// ---------------------------------------------------------------- 搜索

fn start_search(keep: bool) {
    let a = app();
    let text = get_text(a.edit);
    let seq = a.seq.fetch_add(1, SeqCst) + 1;
    let engine = a.engine.clone();
    let seqa = a.seq.clone();
    let pending = a.pending.clone();
    let hwnd = a.hwnd as isize;
    let (col, desc) = (a.sort_col, a.sort_desc);
    a.last_changes = a.engine.changes.load(SeqCst);
    a.refresh_wanted = false;
    std::thread::spawn(move || {
        let cancel = || seqa.load(SeqCst) != seq;
        let res = match engine.search(&text, col, desc, &cancel) {
            Ok(None) => return,
            Ok(Some(h)) => Ok(h),
            Err(e) => Err(e),
        };
        if cancel() {
            return;
        }
        *pending.lock().unwrap() = Some(Pending { seq, keep, res });
        unsafe { PostMessageW(hwnd as HWND, WM_APP_RESULT, 0, 0) };
    });
}

unsafe fn focused_index() -> i32 {
    unsafe { send(app().list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize) as i32 }
}

unsafe fn set_item_state(i: i32, state: u32, mask: u32) {
    unsafe {
        let mut it: LVITEMW = std::mem::zeroed();
        it.state = state;
        it.stateMask = mask;
        send(app().list, LVM_SETITEMSTATE, i as usize, &it as *const _ as isize);
    }
}

fn on_result() {
    let a = app();
    let Some(p) = a.pending.lock().unwrap().take() else { return };
    if p.seq != a.seq.load(SeqCst) {
        return;
    }
    unsafe {
        match p.res {
            Err(e) => {
                a.count_text = format!("搜索语法错误：{e}");
            }
            Ok(h) => {
                // 刷新时按条目编号保留选中项和焦点项
                let (old_focus, old_sel) = if p.keep {
                    let f = focused_index();
                    let focus = if f >= 0 && (f as usize) < a.results.len() { Some(a.results[f as usize]) } else { None };
                    let sel: HashSet<u32> = selected_rows().into_iter().filter(|&r| r < a.results.len()).map(|r| a.results[r]).collect();
                    (focus, sel)
                } else {
                    (None, HashSet::new())
                };
                a.results = h.ids;
                a.res_gen = h.generation;
                a.file_icons.clear();
                let n = a.results.len();
                if p.keep {
                    send(a.list, LVM_SETITEMCOUNT, n, (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize);
                    set_item_state(-1, 0, LVIS_SELECTED | LVIS_FOCUSED);
                    if !old_sel.is_empty() || old_focus.is_some() {
                        for (pos, id) in a.results.iter().enumerate() {
                            let mut st = 0;
                            if old_sel.contains(id) {
                                st |= LVIS_SELECTED;
                            }
                            if old_focus == Some(*id) {
                                st |= LVIS_FOCUSED;
                            }
                            if st != 0 {
                                set_item_state(pos as i32, st, st);
                            }
                        }
                    }
                } else {
                    send(a.list, LVM_SETITEMCOUNT, n, 0);
                    set_item_state(-1, 0, LVIS_SELECTED | LVIS_FOCUSED);
                    if n > 0 {
                        send(a.list, LVM_ENSUREVISIBLE, 0, 0);
                    }
                }
                InvalidateRect(a.list, null(), 0);
                a.count_text = format!("{} 个对象    {:.1} ms", group_digits(n as u64), h.millis);
                a.last_refresh = Instant::now();
            }
        }
        let t = wide(&a.count_text);
        send(a.status, SB_SETTEXTW, 0, t.as_ptr() as isize);
    }
}

fn on_ui_timer() {
    let a = app();
    let e = &a.engine;
    let state = if e.scanning.load(SeqCst) {
        let n = group_digits(e.scan_progress.load(SeqCst) as u64);
        let pending = e.pending_roots().join(" ");
        if e.ready.load(SeqCst) {
            format!("正在扫描 {pending} … 已扫描 {n} 个（不影响搜索）")
        } else {
            format!("正在建立索引 {pending} … 已扫描 {n} 个")
        }
    } else {
        e.status()
    };
    if state != a.state_text {
        a.state_text = state;
        let t = wide(&a.state_text);
        unsafe { send(a.status, SB_SETTEXTW, 1, t.as_ptr() as isize) };
    }
    // 实时更新：窗口可见时最多每 1.5 秒刷新一次结果
    let c = e.changes.load(SeqCst);
    if c != a.last_changes {
        let visible = unsafe { IsWindowVisible(a.hwnd) != 0 && IsIconic(a.hwnd) == 0 };
        // 选中了大量条目时先不自动刷新（恢复选中状态代价大，也避免打扰操作）
        let many_selected = unsafe { send(a.list, LVM_GETSELECTEDCOUNT, 0, 0) } > 5000;
        if visible && !many_selected {
            if a.last_refresh.elapsed().as_millis() >= 1500 {
                start_search(true);
            }
        } else {
            a.refresh_wanted = true;
        }
    }
}

// ---------------------------------------------------------------- 列表

fn put_text(s: &str) -> *mut u16 {
    let a = app();
    a.buf_i = (a.buf_i + 1) % a.bufs.len();
    let b = &mut a.bufs[a.buf_i];
    b.clear();
    b.extend(s.encode_utf16());
    b.push(0);
    b.as_mut_ptr()
}

fn ext_icon(name: &str, dir: bool) -> i32 {
    let a = app();
    let key: Vec<u8> = if dir {
        b"\\".to_vec()
    } else {
        match name.rfind('.') {
            Some(p) => name[p..].to_lowercase().into_bytes(),
            None => Vec::new(),
        }
    };
    if let Some(&i) = a.ext_icons.get(&key) {
        return i;
    }
    let probe = if dir { "folder".to_string() } else { format!("file{}", String::from_utf8_lossy(&key)) };
    let attr = if dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    let i = unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        SHGetFileInfoW(wide(&probe).as_ptr(), attr, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES);
        sfi.iIcon
    };
    a.ext_icons.insert(key, i);
    i
}

/// exe、快捷方式等每个文件图标不同，按文件取（只针对屏幕上可见的行，结果更换时清空缓存）
fn file_icon(id: u32, path: &str) -> i32 {
    let a = app();
    if let Some(&i) = a.file_icons.get(&id) {
        return i;
    }
    let i = unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let r = SHGetFileInfoW(wide(path).as_ptr(), 0, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON);
        if r == 0 { -1 } else { sfi.iIcon }
    };
    a.file_icons.insert(id, i);
    i
}

fn on_dispinfo(di: &mut NMLVDISPINFOW) {
    let a = app();
    let row = di.item.iItem;
    if row < 0 || row as usize >= a.results.len() {
        return;
    }
    let ix = a.engine.index.read().unwrap();
    let stale = a.res_gen != a.engine.generation.load(SeqCst);
    let id = a.results[row as usize];
    if stale || id as usize >= ix.len() {
        if di.item.mask & LVIF_TEXT != 0 {
            di.item.pszText = put_text("");
        }
        return;
    }
    let dir = ix.is_dir(id);
    if di.item.mask & LVIF_TEXT != 0 {
        let s = match di.item.iSubItem {
            0 => ix.name(id).to_string(),
            1 => ix.dir_display(id),
            2 => {
                if dir {
                    String::new()
                } else {
                    format_size_kb(ix.size[id as usize])
                }
            }
            3 => format_filetime(ix.mtime[id as usize]),
            _ => String::new(),
        };
        di.item.pszText = put_text(&s);
    }
    if di.item.mask & LVIF_IMAGE != 0 && di.item.iSubItem == 0 {
        let name = ix.name(id);
        let lower = name.to_ascii_lowercase();
        let special = !dir && [".exe", ".lnk", ".ico", ".url", ".cpl", ".scr", ".msc", ".appref-ms"].iter().any(|e| lower.ends_with(e));
        di.item.iImage = if special {
            let p = ix.full_path(id);
            drop(ix);
            let i = file_icon(id, &p);
            if i >= 0 { i } else { ext_icon(&lower, false) }
        } else {
            ext_icon(name, dir)
        };
    }
}

fn selected_rows() -> Vec<usize> {
    let a = app();
    let mut v = Vec::new();
    let mut i: isize = -1;
    loop {
        i = unsafe { send(a.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize) };
        if i < 0 {
            break;
        }
        v.push(i as usize);
    }
    v
}

/// 选中项的完整路径（没有选中时取焦点行或第一行）
fn selected_paths(fallback_first: bool) -> Vec<String> {
    let a = app();
    let mut rows = selected_rows();
    if rows.is_empty() && fallback_first && !a.results.is_empty() {
        rows.push(0);
    }
    let ix = a.engine.index.read().unwrap();
    if a.res_gen != a.engine.generation.load(SeqCst) {
        return Vec::new();
    }
    rows.into_iter()
        .filter(|&r| r < a.results.len() && (a.results[r] as usize) < ix.len())
        .map(|r| ix.full_path(a.results[r]))
        .collect()
}

fn parent_dir(p: &str) -> String {
    match p.trim_end_matches('\\').rfind('\\') {
        Some(i) => p[..i.max(2)].to_string() + if i <= 2 { "\\" } else { "" },
        None => String::new(),
    }
}

fn open_paths(paths: &[String]) {
    for p in paths.iter().take(16) {
        let dir = parent_dir(p);
        let r = unsafe { ShellExecuteW(app().hwnd, wide("open").as_ptr(), wide(p).as_ptr(), null(), wide(&dir).as_ptr(), SW_SHOWNORMAL) };
        if (r as isize) <= 32 {
            message_box(&format!("无法打开：\n{p}"), TITLE, MB_ICONWARNING);
        }
    }
}

fn open_folders(paths: &[String]) {
    for p in paths.iter().take(16) {
        unsafe {
            let pidl = ILCreateFromPathW(wide(p).as_ptr());
            if pidl.is_null() {
                let dir = parent_dir(p);
                ShellExecuteW(app().hwnd, wide("open").as_ptr(), wide(&dir).as_ptr(), null(), null(), SW_SHOWNORMAL);
                continue;
            }
            SHOpenFolderAndSelectItems(pidl, 0, null(), 0);
            ILFree(pidl);
        }
    }
}

fn set_clipboard_text(s: &str) {
    let w = wide(s);
    unsafe {
        if OpenClipboard(app().hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT as u32, h as HANDLE);
        }
        CloseClipboard();
    }
}

unsafe fn com_release(p: *mut c_void) {
    unsafe {
        if !p.is_null() {
            let vtbl = *(p as *const *const usize);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
            release(p);
        }
    }
}

/// 用外壳 API 创建包含这些文件的数据对象（剪贴板、拖放共用）。
/// 先建 IShellItemArray，再 BindToHandler(BHID_DataObject)，文件可来自不同目录。
fn make_data_object(paths: &[String]) -> *mut c_void {
    unsafe {
        let pidls: Vec<*mut ITEMIDLIST> = paths.iter().map(|p| ILCreateFromPathW(wide(p).as_ptr())).filter(|p| !p.is_null()).collect();
        if pidls.is_empty() {
            return null_mut();
        }
        let mut arr: *mut c_void = null_mut();
        let hr = SHCreateShellItemArrayFromIDLists(pidls.len() as u32, pidls.as_ptr() as *const *const ITEMIDLIST, &mut arr);
        for p in pidls {
            ILFree(p);
        }
        if hr < 0 || arr.is_null() {
            return null_mut();
        }
        // IShellItemArray::BindToHandler 位于虚表第 3 项（前 3 项是 IUnknown）
        type BindFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *const GUID, *mut *mut c_void) -> i32;
        let vtbl = *(arr as *const *const usize);
        let bind: BindFn = std::mem::transmute(*vtbl.add(3));
        let mut pdo: *mut c_void = null_mut();
        let hr = bind(arr, null_mut(), &BHID_DataObject, &IID_IDATAOBJECT, &mut pdo);
        com_release(arr);
        if hr < 0 { null_mut() } else { pdo }
    }
}

fn copy_files(paths: &[String]) {
    let pdo = make_data_object(paths);
    if pdo.is_null() {
        return;
    }
    unsafe {
        OleSetClipboard(pdo);
        OleFlushClipboard();
        com_release(pdo);
    }
}

fn begin_drag() {
    let paths = selected_paths(false);
    let pdo = make_data_object(&paths);
    if pdo.is_null() {
        return;
    }
    unsafe {
        let mut eff = 0u32;
        SHDoDragDrop(app().hwnd, pdo, null_mut(), DROPEFFECT_COPY | DROPEFFECT_LINK, &mut eff);
        com_release(pdo);
    }
}

fn file_names(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| p.trim_end_matches('\\').rsplit('\\').next().unwrap_or(p).to_string()).collect()
}

fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL as i32) < 0 }
}

fn shift_down() -> bool {
    unsafe { GetKeyState(VK_SHIFT as i32) < 0 }
}

fn activate(folder: bool) {
    let paths = selected_paths(true);
    if folder { open_folders(&paths) } else { open_paths(&paths) }
}

fn context_menu() {
    let paths = selected_paths(false);
    if paths.is_empty() {
        return;
    }
    unsafe {
        let m = CreatePopupMenu();
        let add = |id: usize, s: &str| {
            AppendMenuW(m, MF_STRING, id, wide(s).as_ptr());
        };
        add(IDC_OPEN, "打开(&O)\tEnter");
        add(IDC_FOLDER, "打开所在文件夹(&F)\tCtrl+Enter");
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(IDC_COPYPATH, "复制完整路径(&C)\tCtrl+C");
        add(IDC_COPYNAME, "复制文件名(&N)");
        add(IDC_COPYFILE, "复制文件(&Y)\tCtrl+Shift+C");
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(IDC_PROPS, "属性(&R)");
        SetMenuDefaultItem(m, IDC_OPEN as u32, 0);
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, app().hwnd, null());
        DestroyMenu(m);
        match cmd as usize {
            IDC_OPEN => open_paths(&paths),
            IDC_FOLDER => open_folders(&paths),
            IDC_COPYPATH => set_clipboard_text(&paths.join("\r\n")),
            IDC_COPYNAME => set_clipboard_text(&file_names(&paths).join("\r\n")),
            IDC_COPYFILE => copy_files(&paths),
            IDC_PROPS => {
                if let Some(p) = paths.first() {
                    SHObjectProperties(app().hwnd, SHOP_FILEPATH as u32, wide(p).as_ptr(), null());
                }
            }
            _ => {}
        }
    }
}

fn update_sort_arrows() {
    let a = app();
    unsafe {
        let hdr = send(a.list, LVM_GETHEADER, 0, 0) as HWND;
        let col = match a.sort_col {
            SortCol::Name => 0,
            SortCol::Path => 1,
            SortCol::Size => 2,
            SortCol::Date => 3,
            SortCol::None => -1,
        };
        for i in 0..4 {
            let mut it: HDITEMW = std::mem::zeroed();
            it.mask = HDI_FORMAT;
            send(hdr, HDM_GETITEMW, i, &mut it as *mut _ as isize);
            it.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if i as i32 == col {
                it.fmt |= if a.sort_desc { HDF_SORTDOWN } else { HDF_SORTUP };
            }
            send(hdr, HDM_SETITEMW, i, &it as *const _ as isize);
        }
    }
}

fn on_notify(h: &NMHDR, lp: LPARAM) -> LRESULT {
    let a = app();
    if h.hwndFrom != a.list {
        return 0;
    }
    unsafe {
        match h.code {
            LVN_GETDISPINFOW => on_dispinfo(&mut *(lp as *mut NMLVDISPINFOW)),
            NM_DBLCLK => activate(ctrl_down()),
            NM_RETURN => activate(ctrl_down()),
            NM_RCLICK => context_menu(),
            LVN_BEGINDRAG => begin_drag(),
            LVN_COLUMNCLICK => {
                let nm = &*(lp as *const NMLISTVIEW);
                let col = match nm.iSubItem {
                    0 => SortCol::Name,
                    1 => SortCol::Path,
                    2 => SortCol::Size,
                    _ => SortCol::Date,
                };
                if col == a.sort_col {
                    a.sort_desc = !a.sort_desc;
                } else {
                    a.sort_col = col;
                    a.sort_desc = matches!(col, SortCol::Size | SortCol::Date);
                }
                update_sort_arrows();
                start_search(false);
            }
            LVN_KEYDOWN => {
                let k = (*(lp as *const NMLVKEYDOWN)).wVKey;
                match k {
                    VK_C if ctrl_down() && shift_down() => copy_files(&selected_paths(false)),
                    VK_C if ctrl_down() => set_clipboard_text(&selected_paths(false).join("\r\n")),
                    VK_A if ctrl_down() => set_item_state(-1, LVIS_SELECTED, LVIS_SELECTED),
                    VK_ESCAPE | VK_BACK => {
                        SetFocus(a.edit);
                    }
                    VK_UP if focused_index() <= 0 => {
                        SetFocus(a.edit);
                    }
                    VK_F1 => on_command(IDM_HELP),
                    VK_F5 => on_command(IDM_RESCAN),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    0
}

// ---------------------------------------------------------------- 搜索框

unsafe extern "system" fn edit_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        let a = app();
        match msg {
            WM_KEYDOWN => match wp as u16 {
                VK_DOWN | VK_NEXT => {
                    if !a.results.is_empty() {
                        SetFocus(a.list);
                        if send(a.list, LVM_GETSELECTEDCOUNT, 0, 0) == 0 {
                            set_item_state(0, LVIS_SELECTED | LVIS_FOCUSED, LVIS_SELECTED | LVIS_FOCUSED);
                            send(a.list, LVM_ENSUREVISIBLE, 0, 0);
                        }
                    }
                    return 0;
                }
                VK_RETURN => {
                    // 输入后马上回车：先完成搜索
                    if KillTimer(a.hwnd, TIMER_SEARCH) != 0 {
                        start_search(false);
                        return 0;
                    }
                    activate(ctrl_down());
                    return 0;
                }
                VK_ESCAPE => {
                    if GetWindowTextLengthW(h) > 0 {
                        SetWindowTextW(h, wide("").as_ptr());
                    } else {
                        ShowWindow(a.hwnd, SW_HIDE);
                    }
                    return 0;
                }
                VK_A if ctrl_down() => {
                    send(h, EM_SETSEL, 0, -1);
                    return 0;
                }
                VK_F1 => {
                    on_command(IDM_HELP);
                    return 0;
                }
                VK_F5 => {
                    on_command(IDM_RESCAN);
                    return 0;
                }
                _ => {}
            },
            // 回车、Esc、Ctrl+A 不要发出提示音；Ctrl+Backspace 删除前一个词
            WM_CHAR => match wp {
                0x0d | 0x1b | 0x01 => return 0,
                0x7f => {
                    let mut start = 0u32;
                    send(h, EM_GETSEL, &mut start as *mut u32 as usize, 0);
                    let text: Vec<u16> = get_text(h).encode_utf16().collect();
                    let mut i = (start as usize).min(text.len());
                    while i > 0 && text[i - 1] == b' ' as u16 {
                        i -= 1;
                    }
                    while i > 0 && text[i - 1] != b' ' as u16 {
                        i -= 1;
                    }
                    send(h, EM_SETSEL, i, start as isize);
                    send(h, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
                    return 0;
                }
                _ => {}
            },
            _ => {}
        }
        DefSubclassProc(h, msg, wp, lp)
    }
}

// ---------------------------------------------------------------- 菜单命令

fn on_command(id: usize) {
    let a = app();
    match id {
        IDM_RESCAN | IDT_RESCAN => {
            if a.engine.scanning.load(SeqCst) {
                message_box("正在建立索引，请稍候。", TITLE, MB_ICONINFORMATION);
            } else {
                a.engine.rescan();
            }
        }
        IDM_SETTINGS => unsafe {
            let _ = Config::load(); // 确保文件存在
            ShellExecuteW(a.hwnd, wide("open").as_ptr(), wide("notepad.exe").as_ptr(), wide(&format!("\"{}\"", config::settings_path().display())).as_ptr(), null(), SW_SHOWNORMAL);
        },
        IDM_RELOAD => {
            a.engine.reload_config();
            register_hotkey();
        }
        IDM_AUTOSTART => {
            let on = !autostart_get();
            autostart_set(on);
            unsafe {
                let file = GetSubMenu(a.menu, 0);
                CheckMenuItem(file, IDM_AUTOSTART as u32, if autostart_get() { MF_CHECKED } else { MF_UNCHECKED });
            }
        }
        IDM_DATADIR => unsafe {
            ShellExecuteW(a.hwnd, wide("open").as_ptr(), wide(&config::data_dir().display().to_string()).as_ptr(), null(), null(), SW_SHOWNORMAL);
        },
        IDM_EXIT | IDT_EXIT => {
            a.quitting = true;
            unsafe { DestroyWindow(a.hwnd) };
        }
        IDM_HELP => {
            message_box(HELP_TEXT, "anything 帮助", MB_ICONINFORMATION);
        }
        IDM_CLI => {
            message_box(CLI_TEXT, "命令行工具 ae.exe", MB_ICONINFORMATION);
        }
        IDM_ABOUT => {
            let ix = a.engine.index.read().unwrap();
            let s = format!(
                "anything 0.1.0\n无需管理员权限的本地文件名搜索\n\n已索引 {} 个对象\n索引内存约 {:.0} MB\n数据目录：{}",
                group_digits(ix.alive() as u64),
                ix.mem_bytes() as f64 / 1048576.0,
                config::data_dir().display()
            );
            drop(ix);
            message_box(&s, "关于 anything", MB_ICONINFORMATION);
        }
        IDT_SHOW => show_main(),
        _ => {}
    }
}

fn show_main() {
    let a = app();
    unsafe {
        ShowWindow(a.hwnd, if IsIconic(a.hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(a.hwnd);
        SetFocus(a.edit);
        send(a.edit, EM_SETSEL, 0, -1);
    }
    if a.refresh_wanted {
        start_search(true);
    }
}

fn toggle_main() {
    let a = app();
    let shown = unsafe { IsWindowVisible(a.hwnd) != 0 && IsIconic(a.hwnd) == 0 && GetForegroundWindow() == a.hwnd };
    if shown {
        unsafe { ShowWindow(a.hwnd, SW_HIDE) };
    } else {
        show_main();
    }
}

// ---------------------------------------------------------------- 托盘、快捷键、自启动

fn tray_data() -> NOTIFYICONDATAW {
    let a = app();
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = a.hwnd;
    nid.uID = 1;
    nid
}

fn add_tray() {
    let a = app();
    let mut nid = tray_data();
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = a.icon_small;
    let tip: Vec<u16> = "anything 文件搜索".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
}

fn tray_balloon(title: &str, text: &str) {
    let mut nid = tray_data();
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    let t: Vec<u16> = title.encode_utf16().take(63).collect();
    let m: Vec<u16> = text.encode_utf16().take(255).collect();
    nid.szInfoTitle[..t.len()].copy_from_slice(&t);
    nid.szInfo[..m.len()].copy_from_slice(&m);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

fn remove_tray() {
    let nid = tray_data();
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}

fn tray_menu() {
    let a = app();
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, IDT_SHOW, wide("显示 anything(&S)").as_ptr());
        AppendMenuW(m, MF_STRING, IDT_RESCAN, wide("重建索引(&R)").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, IDT_EXIT, wide("退出(&X)").as_ptr());
        SetMenuDefaultItem(m, IDT_SHOW as u32, 0);
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(a.hwnd);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, a.hwnd, null());
        PostMessageW(a.hwnd, WM_NULL, 0, 0);
        DestroyMenu(m);
        if cmd != 0 {
            on_command(cmd as usize);
        }
    }
}

fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut vk = 0u32;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" => mods |= MOD_WIN,
            "space" => vk = VK_SPACE as u32,
            "`" | "~" | "oem3" => vk = VK_OEM_3 as u32,
            "tab" => vk = VK_TAB as u32,
            "enter" => vk = VK_RETURN as u32,
            k if k.len() >= 2 && k.starts_with('f') && k[1..].parse::<u32>().is_ok() => {
                let n: u32 = k[1..].parse().ok()?;
                if !(1..=24).contains(&n) {
                    return None;
                }
                vk = VK_F1 as u32 + n - 1;
            }
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => vk = k.as_bytes()[0].to_ascii_uppercase() as u32,
            _ => return None,
        }
    }
    if vk == 0 { None } else { Some((mods, vk)) }
}

fn register_hotkey() {
    let a = app();
    let hk = a.engine.cfg().hotkey.clone();
    unsafe {
        UnregisterHotKey(a.hwnd, HOTKEY_ID);
        match parse_hotkey(&hk) {
            Some((mods, vk)) => {
                if RegisterHotKey(a.hwnd, HOTKEY_ID, mods | MOD_NOREPEAT, vk) == 0 {
                    message_box(&format!("全局快捷键 {hk} 已被其他程序占用。\n可在“文件 → 编辑设置文件”中修改 hotkey。"), TITLE, MB_ICONWARNING);
                }
            }
            None => {
                message_box(&format!("无法识别的快捷键：{hk}"), TITLE, MB_ICONWARNING);
            }
        }
    }
}

fn autostart_get() -> bool {
    unsafe {
        let mut size = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size) == ERROR_SUCCESS
    }
}

fn autostart_set(on: bool) {
    unsafe {
        if on {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let v = wide(&format!("\"{exe}\" /min"));
            RegSetKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr(), REG_SZ, v.as_ptr() as *const c_void, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(RUN_VALUE).as_ptr());
        }
    }
}
