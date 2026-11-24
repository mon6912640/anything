//! 原生控件回归测试；显式启用的程序测试仅扫描隔离的临时目录，不注册右键菜单。
use super::*;
use anything::index::{F_DIR, Index, NONE};
use anything::query::{self, Query};
use anything::util::fold;

unsafe extern "system" fn test_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NOTIFY {
            let hdr = &*(lp as *const NMHDR);
            if hdr.code == NM_CUSTOMDRAW {
                return list_custom_draw(&mut *(lp as *mut NMLVCUSTOMDRAW));
            }
            if hdr.code == LVN_GETDISPINFOW {
                let info = &mut *(lp as *mut NMLVDISPINFOW);
                if info.item.mask & LVIF_TEXT != 0 {
                    info.item.pszText = windows_sys::core::w!("test.txt") as *mut u16;
                }
            }
        }
        DefWindowProcW(h, msg, wp, lp)
    }
}

struct TestWindow(HWND);
impl TestWindow {
    fn new() -> Self {
        unsafe {
            InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES,
            });
            let instance = GetModuleHandleW(null());
            let class = windows_sys::core::w!("AnythingUiRegressionTests");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(test_proc),
                hInstance: instance,
                lpszClassName: class,
                ..std::mem::zeroed()
            });
            let hwnd = CreateWindowExW(0, class, class, WS_OVERLAPPEDWINDOW,
                0, 0, 640, 480, null_mut(), null_mut(), instance, null());
            assert!(!hwnd.is_null());
            Self(hwnd)
        }
    }

    fn control(&self, class: *const u16, style: u32) -> HWND {
        unsafe {
            let hwnd = CreateWindowExW(0, class, windows_sys::core::w!(""),
                WS_CHILD | WS_VISIBLE | style, 0, 0, 600, 400, self.0,
                null_mut(), GetModuleHandleW(null()), null());
            assert!(!hwnd.is_null());
            hwnd
        }
    }
}
impl Drop for TestWindow {
    fn drop(&mut self) { unsafe { DestroyWindow(self.0); } }
}

struct Render {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl Render {
    fn new(list: HWND) -> Self {
        unsafe {
            let screen = GetDC(list);
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, 600, 400);
            ReleaseDC(list, screen);
            assert!(!dc.is_null() && !bitmap.is_null());
            let previous = SelectObject(dc, bitmap);
            let result = Self { dc, bitmap, previous };
            result.paint(list);
            result
        }
    }

    fn paint(&self, list: HWND) {
        unsafe {
            send(list, WM_PRINT, self.dc as usize,
                (PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize);
        }
    }

    fn row_color(&self, list: HWND, row: usize, x: i32) -> u32 {
        unsafe {
            let mut rect = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
            assert_ne!(send(list, LVM_GETITEMRECT, row, &mut rect as *mut _ as isize), 0);
            GetPixel(self.dc, x, (rect.top + rect.bottom) / 2)
        }
    }

    fn save(&self, name: &str) {
        unsafe {
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = 600;
            info.bmiHeader.biHeight = -400;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut pixels = vec![0u8; 600 * 400 * 4];
            // GetDIBits 要求位图未选入 DC。
            SelectObject(self.dc, self.previous);
            let rows = GetDIBits(self.dc, self.bitmap, 0, 400,
                pixels.as_mut_ptr() as *mut c_void, &mut info, DIB_RGB_COLORS);
            SelectObject(self.dc, self.bitmap);
            assert_eq!(rows, 400);
            let mut bmp = Vec::new();
            bmp.extend_from_slice(b"BM");
            bmp.extend_from_slice(&(54u32 + pixels.len() as u32).to_le_bytes());
            bmp.extend_from_slice(&[0u8; 4]);
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(std::slice::from_raw_parts(
                &info.bmiHeader as *const _ as *const u8, 40));
            bmp.extend_from_slice(&pixels);
            let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-validation");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(name), bmp).unwrap();
        }
    }
}
impl Drop for Render {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

#[test]
fn native_list_stripes_selection_scroll_and_refresh() {
    let parent = TestWindow::new();
    let list = parent.control(WC_LISTVIEWW, LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS);
    unsafe {
        SetWindowTheme(list, wide("Explorer").as_ptr(), null());
        send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
        for (i, width) in [250, 350].into_iter().enumerate() {
            let column = LVCOLUMNW {
                mask: LVCF_WIDTH | LVCF_TEXT,
                cx: width,
                pszText: windows_sys::core::w!("Column") as *mut u16,
                ..std::mem::zeroed()
            };
            send(list, LVM_INSERTCOLUMNW, i, &column as *const _ as isize);
        }
        send(list, LVM_SETITEMCOUNT, 100, 0);
        let render = Render::new(list);
        let check = |row| {
            let expected = if row % 2 == 0 { GetSysColor(COLOR_WINDOW) } else { 0x00F5F5F5 };
            for x in [200, 500] {
                assert_eq!(render.row_color(list, row, x), expected, "row={row}, x={x}");
            }
        };
        for row in 0..6 { check(row); }
        render.save("zebra.bmp");

        // 选中浅灰行：默认高亮应覆盖整行的条纹背景。
        let state = LVITEMW { stateMask: LVIS_SELECTED, state: LVIS_SELECTED, ..std::mem::zeroed() };
        send(list, LVM_SETITEMSTATE, 1, &state as *const _ as isize);
        render.paint(list);
        let selected = render.row_color(list, 1, 500);
        assert_ne!(selected, 0x00F5F5F5);
        assert_eq!(selected, render.row_color(list, 1, 200));
        render.save("zebra-selected.bmp");
        let state = LVITEMW { stateMask: LVIS_SELECTED, ..std::mem::zeroed() };
        send(list, LVM_SETITEMSTATE, 1, &state as *const _ as isize);

        send(list, LVM_ENSUREVISIBLE, 61, 0);
        render.paint(list);
        check(61);
        check(60);
        send(list, LVM_SETITEMCOUNT, 4, 0);
        send(list, LVM_ENSUREVISIBLE, 0, 0);
        render.paint(list);
        for row in 0..4 { check(row); }
    }
}

#[test]
fn empty_queries_never_start_a_search() {
    for text in ["", "  \t ", "\"\"", "path:", "under:", "regex:"] {
        assert!(is_empty_query(text, true), "应按空查询处理：{text:?}");
    }
    for text in ["needle", "file:", "folder:", "ext:rs", "under:C:\\", "regex:("] {
        assert!(!is_empty_query(text, true), "不是空查询：{text:?}");
    }
}

#[test]
fn selection_restore_is_computed_from_result_ids() {
    let old = Selection {
        clear_rows: vec![1, 2, 3],
        focus: Some(20),
        selected: HashSet::from([10, 20, 30]),
    };
    let restored = restore_selection(&[5, 30, 20, 10], &old);
    assert_eq!(restored, vec![
        (1, LVIS_SELECTED),
        (2, LVIS_SELECTED | LVIS_FOCUSED),
        (3, LVIS_SELECTED),
    ]);
}

#[test]
fn native_virtual_list_reset_clears_selection_without_all_item_state() {
    let parent = TestWindow::new();
    let list = parent.control(WC_LISTVIEWW, LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS);
    const N: usize = 1_000_000;
    unsafe {
        send(list, LVM_SETITEMCOUNT, N, 0);
        for row in [1, 999_999] {
            let item = LVITEMW { stateMask: LVIS_SELECTED, state: LVIS_SELECTED, ..std::mem::zeroed() };
            send(list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
        }
        let focus = LVITEMW { stateMask: LVIS_FOCUSED, state: LVIS_FOCUSED, ..std::mem::zeroed() };
        send(list, LVM_SETITEMSTATE, 2, &focus as *const _ as isize);
        assert_eq!(send(list, LVM_GETSELECTEDCOUNT, 0, 0), 2);

        reset_list(list, N);

        assert_eq!(send(list, LVM_GETITEMCOUNT, 0, 0), N as isize);
        assert_eq!(send(list, LVM_GETSELECTEDCOUNT, 0, 0), 0);
        assert_eq!(send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize), -1);
    }
}

#[test]
fn native_edit_directory_query_then_type_and_search() {
    let parent = TestWindow::new();
    let edit = parent.control(windows_sys::core::w!("EDIT"), ES_AUTOHSCROLL as u32);
    for directory in [r"C:\work\hl-dev", r"C:\My Docs", r"C:\中文 资料!", r"C:\work\hl-dev\"] {
        let query = dir_query(directory);
        set_search_text(edit, &query);
        let mut start = 0u32;
        let mut end = 0u32;
        unsafe {
            send(edit, EM_GETSEL, &mut start as *mut _ as usize, &mut end as *mut _ as isize);
            let length = query.encode_utf16().count() as u32;
            assert_eq!((start, end), (length, length));
            assert!(get_text(edit).ends_with(' '));
            send(edit, EM_REPLACESEL, 1, wide("needle").as_ptr() as isize);
        }
        assert_eq!(get_text(edit), format!("{query}needle"));
        let mut index = Index::new();
        let root_name = directory.trim_end_matches('\\');
        let root = index.push(root_name, &fold(root_name), None, NONE, F_DIR, 0, 0);
        let other = index.push("C:\\outside", b"c:\\outside", None, NONE, F_DIR, 0, 0);
        let expected = index.push("needle.txt", b"needle.txt", None, root, 0, 10, 0);
        index.push("unrelated.txt", b"unrelated.txt", None, root, 0, 10, 0);
        index.push("needle.txt", b"needle.txt", None, other, 0, 10, 0);
        index.rebuild_map();
        let parsed = Query::parse(&get_text(edit), false).unwrap();
        assert_eq!(query::search(&index, &parsed, &|| false).unwrap(), vec![expected], "{directory}");
    }
}

#[test]
fn native_settings_save_cancel_and_defaults() {
    use std::sync::{Arc, Mutex};

    let parent = TestWindow::new();
    let saved = Arc::new(Mutex::new(Vec::new()));
    let mut custom = Config::default();
    custom.set_roots(vec![r"D:\Work\".into(), r"E:\Archive".into()]);
    custom.set_excludes(vec!["Node_Modules".into(), r"D:\Temp\".into()]);
    custom.hotkey = "Ctrl+Alt+Space".into();
    custom.rescan_on_start = anything::config::Rescan::Off;
    custom.threads = 7;
    custom.hdd_threads = 3;
    custom.pinyin = false;

    let open = |config: Config, saved: &Arc<Mutex<Vec<settings::Preferences>>>| {
        let saved = Arc::clone(saved);
        settings::show(parent.0, config, true, false, null_mut(), move |prefs| {
            saved.lock().unwrap().push(prefs);
            Ok(())
        });
        let window = unsafe { FindWindowW(wide("AnythingSettingsWindow").as_ptr(), null()) };
        assert!(!window.is_null());
        assert_eq!(unsafe { IsWindowEnabled(parent.0) }, 0);
        window
    };

    let window = open(custom.clone(), &saved);
    unsafe {
        assert_eq!(get_text(GetDlgItem(window, settings::ROOTS as i32)), "D:\\Work\r\nE:\\Archive");
        assert_eq!(get_text(GetDlgItem(window, settings::EXCLUDES as i32)), "Node_Modules\r\nD:\\Temp");
        SetWindowTextW(GetDlgItem(window, settings::THREADS as i32), wide("9").as_ptr());
        SendMessageW(window, WM_COMMAND, settings::SAVE, 0);
        assert!(IsWindow(window) == 0);
        assert_ne!(IsWindowEnabled(parent.0), 0);
    }
    {
        let records = saved.lock().unwrap();
        assert_eq!(records.len(), 1);
        let prefs = &records[0];
        assert_eq!(prefs.config.roots, custom.roots);
        assert_eq!(prefs.config.excludes, custom.excludes);
        assert_eq!(prefs.config.hotkey, custom.hotkey);
        assert_eq!(prefs.config.threads, 9);
        assert_eq!(prefs.config.hdd_threads, 3);
        assert!(!prefs.config.pinyin);
        assert!(prefs.autostart);
        assert!(!prefs.context_menu);
    }

    let window = open(custom, &saved);
    unsafe {
        SendMessageW(window, WM_COMMAND, settings::DEFAULTS, 0);
        assert_eq!(get_text(GetDlgItem(window, settings::THREADS as i32)), "12");
        assert_eq!(get_text(GetDlgItem(window, settings::HOTKEY as i32)), "Ctrl+Alt+F");
        assert_eq!(saved.lock().unwrap().len(), 1, "恢复默认不能提前保存");
        SendMessageW(window, WM_COMMAND, settings::CANCEL, 0);
        assert!(IsWindow(window) == 0);
        assert_ne!(IsWindowEnabled(parent.0), 0);
    }
    assert_eq!(saved.lock().unwrap().len(), 1, "取消不能调用保存回调");
}

#[test]
fn hotkey_parser_accepts_supported_combinations_and_rejects_invalid_input() {
    assert_eq!(parse_hotkey("Ctrl+Alt+F"), Some((MOD_CONTROL | MOD_ALT, 'F' as u32)));
    assert_eq!(parse_hotkey(" shift + space "), Some((MOD_SHIFT, VK_SPACE as u32)));
    assert_eq!(parse_hotkey("Win+F24"), Some((MOD_WIN, VK_F24 as u32)));
    for invalid in ["", "Ctrl+Alt", "Ctrl+Unknown", "Ctrl+F25"] {
        assert_eq!(parse_hotkey(invalid), None, "{invalid}");
    }
}

/// 运行前构建 release，并设置 ANYTHING_UI_TEST_EXE；需要没有其他 anything 实例。
#[test]
#[ignore = "explicitly launch the built application in an isolated test data directory"]
fn executable_path_cold_start_and_existing_instance() {
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    struct RunningApp(Child);
    impl Drop for RunningApp {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(Instant::now() < deadline, "application did not reach expected UI state");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn remote_text(edit: HWND) -> String {
        unsafe {
            let length = send(edit, WM_GETTEXTLENGTH, 0, 0).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            let got = send(edit, WM_GETTEXT, text.len(), text.as_mut_ptr() as isize).max(0) as usize;
            String::from_utf16_lossy(&text[..got])
        }
    }
    fn replace_text(edit: HWND, text: &str) {
        unsafe {
            send(edit, EM_SETSEL, 0, -1);
            send(edit, EM_REPLACESEL, 1, wide(text).as_ptr() as isize);
        }
    }
    fn append_and_check(edit: HWND, list: HWND, directory: &str, initial_count: isize) {
        let query = dir_query(directory);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = remote_text(edit);
            let count = unsafe { send(list, LVM_GETITEMCOUNT, 0, 0) };
            if Instant::now() >= deadline {
                let window = unsafe { GetParent(edit) };
                let status = unsafe { GetDlgItem(window, 102) };
                eprintln!("status={:?}, textlength={}, err={}", remote_text(status), unsafe { GetWindowTextLengthW(edit) }, unsafe { GetLastError() });
            }
            if text == query && count == initial_count { break; }
            assert!(Instant::now() < deadline, "expected text={query:?}, count={initial_count}; actual text={text:?}, count={count}");
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            let selection = send(edit, EM_GETSEL, 0, 0) as u32;
            let length = query.encode_utf16().count() as u32;
            assert_eq!((selection & 0xffff, selection >> 16), (length, length));
            assert!(remote_text(edit).ends_with(' '));
            send(edit, EM_REPLACESEL, 1, wide("needle").as_ptr() as isize);
        }
        assert_eq!(remote_text(edit), format!("{query}needle"));
        wait_until(|| unsafe { send(list, LVM_GETITEMCOUNT, 0, 0) == 1 });
    }

    unsafe {
        assert!(FindWindowW(wide(CLASS).as_ptr(), null()).is_null(),
            "close existing anything instances before this explicit test");
    }
    let executable = std::env::var_os("ANYTHING_UI_TEST_EXE")
        .expect("set ANYTHING_UI_TEST_EXE to the executable being verified");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target").join("ui-validation").join(format!("runtime-{}", std::process::id()));
    let first = data.join("files").join("plain");
    let second = data.join("files").join("中文 My Docs!");
    for directory in [&first, &second] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("needle.txt"), b"matching file").unwrap();
        std::fs::write(directory.join("unrelated.txt"), b"other file").unwrap();
    }
    std::fs::write(second.join("third.txt"), b"third file").unwrap();
    std::fs::write(data.join("settings.ini"), format!(
        "roots={}\nexcludes=\nhotkey=Ctrl+Alt+Shift+F12\nrescan_on_start=0\nthreads=2\npinyin=1\n",
        data.join("files").display())).unwrap();

    let mut running = RunningApp(Command::new(&executable)
        .args(["--path", first.to_str().unwrap()])
        .env("ANYTHING_DATA_DIR", &data).spawn().unwrap());
    let mut window = null_mut();
    wait_until(|| {
        assert!(running.0.try_wait().unwrap().is_none(), "application exited early");
        unsafe { window = FindWindowW(wide(CLASS).as_ptr(), null()); }
        !window.is_null()
    });
    unsafe {
        let mut process_id = 0;
        GetWindowThreadProcessId(window, &mut process_id);
        assert_eq!(process_id, running.0.id(), "must only address our test process");
        let mut edit = null_mut();
        let mut list = null_mut();
        wait_until(|| {
            edit = FindWindowExW(window, null_mut(), windows_sys::core::w!("Edit"), null());
            list = FindWindowExW(window, null_mut(), WC_LISTVIEWW, null());
            !edit.is_null() && !list.is_null()
        });
        append_and_check(edit, list, first.to_str().unwrap(), 2);
        println!("cold-start --path: separator, caret and search results passed");

        // 与资源管理器右键菜单使用完全相同的启动参数，走已有实例的 WM_COPYDATA 分支。
        let mut invocation = Command::new(&executable)
            .args(["--path", second.to_str().unwrap()])
            .env("ANYTHING_DATA_DIR", &data).spawn().unwrap();
        wait_until(|| invocation.try_wait().unwrap().is_some());
        assert!(invocation.wait().unwrap().success());
        assert!(running.0.try_wait().unwrap().is_none());
        assert_eq!(FindWindowW(wide(CLASS).as_ptr(), null()), window);
        append_and_check(edit, list, second.to_str().unwrap(), 3);
        println!("existing-instance --path: separator, caret and search results passed");

        // 普通隐藏再唤出：焦点回到搜索框并全选旧查询，输入一次即可完整替换。
        let previous = remote_text(edit);
        assert!(!previous.is_empty());
        PostMessageW(window, WM_CLOSE, 0, 0);
        wait_until(|| IsWindowVisible(window) == 0);
        PostMessageW(window, WM_APP_SHOW, 0, 0);
        let thread = GetWindowThreadProcessId(window, null_mut());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut start = 0u32;
            let mut end = 0u32;
            send(edit, EM_GETSEL, &mut start as *mut _ as usize, &mut end as *mut _ as isize);
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            let got_info = GetGUIThreadInfo(thread, &mut info) != 0;
            let ready = IsWindowVisible(window) != 0
                && got_info
                && info.hwndFocus == edit
                && start == 0
                && end as usize == previous.encode_utf16().count();
            if ready { break; }
            assert!(Instant::now() < deadline,
                "show state: visible={}, got_info={}, focus={:?}, edit={:?}, selection={}..{}, expected=0..{}, foreground={:?}",
                IsWindowVisible(window), got_info, info.hwndFocus, edit, start, end,
                previous.encode_utf16().count(), GetForegroundWindow());
            std::thread::sleep(Duration::from_millis(20));
        }
        send(edit, EM_REPLACESEL, 1, wide("needle").as_ptr() as isize);
        assert_eq!(remote_text(edit), "needle");
        wait_until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 2);
        println!("hide-and-show: search box focused, old query fully selected and replaced");

        // 搜索成功后手动清空：应立即得到空列表，进程继续响应。
        replace_text(edit, "");
        wait_until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
        assert!(running.0.try_wait().unwrap().is_none());
        println!("clear-after-search: empty results without a background match-all passed");

        // 真实主菜单打开设置，只改线程数：保存到隔离目录，且不得重建索引。
        let index_before = std::fs::read(data.join("index.bin")).unwrap();
        PostMessageW(window, WM_COMMAND, IDM_SETTINGS, 0);
        let mut settings_window = null_mut();
        wait_until(|| {
            settings_window = FindWindowW(wide("AnythingSettingsWindow").as_ptr(), null());
            !settings_window.is_null()
        });
        let mut threads = null_mut();
        wait_until(|| {
            threads = GetDlgItem(settings_window, settings::THREADS as i32);
            !threads.is_null()
        });
        replace_text(threads, "3");
        assert_eq!(remote_text(threads), "3");
        SendMessageW(settings_window, WM_COMMAND, settings::SAVE, 0);
        wait_until(|| FindWindowW(wide("AnythingSettingsWindow").as_ptr(), null()).is_null());
        let saved = std::fs::read_to_string(data.join("settings.ini")).unwrap();
        assert!(saved.contains("threads=3\n"), "{saved}");
        assert!(saved.contains(&format!("roots={}\n", data.join("files").display())), "{saved}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(std::fs::read(data.join("index.bin")).unwrap(), index_before, "普通选项保存不应重扫或改写索引");
        assert!(running.0.try_wait().unwrap().is_none());
        println!("settings-window: save applied without rescanning or rewriting the index");

        // 正常退出以保存索引，再以空搜索框启动。索引加载完成通知不能触发全量搜索。
        PostMessageW(window, WM_COMMAND, IDM_EXIT, 0);
        wait_until(|| running.0.try_wait().unwrap().is_some());
        assert!(running.0.wait().unwrap().success());
        running.0 = Command::new(&executable).env("ANYTHING_DATA_DIR", &data).spawn().unwrap();
        window = null_mut();
        wait_until(|| {
            assert!(running.0.try_wait().unwrap().is_none(), "application exited after restart");
            window = FindWindowW(wide(CLASS).as_ptr(), null());
            !window.is_null()
        });
        let mut process_id = 0;
        GetWindowThreadProcessId(window, &mut process_id);
        assert_eq!(process_id, running.0.id());
        wait_until(|| {
            edit = FindWindowExW(window, null_mut(), windows_sys::core::w!("Edit"), null());
            list = FindWindowExW(window, null_mut(), WC_LISTVIEWW, null());
            !edit.is_null() && !list.is_null()
        });
        let ae = std::path::Path::new(&executable).parent().unwrap().join("ae.exe");
        wait_until(|| {
            let out = Command::new(&ae).arg("-status").env("ANYTHING_DATA_DIR", &data).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains("已索引 8 个对象")
        });
        assert_eq!(send(list, LVM_GETITEMCOUNT, 0, 0), 0);
        replace_text(edit, "needle");
        wait_until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 2);
        replace_text(edit, "");
        wait_until(|| send(list, LVM_GETITEMCOUNT, 0, 0) == 0);
        assert!(running.0.try_wait().unwrap().is_none());
        println!("restart-with-saved-index: no automatic empty search; search and clear still work");
    }
}

/// 显式运行的大索引主进程压测；索引和设置均放在隔离目录，不访问真实文件。
#[test]
#[ignore = "explicit stress test with a synthetic one-million-entry saved index"]
fn executable_large_index_search_clear_stress() {
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    struct RunningApp(Child);
    impl Drop for RunningApp {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !condition() {
            assert!(Instant::now() < deadline, "large-index UI test timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn controls(window: HWND) -> (HWND, HWND) {
        let mut edit = null_mut();
        let mut list = null_mut();
        wait_until(|| unsafe {
            edit = FindWindowExW(window, null_mut(), windows_sys::core::w!("Edit"), null());
            list = FindWindowExW(window, null_mut(), WC_LISTVIEWW, null());
            !edit.is_null() && !list.is_null()
        });
        (edit, list)
    }
    fn replace_text(edit: HWND, text: &str) {
        unsafe {
            send(edit, EM_SETSEL, 0, -1);
            send(edit, EM_REPLACESEL, 1, wide(text).as_ptr() as isize);
        }
    }
    fn wait_count(app: &mut Child, list: HWND, expected: usize) -> u128 {
        let start = Instant::now();
        wait_until(|| unsafe {
            assert!(app.try_wait().unwrap().is_none(), "application exited during stress test");
            send(list, LVM_GETITEMCOUNT, 0, 0) == expected as isize
        });
        start.elapsed().as_millis()
    }

    unsafe {
        assert!(FindWindowW(wide(CLASS).as_ptr(), null()).is_null(), "close existing anything instances before stress test");
    }
    let files: usize = std::env::var("ANYTHING_UI_STRESS_ITEMS").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    assert!(files >= 10_000);
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-validation").join(format!("stress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("settings.ini"), "roots=X:\\anything-synthetic-not-real\nexcludes=\nhotkey=Ctrl+Alt+Shift+F11\nrescan_on_start=0\nthreads=2\npinyin=0\n").unwrap();

    const DIRS: usize = 244;
    let started = Instant::now();
    let mut ix = Index::with_capacity(files + DIRS + 1, files * 24);
    let root = ix.push_nomap("Z:", b"z:", None, NONE, F_DIR, 0, 0);
    let mut dirs = Vec::new();
    for d in 0..DIRS {
        let name = format!("dir{d:04}");
        dirs.push(ix.push_nomap(&name, name.as_bytes(), None, root, F_DIR, 0, 0));
    }
    for i in 0..files {
        let stem = if i % 10 == 0 { "document" } else { "file" };
        let name = format!("{stem}{:07}.txt", (i * 7919) % files);
        ix.push_nomap(&name, name.as_bytes(), None, dirs[i % DIRS], 0, 0, 0);
    }
    anything::store::save(&ix, &data.join("index.bin")).unwrap();
    let total = ix.len();
    drop(ix);
    println!("stress setup: {total} entries saved in {} ms", started.elapsed().as_millis());

    let executable = std::env::var_os("ANYTHING_UI_TEST_EXE").expect("set ANYTHING_UI_TEST_EXE to the executable being verified");
    let mut running = RunningApp(Command::new(&executable).env("ANYTHING_DATA_DIR", &data).spawn().unwrap());
    let mut window = null_mut();
    wait_until(|| unsafe {
        window = FindWindowW(wide(CLASS).as_ptr(), null());
        !window.is_null()
    });
    let (mut edit, mut list) = controls(window);
    let ae = std::path::Path::new(&executable).parent().unwrap().join("ae.exe");
    let status = format!("已索引 {} 个对象", anything::util::group_digits(total as u64));
    wait_until(|| {
        let out = Command::new(&ae).arg("-status").env("ANYTHING_DATA_DIR", &data).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&status)
    });
    // 启动排名在后台计算；留出时间后覆盖有序视图路径，回退路径由单元测试覆盖。
    std::thread::sleep(Duration::from_millis(500));

    for (query, expected) in [("file", files * 9 / 10), ("document", files / 10), ("file", files * 9 / 10)] {
        let search_ms = {
            replace_text(edit, query);
            wait_count(&mut running.0, list, expected)
        };
        let clear_ms = {
            replace_text(edit, "");
            wait_count(&mut running.0, list, 0)
        };
        assert!(clear_ms < 5_000, "clearing {expected} results took {clear_ms} ms");
        println!("stress UI: {query:?} -> {expected} results in {search_ms} ms; clear in {clear_ms} ms");
    }

    // 旧搜索尚未完成时清空，取消后旧结果不得重新填回列表。
    replace_text(edit, "file");
    std::thread::sleep(Duration::from_millis(100));
    replace_text(edit, "document");
    replace_text(edit, "");
    wait_count(&mut running.0, list, 0);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(unsafe { send(list, LVM_GETITEMCOUNT, 0, 0) }, 0, "cancelled search repopulated an empty query");
    assert!(running.0.try_wait().unwrap().is_none());
    println!("stress UI: rapid query replacement and cancellation passed");

    // 带保存索引重启后重复大结果搜索和清空。
    unsafe { PostMessageW(window, WM_COMMAND, IDM_EXIT, 0); }
    wait_until(|| running.0.try_wait().unwrap().is_some());
    assert!(running.0.wait().unwrap().success());
    running.0 = Command::new(&executable).env("ANYTHING_DATA_DIR", &data).spawn().unwrap();
    window = null_mut();
    wait_until(|| unsafe {
        window = FindWindowW(wide(CLASS).as_ptr(), null());
        !window.is_null()
    });
    (edit, list) = controls(window);
    wait_until(|| {
        let out = Command::new(&ae).arg("-status").env("ANYTHING_DATA_DIR", &data).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&status)
    });
    std::thread::sleep(Duration::from_millis(500));
    replace_text(edit, "file");
    let restart_search_ms = wait_count(&mut running.0, list, files * 9 / 10);
    replace_text(edit, "");
    let restart_clear_ms = wait_count(&mut running.0, list, 0);
    assert!(restart_clear_ms < 5_000);
    println!("stress restart: search in {restart_search_ms} ms; clear in {restart_clear_ms} ms");

    unsafe { PostMessageW(window, WM_COMMAND, IDM_EXIT, 0); }
    wait_until(|| running.0.try_wait().unwrap().is_some());
    assert!(running.0.wait().unwrap().success());
    std::fs::remove_dir_all(&data).unwrap();
}
