// 启动密码：不输对就不往下走，服务根本不会监听端口。
// 密码本身不写在 exe 里，只比对加盐 MD5，记事本搜不到明文。
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use md5::{Digest, Md5};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetStockObject, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, HBRUSH, HFONT, HGDIOBJ,
    OPAQUE, TRANSPARENT, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 密码 3.1415926 的加盐 MD5。要换密码，改这一行即可（算法见 README）。
const SALT: &str = "doubao-img-v1";
const PWD_HASH: &str = "be37ca78bf79ca34a1408dbcb2531932";

const ID_EDIT: usize = 1001;
const ID_OK: usize = 1002;
const ID_ERR: usize = 1003;
const ID_HINT: usize = 1004;
const ID_TITLE: usize = 1005;
const ID_CHECK: usize = 1006;
const ID_DOC: usize = 1007;
const ID_PWDLABEL: usize = 1008;

const CW: i32 = 520;
const CH: i32 = 478;

static PASSED: AtomicIsize = AtomicIsize::new(0);
static OLD_EDIT_PROC: AtomicIsize = AtomicIsize::new(0);
static ERR_CTRL: AtomicIsize = AtomicIsize::new(0);
static EDIT_CTRL: AtomicIsize = AtomicIsize::new(0);
static HINT_CTRL: AtomicIsize = AtomicIsize::new(0);
static NEED_PWD: AtomicIsize = AtomicIsize::new(1);
static CHECK_CTRL: AtomicIsize = AtomicIsize::new(0);
static DOC_CTRL: AtomicIsize = AtomicIsize::new(0);
static PWDLABEL_CTRL: AtomicIsize = AtomicIsize::new(0);
static TITLE_FONT: AtomicIsize = AtomicIsize::new(0);
static BODY_FONT: AtomicIsize = AtomicIsize::new(0);
static SUB_FONT: AtomicIsize = AtomicIsize::new(0);

/// 免责声明正文
const DOC: &str = "本软件（豆包生图服务）仅用于个人学习与技术研究，使用前请确认：

一、非官方用途
本软件通过自动化方式操作你已登录的豆包网页版来生成图片，不属于豆包官方接口。豆包页面改版、风控策略调整都可能导致功能失效。

二、内容与合规
生成内容由豆包模型产出，本软件不保证其准确性、可用性与版权归属。请遵守豆包用户协议及所在地法律法规，不得生成违法违规、侵犯他人权益的内容。

三、账号风险
自动化操作存在被平台限流、警告或封号的可能，该风险由使用者自行承担。

四、数据与隐私
所有数据均在本机处理，不上传对话内容；图片默认保存在程序目录，请自行做好备份。

五、责任限制
本软件按「现状」提供，作者不对使用过程中产生的任何直接或间接损失负责。

继续使用即表示你已理解并接受以上全部条款。";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hash_of(pwd: &str) -> String {
    let mut h = Md5::new();
    h.update(format!("{SALT}:{pwd}").as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn check(pwd: &str) -> bool {
    !pwd.is_empty() && hash_of(pwd) == PWD_HASH
}

fn make_font(size: i32, weight: i32) -> HFONT {
    unsafe {
        let face = wide("Microsoft YaHei UI");
        CreateFontW(
            size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        )
    }
}

fn set_font(h: HWND, font: HFONT) {
    unsafe {
        SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

fn ctl(parent: HWND, class: &str, text: &str, id: usize, st: WINDOW_STYLE, ex: WINDOW_EX_STYLE) -> HWND {
    unsafe {
        let c = wide(class);
        let t = wide(text);
        CreateWindowExW(
            ex,
            PCWSTR(c.as_ptr()),
            PCWSTR(t.as_ptr()),
            WS_CHILD | WS_VISIBLE | st,
            0,
            0,
            10,
            10,
            Some(parent),
            Some(HMENU(id as *mut c_void)),
            None,
            None,
        )
        .unwrap_or_default()
    }
}

fn build(hwnd: HWND) {
    unsafe {
        let title = ctl(hwnd, "STATIC", "豆包生图服务", ID_TITLE, WINDOW_STYLE(0), WINDOW_EX_STYLE(0));
        let hint = ctl(
            hwnd,
            "STATIC",
            "使用前请阅读并同意以下声明",
            ID_HINT,
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
        );
        let doc_text = DOC.replace('\n', "\r\n");
        let doc = ctl(
            hwnd,
            "EDIT",
            &doc_text,
            ID_DOC,
            WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32) | WS_VSCROLL | WS_TABSTOP,
            WS_EX_CLIENTEDGE,
        );
        let chk = ctl(
            hwnd,
            "BUTTON",
            "我已阅读并同意上述免责声明",
            ID_CHECK,
            WINDOW_STYLE((BS_AUTOCHECKBOX) as u32) | WS_TABSTOP,
            WINDOW_EX_STYLE(0),
        );
        let pwdlabel = ctl(
            hwnd,
            "STATIC",
            "启动密码",
            ID_PWDLABEL,
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
        );
        let edit = ctl(
            hwnd,
            "EDIT",
            "",
            ID_EDIT,
            WINDOW_STYLE((ES_AUTOHSCROLL | ES_PASSWORD) as u32) | WS_TABSTOP,
            WS_EX_CLIENTEDGE,
        );
        let err = ctl(hwnd, "STATIC", "", ID_ERR, WINDOW_STYLE(0), WINDOW_EX_STYLE(0));
        let btn = ctl(
            hwnd,
            "BUTTON",
            "同意并继续",
            ID_OK,
            WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            WINDOW_EX_STYLE(0),
        );

        MoveWindow(title, 30, 18, 300, 32, true);
        MoveWindow(hint, 30, 52, 460, 22, true);
        MoveWindow(doc, 30, 78, CW - 60, 200, true);
        MoveWindow(chk, 30, 288, CW - 60, 24, true);
        MoveWindow(pwdlabel, 30, 324, CW - 60, 20, true);
        MoveWindow(edit, 30, 344, CW - 60, 30, true);
        MoveWindow(err, 30, 382, CW - 60, 20, true);
        MoveWindow(btn, CW - 30 - 116, 410, 116, 34, true);

        let f_title = make_font(-20, 700);
        let f_body = make_font(-15, 400);
        let f_sub = make_font(-14, 400);
        TITLE_FONT.store(f_title.0 as isize, Ordering::Relaxed);
        BODY_FONT.store(f_body.0 as isize, Ordering::Relaxed);
        SUB_FONT.store(f_sub.0 as isize, Ordering::Relaxed);

        set_font(title, f_title);
        set_font(hint, f_sub);
        set_font(doc, f_sub);
        // 左右各留 8px 内边距，文字不贴边
        SendMessageW(doc, 0x00D3u32, Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)), Some(LPARAM(((8 << 16) | 8) as isize)));
        set_font(chk, f_body);
        set_font(pwdlabel, f_sub);
        set_font(edit, f_body);
        set_font(err, f_sub);
        set_font(btn, f_body);

        EDIT_CTRL.store(edit.0 as isize, Ordering::Relaxed);
        ERR_CTRL.store(err.0 as isize, Ordering::Relaxed);
        HINT_CTRL.store(hint.0 as isize, Ordering::Relaxed);
        CHECK_CTRL.store(chk.0 as isize, Ordering::Relaxed);
        DOC_CTRL.store(doc.0 as isize, Ordering::Relaxed);
        PWDLABEL_CTRL.store(pwdlabel.0 as isize, Ordering::Relaxed);

        // 关掉启动密码时，把输入框藏起来，标签位置改成提示
        if NEED_PWD.load(Ordering::Relaxed) == 0 {
            let _ = ShowWindow(edit, SW_HIDE);
            SetWindowTextW(
                pwdlabel,
                PCWSTR(wide("启动密码已关闭，进入后可在面板顶部重新开启").as_ptr()),
            );
        }

        // 编辑框里按回车等于点「同意并继续」
        let old = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as usize as isize);
        OLD_EDIT_PROC.store(old, Ordering::Relaxed);
    }
}

/// 弹出启动验证窗口。返回 true 表示密码正确。
pub fn unlock(need_pwd: bool) -> bool {
    NEED_PWD.store(if need_pwd { 1 } else { 0 }, Ordering::Relaxed);
    unsafe {
        let hinst = match GetModuleHandleW(PCWSTR::null()) {
            Ok(h) => h,
            Err(_) => return false,
        };
        let cls = wide("DoubaoImgAuth");

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hbrBackground: HBRUSH(6 as *mut c_void), // COLOR_WINDOW + 1，白底
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let mut rc = RECT {
            left: 0,
            top: 0,
            right: CW,
            bottom: CH,
        };
        let _ = AdjustWindowRect(&mut rc, WS_CAPTION | WS_SYSMENU, false);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let title = wide("豆包生图服务 · 启动验证");
        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST,
            PCWSTR(cls.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU | WS_POPUP,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        ) {
            Ok(x) => x,
            Err(_) => return false,
        };

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let edit = EDIT_CTRL.load(Ordering::Relaxed);
        if edit != 0 {
            let _ = SetFocus(Some(HWND(edit as *mut c_void)));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        PASSED.load(Ordering::Relaxed) == 1
    }
}

unsafe extern "system" fn edit_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_KEYDOWN && wp.0 == VK_RETURN.0 as usize {
        if let Ok(parent) = GetParent(h) {
            let _ = PostMessageW(Some(parent), WM_COMMAND, WPARAM(ID_OK), LPARAM(0));
        }
        return LRESULT(0);
    }
    let old = OLD_EDIT_PROC.load(Ordering::Relaxed);
    if old == 0 {
        return DefWindowProcW(h, msg, wp, lp);
    }
    let f: WNDPROC = std::mem::transmute(old);
    CallWindowProcW(f, h, msg, wp, lp)
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            build(h);
            LRESULT(0)
        }
        WM_COMMAND => {
            if (wp.0 & 0xFFFF) == ID_OK {
                let err = HWND(ERR_CTRL.load(Ordering::Relaxed) as *mut c_void);
                let chk = HWND(CHECK_CTRL.load(Ordering::Relaxed) as *mut c_void);

                // 先看有没有勾同意
                let checked = SendMessageW(chk, BM_GETCHECK, None, None).0 == 1;
                if !checked {
                    SetWindowTextW(
                        err,
                        PCWSTR(wide("请先阅读上方声明，并勾选「我已阅读并同意」").as_ptr()),
                    );
                    return LRESULT(0);
                }

                if NEED_PWD.load(Ordering::Relaxed) == 0 {
                    PASSED.store(1, Ordering::Relaxed);
                    let _ = DestroyWindow(h);
                    return LRESULT(0);
                }

                let edit = HWND(EDIT_CTRL.load(Ordering::Relaxed) as *mut c_void);
                // 用 WM_GETTEXT 而不是 GetWindowTextW：密码框上后者不可靠
                let mut buf = [0u16; 128];
                let n = SendMessageW(
                    edit,
                    WM_GETTEXT,
                    Some(WPARAM(buf.len())),
                    Some(LPARAM(buf.as_mut_ptr() as isize)),
                )
                .0 as usize;
                let pwd = String::from_utf16_lossy(&buf[..n.min(128)]);
                if check(&pwd) {
                    PASSED.store(1, Ordering::Relaxed);
                    let _ = DestroyWindow(h);
                } else {
                    crate::util::log("启动验证：密码不正确");
                    SetWindowTextW(err, PCWSTR(wide("密码不对，请重试").as_ptr()));
                    let _ = SetWindowTextW(edit, PCWSTR::null());
                    let _ = SetFocus(Some(edit));
                }
                return LRESULT(0);
            }
            LRESULT(0)
        }
        // 文字标签统一白底、透明背景；副标题灰、错误红
        WM_CTLCOLORSTATIC => {
            let hdc = windows::Win32::Graphics::Gdi::HDC(wp.0 as *mut c_void);
            // 多行只读文本框必须用不透明背景，否则滚动时旧文字不会被擦掉、越滚越花
            if lp.0 as isize == DOC_CTRL.load(Ordering::Relaxed) {
                let _ = SetBkMode(hdc, OPAQUE);
                let _ = SetBkColor(hdc, COLORREF(0x00FFFFFF));
                return LRESULT(GetStockObject(WHITE_BRUSH).0 as isize);
            }
            let _ = SetBkMode(hdc, TRANSPARENT);
            if lp.0 as isize == ERR_CTRL.load(Ordering::Relaxed) {
                let _ = SetTextColor(hdc, COLORREF(0x003030D0)); // BGR，偏红
            } else if lp.0 as isize == HINT_CTRL.load(Ordering::Relaxed) {
                let _ = SetTextColor(hdc, COLORREF(0x00707070)); // 灰
            }
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            for f in [&TITLE_FONT, &BODY_FONT, &SUB_FONT] {
                let v = f.swap(0, Ordering::Relaxed);
                if v != 0 {
                    let _ = DeleteObject(HGDIOBJ(v as *mut c_void));
                }
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}
