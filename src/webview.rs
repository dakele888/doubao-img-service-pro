// 程序自己的窗口：Win32 外壳 + WebView2 渲染面板。
// 页面内容直接注入内存，不经过本地地址，所以不怕本地端口被拦。
// WebView2Loader.dll 被编进 exe：运行时释放到本地目录再动态加载，对外只有一个文件。
use std::cell::RefCell;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2Environment};
use webview2_com::{CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler};
use windows::core::{Interface, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

type Handle = *mut c_void;

thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static ENV: RefCell<Option<ICoreWebView2Environment>> = const { RefCell::new(None) };
}

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static WEBVIEW_READY: AtomicIsize = AtomicIsize::new(0);

// ---------- 托盘 ----------
const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const ID_OPEN: usize = 1;
const ID_FOLDER: usize = 2;
const ID_EXIT: usize = 3;

static TRAY_OK: AtomicIsize = AtomicIsize::new(0);
static HIDDEN: AtomicIsize = AtomicIsize::new(0);
static TIP_SHOWN: AtomicIsize = AtomicIsize::new(0);

fn fill(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
}

fn tray_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        // 资源 1 是 exe 自带的图标；取不到就用系统默认图标
        LoadIconW(Some(HINSTANCE(hinst.0)), PCWSTR(1 as *const u16))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
}

fn tray_data(hwnd: HWND, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: flags,
        uCallbackMessage: WM_TRAY,
        hIcon: tray_icon(),
        ..Default::default()
    }
}

fn add_tray(hwnd: HWND) {
    unsafe {
        let mut nid = tray_data(hwnd, NIF_ICON | NIF_MESSAGE | NIF_TIP);
        fill(&mut nid.szTip, "豆包生图服务 · 服务运行中，双击打开面板");
        let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        TRAY_OK.store(if ok { 1 } else { 0 }, Ordering::Relaxed);
        crate::util::log(if ok {
            "已驻留系统托盘（关面板不退出服务）"
        } else {
            "托盘图标创建失败，关窗口就直接退出"
        });
    }
}

fn stop_tray(hwnd: HWND) {
    unsafe {
        if TRAY_OK.swap(0, Ordering::Relaxed) == 1 {
            let nid = tray_data(hwnd, NOTIFY_ICON_DATA_FLAGS::default());
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

/// 第一次收进托盘时弹个气泡，免得用户以为程序退了
fn tray_balloon(hwnd: HWND) {
    unsafe {
        let mut nid = tray_data(hwnd, NIF_INFO);
        nid.dwInfoFlags = NIIF_INFO;
        fill(&mut nid.szInfoTitle, "豆包生图服务");
        fill(
            &mut nid.szInfo,
            "面板已收进托盘，服务仍在运行。图标在右下角任务栏的 ∧ 里，可以拖出来固定。",
        );
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn tray_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, PCWSTR(wide("打开面板").as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, ID_FOLDER, PCWSTR(wide("打开缓存目录").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, PCWSTR(wide("退出（停止服务）").as_ptr()));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 不加这一句，菜单点外面不会消失
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

fn hide_to_tray(hwnd: HWND) {
    // 只隐藏窗口，不动 WebView2 的可见性 —— 用 SetIsVisible 关掉后，从托盘恢复
    // 时页面会白屏（WebView2 不会自己重绘），这条路上吃过亏。
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    HIDDEN.store(1, Ordering::Relaxed);
    if TIP_SHOWN.swap(1, Ordering::Relaxed) == 0 {
        tray_balloon(hwnd);
    }
    crate::util::log("面板已收进托盘，服务继续运行");
}

fn show_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    resize(hwnd);
    HIDDEN.store(0, Ordering::Relaxed);
}

fn open_out_dir() {
    let cfg = crate::cfg::Config::load(&crate::config_file());
    let p = crate::out_dir(&cfg);
    let _ = std::fs::create_dir_all(&p);
    let _ = std::process::Command::new("explorer").arg(&p).spawn();
}

fn quit_app(hwnd: HWND) {
    stop_tray(hwnd);
    CONTROLLER.with(|c| unsafe {
        if let Some(ctrl) = c.borrow().as_ref() {
            let _ = ctrl.Close();
        }
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// ---------- 内嵌的 WebView2 加载器 ----------
// WebView2Loader.dll 只有 158KB，直接编进 exe，运行时释放到本地目录再 LoadLibrary。
// 这样对外交付只有一个文件，用户不用管什么 DLL。
const LOADER: &[u8] = include_bytes!("../assets/WebView2Loader.dll");

type CreateEnvFn = unsafe extern "system" fn(PCWSTR, PCWSTR, *mut c_void, *mut c_void) -> HRESULT;

static CREATE_ENV: OnceLock<Option<CreateEnvFn>> = OnceLock::new();

/// 把内嵌的 DLL 落到程序目录下的 .webview\rt\，避免往 C 盘写东西；
/// 只有程序目录不可写（比如放在 Program Files）才回退到 %LOCALAPPDATA%。
fn dump_loader() -> Option<PathBuf> {
    // 用户自己放在 exe 旁边的话，优先用他的
    let beside = crate::base_dir().join("WebView2Loader.dll");
    if beside.is_file() {
        return Some(beside);
    }

    if let Some(p) = put_dll(&crate::base_dir().join(".webview").join("rt")) {
        return Some(p);
    }

    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root
        .join("doubao-img")
        .join("runtime")
        .join(LOADER.len().to_string());
    let p = put_dll(&dir);
    if p.is_none() {
        crate::util::log("WebView2 加载器无处可写（程序目录和 LOCALAPPDATA 都不可写）");
    }
    p
}

/// 把 DLL 写到指定目录，返回最终路径
fn put_dll(dir: &PathBuf) -> Option<PathBuf> {
    let dll = dir.join("WebView2Loader.dll");
    if let Ok(m) = std::fs::metadata(&dll) {
        if m.len() as usize == LOADER.len() {
            return Some(dll);
        }
    }
    std::fs::create_dir_all(dir).ok()?;

    let tmp = dir.join(format!("WebView2Loader.{}.tmp", std::process::id()));
    std::fs::write(&tmp, LOADER).ok()?;
    // 多个实例同时启动时，另一个可能已经写好了；rename 失败不算错
    let _ = std::fs::rename(&tmp, &dll);
    let _ = std::fs::remove_file(&tmp);
    if dll.is_file() {
        Some(dll)
    } else {
        None
    }
}

/// 动态取出 CreateCoreWebView2EnvironmentWithOptions，避免 exe 启动时依赖外部 DLL
fn create_env_fn() -> Option<CreateEnvFn> {
    *CREATE_ENV.get_or_init(|| unsafe {
        let path = match dump_loader() {
            Some(p) => p,
            None => {
                crate::util::log("WebView2 加载器释放失败（本地目录不可写）");
                return None;
            }
        };
        let w = wide(&path.to_string_lossy());
        let h = match LoadLibraryW(PCWSTR(w.as_ptr())) {
            Ok(h) => h,
            Err(e) => {
                crate::util::log(&format!("加载 WebView2 加载器失败：{e}"));
                return None;
            }
        };
        match GetProcAddress(h, PCSTR(b"CreateCoreWebView2EnvironmentWithOptions\0".as_ptr())) {
            Some(f) => Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateEnvFn>(f)),
            None => {
                crate::util::log("WebView2 加载器里找不到入口函数");
                None
            }
        }
    })
}

const CLASS_NAME: &str = "DoubaoImgNative";

pub fn run() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hinst = GetModuleHandleW(PCWSTR::null()).unwrap();
        let cls = wide(CLASS_NAME);

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hbrBackground: HBRUSH(6 as *mut c_void),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(cls.as_ptr()),
            PCWSTR(wide("豆包生图服务").as_ptr()),
            WS_OVERLAPPEDWINDOW & !WS_MAXIMIZEBOX & !WS_THICKFRAME,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            900,
            780,
            None,
            None,
            Some(hinst.into()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };

        if hwnd.0.is_null() {
            return;
        }
        MAIN_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        add_tray(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        crate::util::log("消息循环结束");
    }
}

fn init_webview(hwnd: HWND) {
    unsafe {
        let data_dir = crate::base_dir().join(".webview");
        let _ = std::fs::create_dir_all(&data_dir);
        let dir = wide(&data_dir.to_string_lossy());

        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |hr, env| {
            if hr.is_err() || env.is_none() {
                crate::util::log("WebView2 环境创建失败，可能是没装 WebView2 运行时");
                fallback();
                return Ok(());
            }
            let env = env.unwrap();
            let _ = ENV.with(|e| *e.borrow_mut() = Some(env.clone()));

            let ch = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr2, ctrl| {
                if hr2.is_err() || ctrl.is_none() {
                    crate::util::log("WebView2 控制器创建失败");
                    fallback();
                    return Ok(());
                }
                let ctrl = ctrl.unwrap();
                let c = ctrl.clone();
                let _ = CONTROLLER.with(|x| *x.borrow_mut() = Some(c));
                resize(hwnd);
                let _ = ctrl.SetIsVisible(true);

                if let Ok(wv) = ctrl.CoreWebView2() {
                    let html = crate::panel_html();
                    // 直接注入页面内容：界面本身不经过任何地址
                    let h = wide(&html);
                    let _ = wv.NavigateToString(PCWSTR(h.as_ptr()));
                }
                WEBVIEW_READY.store(1, Ordering::Relaxed);
                crate::util::log("WebView2 就绪，页面已注入");
                Ok(())
            }));

            let _ = env.CreateCoreWebView2Controller(HWND(hwnd.0), &ch);
            Ok(())
        }));

        let create_env = match create_env_fn() {
            Some(f) => f,
            None => {
                fallback();
                return;
            }
        };
        let r = create_env(
            PCWSTR::null(),
            PCWSTR(dir.as_ptr()),
            null_mut(),
            handler.as_raw(),
        );
        if r.is_err() {
            crate::util::log(&format!("创建 WebView2 环境失败（0x{:08X}），回退到浏览器窗口", r.0 as u32));
            fallback();
        }
    }
}

/// 没有 WebView2 运行时的时候，退回浏览器窗口，至少还能用
fn fallback() {
    crate::open_browser_window();
}

fn resize(hwnd: HWND) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let _ = CONTROLLER.with(|c| {
            if let Some(ctrl) = c.borrow().as_ref() {
                let _ = ctrl.SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: rc.right,
                    bottom: rc.bottom,
                });
            }
        });
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            init_webview(h);
            LRESULT(0)
        }
        WM_SIZE => {
            resize(h);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = 700;
            mmi.ptMinTrackSize.y = 560;
            LRESULT(0)
        }
        WM_CLOSE => {
            // 有托盘就收进去，服务继续跑；没有托盘才真的退出
            if TRAY_OK.load(Ordering::Relaxed) == 1 {
                if HIDDEN.load(Ordering::Relaxed) == 0 {
                    hide_to_tray(h);
                }
            } else {
                quit_app(h);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as usize {
                ID_OPEN => show_window(h),
                ID_FOLDER => open_out_dir(),
                ID_EXIT => quit_app(h),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY => {
            match lp.0 as u32 {
                WM_LBUTTONUP => show_window(h),
                WM_RBUTTONUP => tray_menu(h),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            stop_tray(h);
            CONTROLLER.with(|c| {
                if let Some(ctrl) = c.borrow().as_ref() {
                    let _ = ctrl.Close();
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub fn bring_to_front() {
    let p = MAIN_HWND.load(Ordering::Relaxed);
    if p != 0 {
        show_window(HWND(p as *mut c_void));
    }
}

/// 退出前把托盘图标摘掉，别在任务栏留个幽灵图标
pub fn remove_tray() {
    let p = MAIN_HWND.load(Ordering::Relaxed);
    if p != 0 {
        stop_tray(HWND(p as *mut c_void));
    }
}

pub fn alive() -> bool {
    MAIN_HWND.load(Ordering::Relaxed) != 0
}

pub fn ready() -> bool {
    WEBVIEW_READY.load(Ordering::Relaxed) == 1
}

#[allow(dead_code)]
fn unused(_: Handle) {
    let _ = null_mut::<c_void>();
}
