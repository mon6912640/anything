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
    }
}
