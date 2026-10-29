#![windows_subsystem = "windows"]

// 豆包网页版生图 → 本地 OpenAI 兼容接口（Rust 版）
// 功能与 Node 版一致，只是不再背一个 84MB 的运行时。
mod auth;
mod cfg;
mod ego;
mod sched;
mod util;
mod webview;

use cfg::Config;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

pub const PORT_DEFAULT: u16 = 8788;
static PORT_V: OnceLock<u16> = OnceLock::new();

/// 默认 8788，可用环境变量 DOUBAO_PORT 覆盖
pub fn port() -> u16 {
    *PORT_V.get_or_init(|| {
        let from_env = std::env::var("DOUBAO_PORT")
            .ok()
            .and_then(|v| v.trim().parse().ok());
        match from_env {
            Some(p) => p,
            None => {
                // 更新重启走 ShellExecute，环境变量传不过来，只能靠命令行
                let args: Vec<String> = std::env::args().collect();
                args.iter()
                    .position(|a| a == "--port")
                    .and_then(|i| args.get(i + 1))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(PORT_DEFAULT)
            }
        }
    })
}
const HOST: &str = "127.0.0.1";
const GEN_TIMEOUT_MS: u64 = 240_000;
/// 视频生成比图片慢很多，给足时间（10 分钟）
const VIDEO_TIMEOUT_MS: u64 = 600_000;
/// 面板里进入「视频中转」的密码
const VIDEO_PASSWORD: &str = "12345678";
/// 视频解锁状态（进程内，重启后需重输）
static VIDEO_UNLOCKED: AtomicU64 = AtomicU64::new(0);
/// 视频串行锁：同一时间只跑一个，避免抢豆包页面
static VIDEO_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
const MAX_QUEUE: usize = 30;
const QUEUE_WAIT_MS: u64 = 1_800_000;

static BASE: OnceLock<PathBuf> = OnceLock::new();
static LAST_PING: AtomicU64 = AtomicU64::new(0);
static HAD_PING: AtomicU64 = AtomicU64::new(0);

pub fn base_dir() -> PathBuf {
    BASE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|x| x.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    })
    .clone()
}

pub fn config_file() -> PathBuf {
    base_dir().join("config.json")
}

pub fn out_dir(cfg: &Config) -> PathBuf {
    if cfg.out_dir.trim().is_empty() {
        base_dir().join("out")
    } else {
        PathBuf::from(cfg.out_dir.trim())
    }
}

fn main() {
    // panic 在 GUI 程序里没法看到，统一写进日志
    std::panic::set_hook(Box::new(|info| {
        crate::util::log(&format!("panic: {info}"));
    }));

    util::log("========== 豆包生图服务启动 ==========");

    let addr = format!("{}:{}", HOST, port());

    // 更新后重启：旧进程还没退出，先等它把端口放出来，别去唤醒它
    let args: Vec<String> = std::env::args().collect();
    let has_arg = |f: &str| args.iter().any(|a| a == f);
    let restarting = std::env::var("DOUBAO_RESTART").map(|v| v.trim() == "1").unwrap_or(false) || has_arg("--restart");

    if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(600)).is_ok() {
        if restarting {
            for _ in 0..60 {
                if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(300)).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(300));
            }
        } else {
            util::log("检测到已有实例，唤醒它的窗口");
            poke("/api/show");
            return;
        }
    }

    let cfg = Config::load(&config_file());

    // 仅供自动化测试 / 无界面部署：设置了 DOUBAO_HEADLESS=1 就不弹验证窗、不建窗口
    let headless = std::env::var("DOUBAO_HEADLESS").map(|v| v.trim() == "1").unwrap_or(false)
        || has_arg("--headless");
    // 更新重启时用：不重复弹免责声明和密码
    let trusted = std::env::var("DOUBAO_SKIP_AUTH").map(|v| v.trim() == "1").unwrap_or(false)
        || has_arg("--trusted");

    // 免责声明每次都显示；启动密码可以在面板里关掉
    if !headless && !trusted && !auth::unlock(cfg.require_password) {
        util::log("启动验证未通过，退出");
        return;
    }
    if trusted {
        util::log("更新后自动重启，已跳过启动验证");
    }

    // 上一次更新留下的备份，能删就删
    if restarting || trusted {
        if let Ok(entries) = std::fs::read_dir(base_dir()) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.ends_with(".old.exe") || n == "_update.exe" {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    let od = out_dir(&cfg);
    let _ = std::fs::create_dir_all(&od);
    util::log(&format!("界面：原生窗口 · 接口 http://{}:{}/", HOST, port()));
    util::log(&format!(
        "专用对话 {} · 缓存 {} · 直传 {}",
        cfg.conversation_name,
        od.display(),
        if cfg.direct_mode { "开" } else { "关" }
    ));
    let bin = util::find_ego(&cfg);
    util::log(&format!("ego-browser {}", bin));
    if !util::ego_available(&bin) {
        util::log("警告：ego-browser 不可用，生图会失败");
    }



    // 服务常驻：面板可以关进托盘，不再按心跳自动退出
    thread::spawn(serve);
    if headless {
        util::log("无界面模式运行中（DOUBAO_HEADLESS=1）");
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    webview::run();
    util::log("窗口已关闭，退出");
}

fn serve() {
    let addr = format!("{}:{}", HOST, port());
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            util::log(&format!("端口 {} 绑定失败：{}", port(), e));
            return;
        }
    };
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                thread::spawn(move || {
                    // 单个请求出问题不能把整个服务带走
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(s)));
                    if r.is_err() {
                        util::log("[崩溃] 处理请求时发生 panic，已隔离");
                    }
                });
            }
            Err(_) => continue,
        }
    }
}

/// 启动第二个实例时，用它唤醒已有窗口
fn poke(path: &str) {
    if let Ok(mut s) = TcpStream::connect((HOST, port())) {
        let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
        let _ = s.write_all(format!("GET {} HTTP/1.0\r\nHost: {}\r\n\r\n", path, HOST).as_bytes());
        let mut buf = [0u8; 64];
        let _ = s.read(&mut buf);
    }
}

pub fn app_quit() {
    std::process::exit(0);
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(h: *mut std::ffi::c_void, text: *const u16, cap: *const u16, ty: u32) -> i32;
}

/// 用 Chromium 的 --app 模式开一个无地址栏的独立窗口。
/// 按 Edge → Chrome → 360 顺序试；哪个启动后立刻退出（说明被策略拦了）就换下一个。
pub fn open_browser_window() {
    let url = format!("http://{}:{}/", HOST, port());
    let profile = base_dir().join(".win");
    let _ = std::fs::create_dir_all(&profile);

    for bin in util::browser_candidates() {
        util::log(&format!("打开窗口：{}", bin.display()));
        match Command::new(&bin)
            .arg(format!("--app={}", url))
            .arg("--window-size=880,760")
            .arg(format!("--user-data-dir={}", profile.display()))
            .spawn()
        {
            Ok(mut c) => {
                thread::sleep(Duration::from_millis(1000));
                if c.try_wait().ok().flatten().is_none() {
                    util::log("窗口已打开");
                    return;
                }
                util::log("  这个浏览器启动后立刻退出，换下一个");
            }
            Err(e) => util::log(&format!("  启动失败：{}", e)),
        }
    }

    util::log("没有可用的应用窗口，改用默认浏览器");
    util::open_url(&url);
    unsafe {
        let msg = to_wide(&format!(
            "面板没能自动打开。\n\n服务已经在运行，用浏览器访问这个地址就能看到：\n{}\n\n（不影响魔哈调用生图）",
            url
        ));
        MessageBoxW(
            std::ptr::null_mut(),
            msg.as_ptr(),
            to_wide("豆包生图服务").as_ptr(),
            0x40,
        );
    }
}
// ---------------- HTTP ----------------

const MAX_BODY: usize = 64 * 1024 * 1024;

struct Req {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    ctype: String,
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let peer = stream.try_clone();
    let mut reader = match peer {
        Ok(s) => BufReader::new(s),
        Err(_) => return,
    };

    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut it = line.trim().split(' ');
    let method = it.next().unwrap_or("").to_string();
    let full = it.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return;
    }
    let path = full.split('?').next().unwrap_or("/").to_string();
    let query = full
        .splitn(2, '?')
        .nth(1)
        .unwrap_or("")
        .to_string();

    let mut content_length = 0usize;
    let mut ctype = String::new();
    let mut chunked = false;
    let mut expect_continue = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() {
            return;
        }
        let t = h.trim();
        if t.is_empty() {
            break;
        }
        let low = t.to_lowercase();
        if low.starts_with("content-length:") {
            content_length = low[15..].trim().parse().unwrap_or(0);
        } else if low.starts_with("content-type:") {
            ctype = t[13..].trim().to_string();
        } else if low.starts_with("transfer-encoding:") && low.contains("chunked") {
            chunked = true;
        } else if low.starts_with("expect:") && low.contains("100-continue") {
            expect_continue = true;
        }
    }

    // 有的客户端（Go / Python requests）传文件前会等一个 100 再发 body
    if expect_continue {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = stream.flush();
    }

    let body = if chunked {
        read_chunked(&mut reader)
    } else {
        read_fixed(&mut reader, content_length)
    };

    let req = Req { method, path, query, body, ctype };
    route(&mut stream, req);
}

fn read_fixed(reader: &mut BufReader<TcpStream>, n: usize) -> Vec<u8> {
    let mut left = n.min(MAX_BODY);
    let mut body = Vec::with_capacity(left.min(4 * 1024 * 1024));
    let mut buf = [0u8; 16384];
    while left > 0 {
        let want = left.min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(k) => {
                body.extend_from_slice(&buf[..k]);
                left -= k;
            }
            Err(_) => break,
        }
    }
    body
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        let size = usize::from_str_radix(s.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        if size == 0 || body.len() + size > MAX_BODY {
            break;
        }
        let mut chunk = vec![0u8; size];
        if reader.read_exact(&mut chunk).is_err() {
            break;
        }
        body.extend_from_slice(&chunk);
        let mut crlf = String::new();
        let _ = reader.read_line(&mut crlf);
    }
    body
}

fn route(stream: &mut TcpStream, req: Req) {
    let m = req.method.as_str();
    let p = req.path.as_str();

    if m == "OPTIONS" {
        return resp(stream, 204, "text/plain", b"");
    }

    if m == "GET" && (p == "/" || p == "/panel") {
        return resp(stream, 200, "text/html; charset=utf-8", PANEL.as_bytes());
    }

    if m == "GET" && p == "/api/show" {
        webview::bring_to_front();
        return json_resp(stream, 200, &json!({ "ok": true }));
    }

    if m == "GET" && p == "/api/ping" {
        LAST_PING.store(util::now_ms(), Ordering::Relaxed);
        HAD_PING.store(1, Ordering::Relaxed);
        return json_resp(stream, 200, &json!({ "ok": true }));
    }

    if m == "GET" && p == "/health" {
        let cfg = Config::load(&config_file());
        let (count, bytes) = util::stats(&out_dir(&cfg));
        return json_resp(
            stream,
            200,
            &json!({
                "ok": true,
                "service": "doubao-web-image",
                "space": ego::SPACE,
                "conversation": cfg.conversation_name,
                "cached": { "count": count, "bytes": bytes },
            }),
        );
    }

    if m == "GET" && p == "/api/queue" {
        return json_resp(stream, 200, &sched::snapshot());
    }

    if m == "GET" && p == "/api/config" {
        let cfg = Config::load(&config_file());
        let od = out_dir(&cfg);
        let (count, bytes) = util::stats(&od);
        let images: Vec<Value> = util::list_images(&od)
            .iter()
            .take(200)
            .map(|i| json!({ "name": i.name, "size": i.size, "mtime": i.mtime }))
            .collect();
        let bin = util::find_ego(&cfg);
        let ego_ok = util::ego_available(&bin);
        let mut q = sched::snapshot();
        if let Some(o) = q.as_object_mut() {
            o.insert("maxQueue".into(), json!(MAX_QUEUE));
        }
        return json_resp(
            stream,
            200,
            &json!({
                "cfg": cfg.to_value(),
                "baseDir": base_dir().display().to_string(),
                "outDir": od.display().to_string(),
                "stat": { "count": count, "bytes": bytes },
                "images": images,
                "ego": ego_ok,
                "egoBin": bin,
                "queue": q,
            }),
        );
    }

    if m == "POST" && p == "/api/config" {
        let v: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return json_resp(stream, 400, &json!({ "ok": false, "error": "请求体不是合法 JSON" })),
        };
        let mut cfg = Config::load(&config_file());
        if let Some(x) = v.get("conversationName").and_then(|x| x.as_str()) {
            cfg.conversation_name = x.trim().chars().take(40).collect();
        }
        if let Some(x) = v.get("conversationUrl").and_then(|x| x.as_str()) {
            cfg.conversation_url = x.trim().chars().take(300).collect();
        }
        if let Some(x) = v.get("pageLabel").and_then(|x| x.as_str()) {
            cfg.page_label = if x.is_empty() { "p2".into() } else { x.chars().take(20).collect() };
        }
        if let Some(x) = v.get("outDir").and_then(|x| x.as_str()) {
            let d = x.trim().to_string();
            if !d.is_empty() && std::fs::create_dir_all(&d).is_err() {
                return json_resp(stream, 400, &json!({ "ok": false, "error": "目录不可用" }));
            }
            cfg.out_dir = d;
        }
        if let Some(x) = v.get("directMode").and_then(|x| x.as_bool()) {
            cfg.direct_mode = x;
        }
        for (k, field) in [("maxFiles", 0), ("maxMB", 1), ("keepDays", 2)] {
            if let Some(x) = v.get(k).and_then(|x| x.as_u64()) {
                match field {
                    0 => cfg.max_files = x,
                    1 => cfg.max_mb = x,
                    _ => cfg.keep_days = x,
                }
            }
        }
        if let Some(x) = v.get("egoPath").and_then(|x| x.as_str()) {
            let p2 = x.trim().to_string();
            if !p2.is_empty() && !PathBuf::from(&p2).exists() {
                return json_resp(stream, 400, &json!({ "ok": false, "error": format!("这个路径不存在：{}", p2) }));
            }
            cfg.ego_path = p2;
        }
        if let Some(x) = v.get("requirePassword").and_then(|x| x.as_bool()) {
            cfg.require_password = x;
        }
        if let Some(x) = v.get("imageTier").and_then(|x| x.as_str()) {
            let t = x.trim();
            if ["ori", "raw", "thumb"].contains(&t) {
                cfg.image_tier = t.to_string();
            } else {
                return json_resp(
                    stream,
                    400,
                    &json!({ "ok": false, "error": "imageTier 只能是 ori / raw / thumb".to_string() }),
                );
            }
        } else if let Some(x) = v.get("rawImage").and_then(|x| x.as_bool()) {
            // 兼容旧面板/脚本
            cfg.image_tier = if x { "raw".into() } else { "ori".into() };
        }
        if let Some(x) = v.get("videoConversationName").and_then(|x| x.as_str()) {
            let t = x.trim();
            cfg.video_conversation_name = if t.is_empty() { "视频专用".into() } else { t.chars().take(40).collect() };
        }
        cfg.save(&config_file());
        let od = out_dir(&cfg);
        let _ = std::fs::create_dir_all(&od);
        return json_resp(
            stream,
            200,
            &json!({ "ok": true, "cfg": cfg.to_value(), "outDir": od.display().to_string() }),
        );
    }

    if m == "GET" && p == "/api/checkego" {
        let cfg = Config::load(&config_file());
        let bin = util::find_ego(&cfg);
        let ok = util::ego_available(&bin);
        return json_resp(stream, 200, &json!({ "ok": ok, "bin": bin }));
    }

    if m == "GET" && p == "/api/images" {
        let cfg = Config::load(&config_file());
        let od = out_dir(&cfg);
        let (count, bytes) = util::stats(&od);
        let images: Vec<Value> = util::list_images(&od)
            .iter()
            .map(|i| json!({ "name": i.name, "size": i.size, "mtime": i.mtime }))
            .collect();
        return json_resp(stream, 200, &json!({ "outDir": od.display().to_string(), "stat": {"count": count, "bytes": bytes}, "images": images }));
    }

    if m == "POST" && p == "/api/delete" {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let safe = std::path::Path::new(name)
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        if safe.is_empty() {
            return json_resp(stream, 400, &json!({ "ok": false }));
        }
        let cfg = Config::load(&config_file());
        let _ = std::fs::remove_file(out_dir(&cfg).join(&safe));
        return json_resp(stream, 200, &json!({ "ok": true }));
    }

    if m == "POST" && p == "/api/cleanup" {
        let cfg = Config::load(&config_file());
        let od = out_dir(&cfg);
        util::auto_clean(&od, &cfg);
        let (count, bytes) = util::stats(&od);
        return json_resp(stream, 200, &json!({ "ok": true, "stat": {"count": count, "bytes": bytes}, "images": util::list_images(&od).iter().map(|i| json!({"name": i.name, "size": i.size, "mtime": i.mtime})).collect::<Vec<_>>() }));
    }

    if m == "POST" && p == "/api/clear" {
        let cfg = Config::load(&config_file());
        let n = util::clear_all(&out_dir(&cfg));
        return json_resp(stream, 200, &json!({ "ok": true, "removed": n }));
    }

    // 用系统默认浏览器打开 ego 官网文档（URL 写死，不接受外部传入）
    // 手动点“检查更新”带 ?force=1，跳过 10 分钟缓存立刻查 Gitee
    if m == "GET" && p == "/api/version" {
        let force = req.query.split('&').any(|kv| kv == "force=1");
        let current = env!("CARGO_PKG_VERSION").to_string();
        let (rel, err) = latest_release(force);
        let has = rel.as_ref().map(|r| is_newer(&r.tag, &current)).unwrap_or(false);
        return json_resp(
            stream,
            200,
            &json!({
                "current": current,
                "latest": rel.as_ref().map(|r| r.tag.clone()),
                "hasUpdate": has,
                // checked=false 表示根本没查到，前端不能当成“已是最新”
                "checked": err.is_none(),
                "error": err.unwrap_or_default(),
                "downloadUrl": rel.as_ref().map(|r| r.url.clone()).unwrap_or_default(),
                "url": "https://gitee.com/super88888/doubao-img-service/releases",
            }),
        );
    }

    // 一键更新：改成异步，接口立刻返回，下载/校验/替换的进度走 /api/update/status
    if m == "POST" && p == "/api/update" {
        if upd_running() {
            return json_resp(stream, 200, &json!({ "ok": true, "started": true, "running": true }));
        }
        if let Err(e) = upd_start() {
            util::log(&format!("[更新] 启动失败：{}", e));
            return json_resp(stream, 200, &json!({ "ok": false, "error": { "message": e } }));
        }
        return json_resp(stream, 200, &json!({ "ok": true, "started": true }));
    }

    // 更新进度：前端覆盖层轮询这个接口
    if m == "GET" && p == "/api/update/status" {
        let s = upd_status();
        return json_resp(stream, 200, &json!({ "ok": true, "stage": s.0, "msg": s.1 }));
    }

    if m == "POST" && p == "/api/release" {
        const REL: &str = "https://gitee.com/super88888/doubao-img-service/releases";
        let ok = Command::new("cmd")
            .args(["/c", "start", "", REL])
            .spawn()
            .is_ok();
        return json_resp(stream, 200, &json!({ "ok": ok }));
    }

    if m == "POST" && p == "/api/ego" {
        const EGO_DOC: &str = "https://lite.ego.app/document/zh/docs/windows/product-introduce";
        let ok = Command::new("cmd")
            .args(["/c", "start", "", EGO_DOC])
            .spawn()
            .is_ok();
        return json_resp(stream, 200, &json!({ "ok": ok }));
    }

    if m == "POST" && p == "/api/reveal" {
        let cfg = Config::load(&config_file());
        let od = out_dir(&cfg);
        let _ = std::fs::create_dir_all(&od);
        let _ = Command::new("explorer").arg(od.display().to_string()).spawn();
        return json_resp(stream, 200, &json!({ "ok": true, "dir": od.display().to_string() }));
    }

    if m == "GET" && p.starts_with("/files/") {
        let name = std::path::Path::new(&p["/files/".len()..])
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let cfg = Config::load(&config_file());
        let file = out_dir(&cfg).join(&name);
        return match std::fs::read(&file) {
            Ok(buf) => {
                let low = name.to_lowercase();
                let ct = if low.ends_with(".png") {
                    "image/png"
                } else if low.ends_with(".webp") {
                    "image/webp"
                } else if low.ends_with(".mp4") {
                    "video/mp4"
                } else if low.ends_with(".webm") {
                    "video/webm"
                } else {
                    "image/jpeg"
                };
                resp(stream, 200, ct, &buf)
            }
            Err(_) => json_resp(stream, 404, &json!({ "error": "not found" })),
        };
    }

    if m == "POST" && (p == "/v1/images/generations" || p == "/v1/images/edits") {
        return generate(stream, &req);
    }

    // 视频中转：面板输密码解锁后才会显示入口
    if m == "POST" && p == "/api/video/unlock" {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
        let pw = v.get("password").and_then(|x| x.as_str()).unwrap_or("");
        if pw == VIDEO_PASSWORD {
            VIDEO_UNLOCKED.store(1, Ordering::Relaxed);
            util::log("[视频] 面板已解锁视频中转");
            return json_resp(stream, 200, &json!({ "ok": true }));
        }
        util::log("[视频] 面板解锁密码不对");
        return json_resp(stream, 200, &json!({ "ok": false, "error": "密码不对" }));
    }

    if m == "GET" && p == "/api/video/status" {
        return json_resp(
            stream,
            200,
            &json!({
                "ok": true,
                "unlocked": VIDEO_UNLOCKED.load(Ordering::Relaxed) == 1,
                "endpoint": format!("http://{}:{}/v1/videos/generations", HOST, port()),
            }),
        );
    }

    if m == "POST" && p == "/v1/videos/generations" {
        return generate_video(stream, &req);
    }

    // OpenAI 通用视频接口（异步任务制）
    if m == "POST" && p == "/v1/videos" {
        return video_create(stream, &req);
    }
    if m == "GET" && p == "/v1/videos" {
        return video_list(stream);
    }
    if let Some(rest) = p.strip_prefix("/v1/videos/") {
        if rest != "generations" && !rest.is_empty() {
            if let Some(id) = rest.strip_suffix("/content") {
                if m == "GET" {
                    return video_content(stream, id);
                }
            } else if m == "GET" {
                return video_get(stream, rest);
            } else if m == "DELETE" {
                return video_delete(stream, rest);
            }
        }
    }

    json_resp(stream, 404, &json!({ "error": "not found" }))
}

fn generate(stream: &mut TcpStream, req: &Req) {
    let low = req.ctype.to_lowercase();
    let is_json_body = req.body.first().map(|b| *b == b'{' || *b == b'[').unwrap_or(false);

    util::log(&format!(
        "[请求] {} {} · {}B · {}",
        req.method,
        req.path,
        req.body.len(),
        if req.ctype.is_empty() { "(无 content-type)".to_string() } else { req.ctype.clone() }
    ));

    let parsed = if low.contains("multipart/form-data") {
        parse_multipart_req(&req.body, &req.ctype)
    } else if low.contains("x-www-form-urlencoded") {
        parse_form_req(&req.body)
    } else if low.contains("json") || is_json_body || req.body.is_empty() {
        parse_json_req(&req.body)
    } else {
        // 没写 content-type：先按 JSON 试，再按 multipart 试
        parse_json_req(&req.body).or_else(|_| parse_multipart_req(&req.body, &req.ctype))
    };

    let (prompt, want_b64, refs) = match parsed {
        Ok(x) => x,
        Err(e) => {
            util::log(&format!("[请求] 解析失败：{} · 内容开头：{}", e, brief(&req.body)));
            return json_resp(
                stream,
                400,
                &json!({ "error": { "message": e, "type": "invalid_request_error" } }),
            );
        }
    };

    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return json_resp(
            stream,
            400,
            &json!({ "error": { "message": "prompt 不能为空", "type": "invalid_request_error" } }),
        );
    }

    // 请求里提到了图片却没认出参考图：把结构记下来，方便下次排查
    if refs.is_empty() && !req.body.is_empty() {
        let head = String::from_utf8_lossy(&req.body[..req.body.len().min(4096)]);
        if head.contains("image") || head.contains("base64") || head.contains("Image") {
            util::log(&format!("[请求] 没认出参考图，请求结构：{}", body_shape(&req.body)));
        }
    }

    let ticket = match sched::enqueue(&prompt, MAX_QUEUE) {
        Ok(t) => t,
        Err(_) => {
            return json_resp(
                stream,
                429,
                &json!({ "error": { "message": format!("排队已满（{} 个），请稍后重试", MAX_QUEUE), "code": "QUEUE_FULL" } }),
            )
        }
    };
    if ticket.ahead > 0 {
        util::log(&format!("[队列] #{} 入队，前面 {} 个", ticket.seq, ticket.ahead));
    } else {
        util::log(&format!(
            "[生图] 收到{}：{}",
            if refs.is_empty() { String::new() } else { format!("（参考图 {} 张）", refs.len()) },
            prompt.chars().take(40).collect::<String>()
        ));
    }

    if sched::acquire(ticket.seq, QUEUE_WAIT_MS).is_err() {
        util::log("[队列] 等待超时，放弃");
        return json_resp(
            stream,
            504,
            &json!({ "error": { "message": "排队等待超时，已放弃", "code": "QUEUE_TIMEOUT" } }),
        );
    }

    let started = util::now_ms();
    // 不管中间怎么失败（包括 panic），队列槽位都要还回去，否则后面的请求会一直等
    let mut slot = SlotGuard { released: false };
    let outcome = produce_image(&prompt, &refs);
    let cost = ((util::now_ms().saturating_sub(started)) as f64) / 1000.0;

    match outcome {
        Ok(res) => {
            sched::release(true, cost);
            slot.released = true;
            util::log(&format!(
                "[生图] 完成（{:.1}s）{}",
                cost,
                if res.direct {
                    "直传模式，未落盘".to_string()
                } else {
                    format!("{} {}KB", res.name, res.bytes / 1024)
                }
            ));
            let public_url = if res.direct {
                res.src_url.clone()
            } else {
                format!("http://{}:{}/files/{}", HOST, port(), res.name)
            };
            let mut data = json!({ "url": public_url });
            if want_b64 {
                let bytes = if res.direct {
                    util::download(&res.src_url, &base_dir().join("_tmp_dl")).ok().and_then(|_| {
                        std::fs::read(base_dir().join("_tmp_dl")).ok()
                    })
                } else {
                    std::fs::read(&res.file).ok()
                };
                if let Some(b) = bytes {
                    data["b64_json"] = json!(b64(&b));
                }
            }
            json_resp(
                stream,
                200,
                &json!({
                    "created": util::now_ms() / 1000,
                    "data": [data],
                    "elapsed": cost,
                }),
            )
        }
        Err(f) => {
            sched::release(false, cost);
            slot.released = true;
            util::log(&format!("[生图] 失败（{:.1}s）：{}", cost, f.message));
            let code = match f.code.as_str() {
                "QUEUE_FULL" => 429,
                "QUEUE_TIMEOUT" => 504,
                "NOT_LOGGED_IN" => 401,
                _ => 502,
            };
            json_resp(
                stream,
                code,
                &json!({ "error": { "message": f.message, "code": f.code } }),
            )
        }
    }
}

// ---------------- OpenAI 通用视频接口（异步任务制） ----------------
// POST   /v1/videos              创建任务 → 返回 job（status=queued）
// GET    /v1/videos/{id}         查询任务状态
// GET    /v1/videos/{id}/content 下载视频文件
// GET    /v1/videos              列出任务
// DELETE /v1/videos/{id}         删除任务记录

#[derive(Clone)]
struct VideoJob {
    id: String,
    created_at: i64,
    status: String,
    progress: u8,
    seconds: String,
    size: String,
    model: String,
    prompt: String,
    url: String,
    error: Option<String>,
}

impl VideoJob {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "object": "video",
            "created_at": self.created_at,
            "status": self.status,
            "progress": self.progress,
            "seconds": self.seconds,
            "size": self.size,
            "model": self.model,
            "prompt": self.prompt,
            "url": self.url,
            "error": self.error,
        })
    }
}

static VIDEO_JOBS: OnceLock<std::sync::Mutex<Vec<VideoJob>>> = OnceLock::new();
static VIDEO_SEQ: AtomicU64 = AtomicU64::new(1);

fn video_jobs() -> &'static std::sync::Mutex<Vec<VideoJob>> {
    VIDEO_JOBS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn video_job_update<F: FnOnce(&mut VideoJob)>(id: &str, f: F) {
    if let Ok(mut g) = video_jobs().lock() {
        if let Some(j) = g.iter_mut().find(|x| x.id == id) {
            f(j);
        }
    }
}

fn video_job_get(id: &str) -> Option<VideoJob> {
    video_jobs().lock().ok().and_then(|g| g.iter().find(|x| x.id == id).cloned())
}

fn video_create(stream: &mut TcpStream, req: &Req) {
    let low = req.ctype.to_lowercase();
    let is_json_body = req.body.first().map(|b| *b == b'{').unwrap_or(false);

    let parsed = if low.contains("multipart/form-data") {
        parse_multipart_req(&req.body, &req.ctype)
    } else if low.contains("json") || is_json_body || req.body.is_empty() {
        parse_json_req(&req.body)
    } else {
        parse_json_req(&req.body).or_else(|_| parse_multipart_req(&req.body, &req.ctype))
    };
    let (prompt, _want_b64, refs) = match parsed {
        Ok(x) => x,
        Err(e) => {
            return json_resp(
                stream,
                400,
                &json!({ "error": { "message": e, "type": "invalid_request_error" } }),
            )
        }
    };
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return json_resp(
            stream,
            400,
            &json!({ "error": { "message": "prompt 不能为空", "type": "invalid_request_error" } }),
        );
    }

    // 可选参数原样回显（不真正控制豆包，只是为了符合 OpenAI 的响应结构）
    let (size, seconds, model) = if is_json_body {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
        let sec = match v.get("seconds") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => "8".to_string(),
        };
        (
            v.get("size").and_then(|x| x.as_str()).unwrap_or("1280x720").to_string(),
            sec,
            v.get("model").and_then(|x| x.as_str()).unwrap_or("doubao-video").to_string(),
        )
    } else {
        ("1280x720".to_string(), "8".to_string(), "doubao-video".to_string())
    };

    let seq = VIDEO_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("video_{}_{}", util::now_ms(), seq);
    let job = VideoJob {
        id: id.clone(),
        created_at: (util::now_ms() / 1000) as i64,
        status: "queued".into(),
        progress: 0,
        seconds,
        size,
        model,
        prompt: prompt.clone(),
        url: String::new(),
        error: None,
    };
    if let Ok(mut g) = video_jobs().lock() {
        // 只留最近 50 个，避免内存无限涨
        if g.len() >= 50 {
            g.remove(0);
        }
        g.push(job.clone());
    }

    util::log(&format!(
        "[视频] 建任务 {}：{}（参考图 {} 张）",
        id,
        prompt.chars().take(30).collect::<String>(),
        refs.len()
    ));

    // 后台跑，立刻返回 queued
    let id2 = id.clone();
    thread::spawn(move || {
        video_job_update(&id2, |j| {
            j.status = "in_progress".into();
            j.progress = 10;
        });
        let _guard = VIDEO_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let started = util::now_ms();
        match produce_video(&prompt, &refs) {
            Ok(res) => {
                let cost = ((util::now_ms().saturating_sub(started)) as f64) / 1000.0;
                util::log(&format!("[视频] 任务 {} 完成（{:.1}s）{}", id2, cost, res.name));
                let url = format!("http://{}:{}/files/{}", HOST, port(), res.name);
                video_job_update(&id2, |j| {
                    j.status = "completed".into();
                    j.progress = 100;
                    j.url = url;
                });
            }
            Err(f) => {
                let cost = ((util::now_ms().saturating_sub(started)) as f64) / 1000.0;
                util::log(&format!("[视频] 任务 {} 失败（{:.1}s）：{}", id2, cost, f.message));
                video_job_update(&id2, |j| {
                    j.status = "failed".into();
                    j.error = Some(f.message);
                });
            }
        }
    });

    json_resp(stream, 200, &job.to_json())
}

fn video_get(stream: &mut TcpStream, id: &str) {
    match video_job_get(id) {
        Some(j) => json_resp(stream, 200, &j.to_json()),
        None => json_resp(
            stream,
            404,
            &json!({ "error": { "message": format!("没有这个视频任务：{}", id), "code": "NOT_FOUND" } }),
        ),
    }
}

fn video_content(stream: &mut TcpStream, id: &str) {
    let job = match video_job_get(id) {
        Some(j) => j,
        None => {
            return json_resp(
                stream,
                404,
                &json!({ "error": { "message": format!("没有这个视频任务：{}", id), "code": "NOT_FOUND" } }),
            )
        }
    };
    if job.status == "failed" {
        return json_resp(
            stream,
            400,
            &json!({ "error": { "message": job.error.clone().unwrap_or_else(|| "视频生成失败".into()), "code": "VIDEO_FAILED" } }),
        );
    }
    if job.status != "completed" || job.url.is_empty() {
        return json_resp(
            stream,
            409,
            &json!({ "error": { "message": "视频还没生成完，稍后再取", "code": "NOT_READY", "status": job.status } }),
        );
    }
    let name = job.url.rsplit('/').next().unwrap_or("").to_string();
    let cfg = Config::load(&config_file());
    let file = out_dir(&cfg).join(&name);
    match std::fs::read(&file) {
        Ok(buf) => resp(stream, 200, "video/mp4", &buf),
        Err(_) => json_resp(
            stream,
            404,
            &json!({ "error": { "message": "视频文件已被清理，请重新生成", "code": "FILE_GONE" } }),
        ),
    }
}

fn video_list(stream: &mut TcpStream) {
    let data: Vec<Value> = video_jobs()
        .lock()
        .map(|g| g.iter().rev().map(|j| j.to_json()).collect())
        .unwrap_or_default();
    json_resp(stream, 200, &json!({ "object": "list", "data": data }));
}

fn video_delete(stream: &mut TcpStream, id: &str) {
    let removed = video_jobs()
        .lock()
        .map(|mut g| {
            let before = g.len();
            g.retain(|x| x.id != id);
            g.len() != before
        })
        .unwrap_or(false);
    if removed {
        json_resp(stream, 200, &json!({ "id": id, "object": "video.deleted", "deleted": true }))
    } else {
        json_resp(
            stream,
            404,
            &json!({ "error": { "message": format!("没有这个视频任务：{}", id), "code": "NOT_FOUND" } }),
        )
    }
}

/// 视频中转：接 {"prompt": "..."}（可带参考图），返回 {"data":[{"url":"..."}]}
fn generate_video(stream: &mut TcpStream, req: &Req) {
    let low = req.ctype.to_lowercase();
    let is_json_body = req.body.first().map(|b| *b == b'{' || *b == b'[').unwrap_or(false);

    let parsed = if low.contains("multipart/form-data") {
        parse_multipart_req(&req.body, &req.ctype)
    } else if low.contains("x-www-form-urlencoded") {
        parse_form_req(&req.body)
    } else if low.contains("json") || is_json_body || req.body.is_empty() {
        parse_json_req(&req.body)
    } else {
        parse_json_req(&req.body).or_else(|_| parse_multipart_req(&req.body, &req.ctype))
    };

    let (prompt, _want_b64, refs) = match parsed {
        Ok(x) => x,
        Err(e) => {
            return json_resp(
                stream,
                400,
                &json!({ "error": { "message": e, "type": "invalid_request_error" } }),
            )
        }
    };
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return json_resp(
            stream,
            400,
            &json!({ "error": { "message": "prompt 不能为空", "type": "invalid_request_error" } }),
        );
    }

    util::log(&format!(
        "[视频] 收到{}：{}",
        if refs.is_empty() { String::new() } else { format!("（参考图 {} 张）", refs.len()) },
        prompt.chars().take(40).collect::<String>()
    ));

    // 视频生成很慢，同一时间只跑一个，避免多个请求把豆包页面抢乱
    let _guard = VIDEO_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let started = util::now_ms();
    match produce_video(&prompt, &refs) {
        Ok(res) => {
            let cost = ((util::now_ms().saturating_sub(started)) as f64) / 1000.0;
            util::log(&format!("[视频] 完成（{:.1}s）{} {}KB", cost, res.name, res.bytes / 1024));
            let public_url = format!("http://{}:{}/files/{}", HOST, port(), res.name);
            json_resp(
                stream,
                200,
                &json!({
                    "created": util::now_ms() / 1000,
                    "data": [{ "url": public_url }],
                    "elapsed": cost,
                }),
            )
        }
        Err(f) => {
            let cost = ((util::now_ms().saturating_sub(started)) as f64) / 1000.0;
            util::log(&format!("[视频] 失败（{:.1}s）：{}", cost, f.message));
            let code = match f.code.as_str() {
                "NOT_LOGGED_IN" => 401,
                _ => 502,
            };
            json_resp(stream, code, &json!({ "error": { "message": f.message, "code": f.code } }))
        }
    }
}

fn produce_video(prompt: &str, refs: &[ego::RefImage]) -> Result<Produced, Failure> {
    let cfg = Config::load(&config_file());
    let bin = util::find_ego(&cfg);

    let script = ego::build_video_script(
        prompt,
        &cfg.page_label,
        &cfg.video_conversation_name,
        VIDEO_TIMEOUT_MS,
        refs,
    );

    let v = match ego::run(&bin, &script, Duration::from_millis(VIDEO_TIMEOUT_MS)) {
        Ok(v) => v,
        Err(e) => return Err(Failure { message: e, code: "VIDEO_FAILED".into() }),
    };
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let msg = v.get("error").and_then(|x| x.as_str()).unwrap_or("未知错误").to_string();
        let code = v.get("code").and_then(|x| x.as_str()).unwrap_or("").to_string();
        // 把页面上看到的内容记下来，方便下次调整选择器
        if let Some(t) = v.get("pageTail").and_then(|x| x.as_str()) {
            let tail: String = t.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
            util::log(&format!("[视频] 页面尾部：{}", tail.replace('\n', " ")));
        }
        if let Some(m) = v.get("mode") {
            util::log(&format!("[视频] 模式切换结果：{}", m));
        }
        if code.is_empty() {
            return Err(Failure { message: msg, code: "VIDEO_FAILED".into() });
        }
        return Err(Failure { message: msg, code });
    }

    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if url.is_empty() {
        return Err(Failure { message: "没有拿到视频地址".into(), code: "VIDEO_FAILED".into() });
    }
    if let Some(m) = v.get("mode") {
        util::log(&format!("[视频] 模式切换结果：{}", m));
    }
    if let Some(urls) = v.get("urls").and_then(|x| x.as_array()) {
        for u in urls {
            if let Some(s) = u.as_str() {
                let short: String = s.chars().take(220).collect();
                util::log(&format!("[视频] 抓到地址：{}", short));
            }
        }
    }

    // 回写视频会话地址，方便排查
    let mut cur = Config::load(&config_file());
    if let Some(cu) = v.get("convUrl").and_then(|x| x.as_str()) {
        if cu.contains("/chat/") && cur.conversation_url != cu {
            cur.conversation_url = cu.to_string();
            cur.save(&config_file());
        }
    }

    let od = out_dir(&cfg);
    let _ = std::fs::create_dir_all(&od);
    let name = util::md5_name_ext(&url, "mp4");
    let file = od.join(&name);
    match util::download(&url, &file) {
        Ok(bytes) => {
            util::auto_clean(&od, &cfg);
            Ok(Produced { direct: false, src_url: url, name, bytes, file })
        }
        Err(e) => Err(Failure { message: format!("下载失败：{}", e), code: "DOWNLOAD_FAILED".into() }),
    }
}

/// 队列槽位的兜底释放：正常路径里手动 release 后把 released 置 true
struct SlotGuard {
    released: bool,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if !self.released {
            sched::release(false, 0.0);
        }
    }
}

struct Produced {
    direct: bool,
    src_url: String,
    name: String,
    bytes: u64,
    file: PathBuf,
}

struct Failure {
    message: String,
    code: String,
}

fn produce_image(prompt: &str, refs: &[ego::RefImage]) -> Result<Produced, Failure> {
    let cfg = Config::load(&config_file());
    let bin = util::find_ego(&cfg);

    let mut last: Option<Value> = None;
    for attempt in 1..=3 {
        let script = ego::build_script(prompt, &cfg.page_label, &cfg.conversation_name, GEN_TIMEOUT_MS, refs, &cfg.image_tier);
        let r = ego::run(&bin, &script, Duration::from_millis(GEN_TIMEOUT_MS));
        match r {
            Ok(v) => {
                if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
                    last = Some(v);
                    break;
                }
                let msg = v.get("error").and_then(|x| x.as_str()).unwrap_or("未知错误").to_string();
                let code = v.get("code").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !code.is_empty() {
                    return Err(Failure { message: msg, code });
                }
                let transient = msg.contains("连接") || msg.contains("ECONN") || msg.contains("10061");
                if !transient || attempt == 3 {
                    return Err(Failure { message: msg, code: "GEN_FAILED".into() });
                }
                util::log(&format!("[生图] 连接失败，第 {} 次重试", attempt));
                thread::sleep(Duration::from_secs(2));
            }
            Err(e) => {
                if attempt == 3 {
                    return Err(Failure { message: e, code: "GEN_FAILED".into() });
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
    }

    let v = match last {
        Some(v) => v,
        None => return Err(Failure { message: "生成失败".into(), code: "GEN_FAILED".into() }),
    };

    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if url.is_empty() {
        return Err(Failure { message: "没有拿到图片地址".into(), code: "GEN_FAILED".into() });
    }
    // 诊断：把页面上抓到的生成图 URL 都记下来，方便排查拿到的是不是缩略图
    util::log(&format!("[原图] 档位={}", v.get("tier").and_then(|x| x.as_str()).unwrap_or("thumb")));
    if let Some(d) = v.get("rawDbg") {
        util::log(&format!(
            "[原图] cached={} targets={} found={} err={}",
            d.get("cached").and_then(|x| x.as_bool()).unwrap_or(false),
            d.get("targets").and_then(|x| x.as_i64()).unwrap_or(-1),
            d.get("found").map(|x| !x.is_null()).unwrap_or(false),
            d.get("err").and_then(|x| x.as_str()).unwrap_or("-")
        ));
    }
    if let Some(urls) = v.get("urls").and_then(|x| x.as_array()) {
        for u in urls {
            if let Some(s) = u.as_str() {
                let short: String = s.chars().take(220).collect();
                util::log(&format!("[抓图] {}", short));
            }
        }
    }

    // 回写页面/对话信息，方便下次直接命中
    let mut cur = Config::load(&config_file());
    let mut dirty = false;
    if let Some(pl) = v.get("pageLabel").and_then(|x| x.as_str()) {
        if !pl.is_empty() && cur.page_label != pl {
            cur.page_label = pl.to_string();
            dirty = true;
        }
    }
    if let Some(cu) = v.get("convUrl").and_then(|x| x.as_str()) {
        if cu.contains("/chat/") && cur.conversation_url != cu {
            cur.conversation_url = cu.to_string();
            dirty = true;
            util::log(&format!("[生图] 固定对话：{}", cu));
        }
    }
    if v.get("renamed").and_then(|x| x.as_bool()) == Some(true) {
        util::log(&format!("[生图] 对话已命名为「{}」", cur.conversation_name));
    }
    if dirty {
        cur.save(&config_file());
    }

    if cfg.direct_mode {
        return Ok(Produced { direct: true, src_url: url, name: String::new(), bytes: 0, file: PathBuf::new() });
    }

    let od = out_dir(&cfg);
    let _ = std::fs::create_dir_all(&od);
    let name = util::md5_name(&url);
    let file = od.join(&name);
    match util::download(&url, &file) {
        Ok(bytes) => {
            util::auto_clean(&od, &cfg);
            Ok(Produced { direct: false, src_url: url, name, bytes, file })
        }
        Err(e) => Err(Failure { message: format!("下载失败：{}", e), code: "DOWNLOAD_FAILED".into() }),
    }
}

// ---------------- 响应 ----------------

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

fn resp(stream: &mut TcpStream, code: u16, ct: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type, authorization\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        code,
        reason(code),
        ct,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_resp(stream: &mut TcpStream, code: u16, v: &Value) {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    resp(stream, code, "application/json; charset=utf-8", &body);
}

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// ---------- 参考图 ----------
const MAX_REFS: usize = 9;
const MAX_REF_BYTES: usize = 10 * 1024 * 1024;

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in s.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = b64_val(b)?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    if b.len() > 8 && &b[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if b.len() > 3 && &b[..3] == b"\xFF\xD8\xFF" {
        return Some("image/jpeg");
    }
    if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if b.len() > 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// 从一段字符串里取参考图：支持 data URL、纯 base64、http(s) 地址三种写法。
fn fetch_ref(item: &str, idx: usize) -> Result<ego::RefImage, String> {
    let item = item.trim();
    if item.starts_with("http://") || item.starts_with("https://") {
        let dir = base_dir().join("_ref");
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join(format!("r{}_{}.img", util::now_ms(), idx));
        util::download(item, &dest).map_err(|e| format!("参考图下载失败：{}", e))?;
        let b = std::fs::read(&dest).map_err(|e| format!("参考图读取失败：{}", e))?;
        let _ = std::fs::remove_file(&dest);
        return finish_ref(b, None, idx);
    }
    let (declared, payload) = match item.strip_prefix("data:") {
        Some(rest) => {
            let (head, payload) = rest.split_once(',').ok_or("参考图的 data URL 格式不对")?;
            (head.split(';').next().map(|s| s.to_string()), payload)
        }
        None => (None, item),
    };
    let clean: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = b64_decode(&clean).ok_or("参考图 base64 解不开")?;
    finish_ref(bytes, declared, idx)
}

fn finish_ref(bytes: Vec<u8>, declared: Option<String>, idx: usize) -> Result<ego::RefImage, String> {
    if bytes.is_empty() {
        return Err("参考图是空的".into());
    }
    if bytes.len() > MAX_REF_BYTES {
        return Err(format!("参考图太大（单张上限 {} MB）", MAX_REF_BYTES / 1024 / 1024));
    }
    let mime = match sniff_mime(&bytes) {
        Some(m) => m.to_string(),
        None => match declared.filter(|m| m.starts_with("image/")) {
            Some(m) => m,
            None => return Err("参考图格式不支持，只认 PNG / JPEG / WEBP / GIF".into()),
        },
    };
    let ext = mime.rsplit('/').next().unwrap_or("png").to_string();
    Ok((format!("ref{}.{}", idx + 1, ext), mime, b64(&bytes)))
}

/// 判断一个字符串值是不是图片：data URL / 图片链接 / 长 base64
fn looks_like_image(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    if s.starts_with("data:image/") {
        return true;
    }
    let low = s.to_ascii_lowercase();
    if low.starts_with("http://") || low.starts_with("https://") {
        return low.contains(".png")
            || low.contains(".jpg")
            || low.contains(".jpeg")
            || low.contains(".webp")
            || low.contains(".gif")
            || low.contains("image")
            || low.contains("blob");
    }
    // 纯 base64：去掉换行后全是 base64 字符，且解码出来能认出图片格式
    let dense: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if dense.len() >= 64 && dense.bytes().all(|c| b64_val(c).is_some() || c == b'=') {
        let head: String = dense.chars().take(4096).collect();
        if let Some(b) = b64_decode(&head) {
            return sniff_mime(&b).is_some();
        }
    }
    false
}

const REF_KEYS: [&str; 13] = [
    "image",
    "images",
    "image_url",
    "imageUrl",
    "init_image",
    "reference_image",
    "reference_images",
    "ref_images",
    "source_image",
    "input_image",
    "img",
    "photo",
    "mask",
];

/// 从已知字段里取值，兼容字符串 / 数组 / {url:...} 三种形态
fn grab_node(x: &Value, out: &mut Vec<String>) {
    match x {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for it in a {
                grab_node(it, out);
            }
        }
        Value::Object(o) => {
            for k in ["url", "b64_json", "base64", "data", "image_url", "image", "content", "src", "value"] {
                if let Some(v) = o.get(k) {
                    grab_node(v, out);
                }
            }
        }
        _ => {}
    }
}

/// 兜底：整个 JSON 里任何看起来像图片的值都收进来（字段名随便叫什么都能认）
fn scan_any(x: &Value, out: &mut Vec<String>) {
    match x {
        Value::String(s) => {
            if looks_like_image(s) {
                out.push(s.clone());
            }
        }
        Value::Array(a) => {
            for it in a {
                scan_any(it, out);
            }
        }
        Value::Object(o) => {
            for (_, v) in o {
                scan_any(v, out);
            }
        }
        _ => {}
    }
}

fn collect_refs(v: &Value) -> Result<Vec<ego::RefImage>, String> {
    let mut raw: Vec<String> = Vec::new();
    for k in REF_KEYS {
        if let Some(x) = v.get(k) {
            grab_node(x, &mut raw);
        }
    }
    scan_any(v, &mut raw);

    let mut seen = std::collections::HashSet::new();
    let mut uniq: Vec<String> = Vec::new();
    for s in raw {
        let t = s.trim().to_string();
        if t.is_empty() || !looks_like_image(&t) {
            continue;
        }
        if seen.insert(t.clone()) {
            uniq.push(t);
        }
    }
    if uniq.len() > MAX_REFS {
        return Err(format!("参考图最多 {} 张", MAX_REFS));
    }
    let mut out = Vec::new();
    for (i, s) in uniq.iter().enumerate() {
        out.push(fetch_ref(s, i)?);
    }
    Ok(out)
}

// ---------- 请求格式：JSON / multipart / urlencoded ----------

fn brief(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(&body[..body.len().min(200)]);
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn shape(v: &Value) -> String {
    match v {
        Value::String(s) => format!("string({})", s.len()),
        Value::Array(a) => format!("array({})", a.len()),
        Value::Object(o) => format!("object({})", o.len()),
        Value::Number(n) => format!("number({})", n),
        Value::Bool(b) => format!("bool({})", b),
        Value::Null => "null".to_string(),
    }
}

/// 只打印请求体的字段结构和类型，不打印内容（图太大）
fn body_shape(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(o) = v.as_object() {
            return o
                .iter()
                .map(|(k, val)| format!("{}={}", k, shape(val)))
                .collect::<Vec<_>>()
                .join(" · ");
        }
    }
    brief(body)
}

fn parse_json_req(body: &[u8]) -> Result<(String, bool, Vec<ego::RefImage>), String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "请求体不是合法 JSON".to_string())?;
    let prompt = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let want_b64 = v.get("response_format").and_then(|x| x.as_str()) == Some("b64_json")
        || v.get("b64_json").and_then(|x| x.as_bool()) == Some(true);
    let refs = collect_refs(&v)?;
    Ok((prompt, want_b64, refs))
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn parse_form_req(body: &[u8]) -> Result<(String, bool, Vec<ego::RefImage>), String> {
    let s = String::from_utf8_lossy(body).to_string();
    let mut prompt = String::new();
    let mut want_b64 = false;
    let mut raws: Vec<String> = Vec::new();
    for pair in s.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(x) => x,
            None => continue,
        };
        let k = url_decode(k);
        let val = url_decode(v);
        if k == "prompt" {
            prompt = val;
        } else if k == "response_format" {
            want_b64 = val == "b64_json";
        } else if looks_like_image(&val) {
            raws.push(val);
        }
    }
    if raws.len() > MAX_REFS {
        return Err(format!("参考图最多 {} 张", MAX_REFS));
    }
    let mut refs = Vec::new();
    for (i, r) in raws.iter().enumerate() {
        refs.push(fetch_ref(r, i)?);
    }
    Ok((prompt, want_b64, refs))
}

fn boundary_of(ctype: &str) -> Option<String> {
    let low = ctype.to_lowercase();
    let i = low.find("boundary=")?;
    let s = ctype[i + 9..].trim();
    if let Some(rest) = s.strip_prefix('"') {
        let e = rest.find('"')?;
        Some(rest[..e].to_string())
    } else {
        Some(s.split(';').next().unwrap_or(s).trim().to_string())
    }
}

/// 取 form-data 里某个属性，如 name="x" / filename="a.png"
fn attr(head: &str, key: &str) -> Option<String> {
    let b = head.as_bytes();
    let k = key.as_bytes();
    let mut i = 0;
    while i + k.len() < b.len() {
        let hit = b[i..i + k.len()].eq_ignore_ascii_case(k)
            && i > 0
            && matches!(b[i - 1], b' ' | b';' | b'\t' | b'\r' | b'\n');
        if hit {
            let mut j = i + k.len();
            if b.get(j) == Some(&b'=') {
                j += 1;
                if b.get(j) == Some(&b'"') {
                    j += 1;
                    let start = j;
                    while j < b.len() && b[j] != b'"' {
                        j += 1;
                    }
                    return Some(String::from_utf8_lossy(&b[start..j]).to_string());
                }
                let start = j;
                while j < b.len() && b[j] != b';' && b[j] != b'\r' && b[j] != b'\n' {
                    j += 1;
                }
                return Some(String::from_utf8_lossy(&b[start..j]).trim().to_string());
            }
        }
        i += 1;
    }
    None
}

fn find_sub(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() || from > h.len() - n.len() {
        return None;
    }
    let mut i = from;
    let last = h.len() - n.len();
    while i <= last {
        if &h[i..i + n.len()] == n {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_multipart_req(body: &[u8], ctype: &str) -> Result<(String, bool, Vec<ego::RefImage>), String> {
    let bnd = match boundary_of(ctype) {
        Some(b) => b,
        None => {
            return Err(format!(
                "multipart 缺少 boundary（content-type：{}）",
                if ctype.is_empty() { "空" } else { ctype }
            ))
        }
    };
    let marker = format!("--{}", bnd).into_bytes();
    let sep = format!("\r\n--{}", bnd).into_bytes();

    let mut pos = find_sub(body, &marker, 0).ok_or("multipart 里找不到分隔符，格式可能不对")?;
    pos += marker.len();

    let mut prompt = String::new();
    let mut want_b64 = false;
    let mut raws: Vec<(Option<String>, Vec<u8>)> = Vec::new();

    loop {
        if pos + 1 < body.len() && &body[pos..pos + 2] == b"--" {
            break;
        }
        if pos + 1 < body.len() && &body[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
        let end = find_sub(body, &sep, pos).unwrap_or(body.len());
        let part = &body[pos..end];

        let (head, content) = match find_sub(part, b"\r\n\r\n", 0) {
            Some(x) => (&part[..x], &part[x + 4..]),
            None => (&part[..0], part),
        };
        let hstr = String::from_utf8_lossy(head).to_string();
        let name = attr(&hstr, "name").unwrap_or_default();
        let fname = attr(&hstr, "filename").filter(|s| !s.is_empty());
        let ptype = attr(&hstr, "content-type").filter(|s| !s.is_empty());
        let lname = name.to_lowercase();

        let looks_file = fname.is_some()
            || ptype.as_deref().map(|p| p.to_lowercase().starts_with("image/")).unwrap_or(false)
            || lname.contains("image")
            || lname.contains("file")
            || lname.contains("ref")
            || lname.contains("photo")
            || lname.contains("source")
            || lname.contains("mask")
            || lname.contains("img");

        if looks_file && !content.is_empty() {
            util::log(&format!(
                "  part name={:?} file={:?} type={:?} {}B",
                name,
                fname,
                ptype,
                content.len()
            ));
            raws.push((ptype, content.to_vec()));
        } else if lname == "prompt" || lname == "text" || lname == "q" {
            prompt = String::from_utf8_lossy(content).trim().to_string();
        } else if lname == "response_format" {
            want_b64 = String::from_utf8_lossy(content).trim() == "b64_json";
        } else if prompt.is_empty() {
            if let Ok(s) = std::str::from_utf8(content) {
                let t = s.trim();
                if !t.is_empty() && !t.contains('\u{0}') {
                    prompt = t.to_string();
                }
            }
        }

        if end >= body.len() {
            break;
        }
        pos = end + sep.len();
    }

    if raws.len() > MAX_REFS {
        return Err(format!("参考图最多 {} 张", MAX_REFS));
    }
    let mut refs = Vec::new();
    for (i, (mime, bytes)) in raws.into_iter().enumerate() {
        refs.push(finish_ref(bytes, mime, i)?);
    }
    Ok((prompt, want_b64, refs))
}

// ---------- 版本检测 ----------
static VER_CACHE: OnceLock<std::sync::Mutex<Option<(u64, Option<Release>, Option<String>)>>> =
    OnceLock::new();

fn ver_tuple(s: &str) -> Vec<u32> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse().ok())
        .collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let a = ver_tuple(latest);
    let b = ver_tuple(current);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

#[derive(Clone)]
struct Release {
    tag: String,
    url: String,
}

/// 查询失败的原因，决定给用户看什么提示
#[derive(Debug)]
enum FetchErr {
    /// 403/404：仓库私有、没权限、或被风控拦
    Forbidden,
    /// 连不上、超时
    Network,
    /// 连上了但内容不对
    Bad(String),
}

impl FetchErr {
    fn hint(&self) -> String {
        match self {
            FetchErr::Forbidden => "Gitee 拒绝访问（HTTP 403/404）。仓库若已设为私有，匿名访问拿不到版本号，请把仓库改回公开，或在软件里配置访问令牌".into(),
            FetchErr::Network => "连不上 Gitee，检查网络或代理".into(),
            FetchErr::Bad(s) => format!("Gitee 返回内容异常：{}", s),
        }
    }
}

/// curl 发 GET，返回 (HTTP 状态码, 主体)；redirect_only=true 时主体位置是重定向地址
fn curl_get(url: &str, redirect_only: bool) -> Result<(u16, String), FetchErr> {
    let mut cmd = Command::new("curl");
    cmd.arg("-sS").args(["--max-time", "10"]);
    if redirect_only {
        cmd.args(["-o", "NUL", "-w", "%{redirect_url}\n%{http_code}"]);
    } else {
        cmd.args(["-w", "\n%{http_code}"]);
    }
    // 用浏览器 UA，降低被 Gitee 风控当爬虫拦的概率
    cmd.args([
        "-H",
        "User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "-H",
        "Accept: text/html,application/json,*/*",
        "-H",
        "Accept-Language: zh-CN,zh;q=0.9",
        url,
    ]);
    let out = cmd
        .creation_flags(util::CREATE_NO_WINDOW)
        .output()
        .map_err(|_| FetchErr::Network)?;
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = match s.rsplit_once('\n') {
        Some((b, c)) => (b.to_string(), c.trim().parse::<u16>().unwrap_or(0)),
        None => (String::new(), 0),
    };
    if code == 0 {
        return Err(FetchErr::Network);
    }
    Ok((code, body))
}

/// 查 Gitee 上最新发行版，10 分钟内复用缓存；force=true 跳过缓存直接查。
/// 返回 (结果, 错误提示)；两者互斥，都拿不到时���误提示一定有值。
fn latest_release(force: bool) -> (Option<Release>, Option<String>) {
    let m = VER_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if !force {
        if let Ok(g) = m.lock() {
            if let Some((at, rel, err)) = g.as_ref() {
                if util::now_ms().saturating_sub(*at) < 10 * 60 * 1000 {
                    return (rel.clone(), err.clone());
                }
            }
        }
    }
    // 主通道：官方 API（能拿到准确附件名和地址）
    let (rel, err) = match fetch_release_api() {
        Ok(r) => (Some(r), None),
        Err(api_err) => {
            // 兜底通道：网页重定向（API 限流/风控时还能用）
            match fetch_release_web() {
                Ok(r) => {
                    util::log(&format!(
                        "[更新] API 不可用（{}），已改用网页通道",
                        api_err.hint()
                    ));
                    (Some(r), None)
                }
                Err(web_err) => {
                    // 两条都不通：优先报 API 的错误（通常是真正的故障原因）
                    let msg = if matches!(api_err, FetchErr::Forbidden) {
                        api_err.hint()
                    } else {
                        format!("{}；网页通道也没通：{}", api_err.hint(), web_err.hint())
                    };
                    util::log(&format!("[更新] 检查失败：{}", msg));
                    (None, Some(msg))
                }
            }
        }
    };
    // 成功就写缓存；失败保留上次成功的缓存，别让一次网络抖动把已知数据清掉
    if let Ok(mut g) = m.lock() {
        let has_good = g.as_ref().map(|(_, r, _)| r.is_some()).unwrap_or(false);
        if rel.is_some() || !has_good {
            *g = Some((util::now_ms(), rel.clone(), err.clone()));
        }
    }
    (rel, err)
}

/// Gitee 官方 API：匿名访问私有仓库会 403
fn fetch_release_api() -> Result<Release, FetchErr> {
    let (code, body) = curl_get(
        "https://gitee.com/api/v5/repos/super88888/doubao-img-service/releases/latest",
        false,
    )?;
    if code == 403 || code == 404 {
        return Err(FetchErr::Forbidden);
    }
    if code != 200 {
        return Err(FetchErr::Bad(format!("HTTP {}", code)));
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|_| FetchErr::Bad("返回的不是 JSON".into()))?;
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
        .unwrap_or_default();
    if tag.is_empty() {
        return Err(FetchErr::Bad("没有 tag_name".into()));
    }
    // Gitee 会自带源码 zip，这里只挑 exe 附件
    let url = v
        .get("assets")
        .and_then(|x| x.as_array())
        .and_then(|a| {
            a.iter().find(|it| {
                it.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.to_lowercase().ends_with(".exe"))
                    .unwrap_or(false)
            })
        })
        .and_then(|it| it.get("browser_download_url"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Release { tag, url })
}

/// 网页通道（不走 API）：
/// https://gitee.com/.../releases/latest 会 302 到 /releases/tag/v1.2.5，从跳转地址取版本号；
/// 下载地址按固定模式拼（附件名固定为 豆包生图服务.exe，URL 编码写死）。
fn fetch_release_web() -> Result<Release, FetchErr> {
    let (code, loc) = curl_get(
        "https://gitee.com/super88888/doubao-img-service/releases/latest",
        true,
    )?;
    if code == 403 || code == 404 {
        return Err(FetchErr::Forbidden);
    }
    let tag = loc
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    // 只有版本号形态才对，跳登录页之类的一律不认
    if tag.is_empty() || !tag.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return Err(FetchErr::Bad(format!("重定向地址异常（HTTP {}）", code)));
    }
    let url = format!(
        "https://gitee.com/super88888/doubao-img-service/releases/download/v{}/%E8%B1%86%E5%8C%85%E7%94%9F%E5%9B%BE%E6%9C%8D%E5%8A%A1.exe",
        tag
    );
    Ok(Release { tag, url })
}

/// 用 ShellExecute 拉起新进程：参数走命令行，新进程由系统 shell 创建
fn shell_start(exe: &PathBuf) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let dir = match exe.parent() {
        Some(p) => p.to_string_lossy().to_string(),
        None => return false,
    };
    // 新进程不继承本进程的环境变量，运行模式只能靠命令行传过去
    let mut params = String::from("--restart --trusted");
    let headless = std::env::var("DOUBAO_HEADLESS").map(|v| v.trim() == "1").unwrap_or(false)
        || std::env::args().any(|a| a == "--headless");
    if headless {
        params.push_str(" --headless");
    }
    if port() != PORT_DEFAULT {
        params.push_str(&format!(" --port {}", port()));
    }
    let h = unsafe {
        ShellExecuteW(
            None,
            &windows::core::HSTRING::from("open"),
            &windows::core::HSTRING::from(exe.to_string_lossy().as_ref()),
            &windows::core::HSTRING::from(params.as_str()),
            &windows::core::HSTRING::from(dir.as_str()),
            SW_SHOWNORMAL,
        )
    };
    // 返回值大于 32 才是成功，否则是错误码
    h.0 as isize > 32
}

/// 一键更新：下新版 → 换掉自己 → 重启。
/// report(阶段号, 文字) 把进度报出去（覆盖层轮询 /api/update/status 用）。
/// 阶段：1 检查 / 2 下载 / 3 校验 / 4 替换 / 5 重启中 / 6 已最新 / 9 错误
fn do_update(report: &dyn Fn(u8, &str)) -> Result<Value, String> {
    let cur = env!("CARGO_PKG_VERSION").to_string();
    report(1, "正在检查最新版本…");
    *VER_CACHE.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap() = None; // 强制重查
    let (rel, err) = latest_release(true);
    let rel = rel.ok_or_else(|| err.unwrap_or_else(|| "查不到最新版本".into()))?;
    if !is_newer(&rel.tag, &cur) {
        report(6, "已是最新版本");
        return Ok(json!({ "ok": true, "upToDate": true, "current": cur, "latest": rel.tag }));
    }
    if rel.url.is_empty() {
        return Err("最新版里没找到 exe 附件".into());
    }
    report(2, &format!("发现 v{}，正在下载…", rel.tag));

    let exe = std::env::current_exe().map_err(|_| "拿不到本程序的路径")?;
    let dir = exe.parent().map(|p| p.to_path_buf()).ok_or("拿不到程序目录")?;
    let tmp = dir.join("_update.exe");

    util::log(&format!("[更新] 下载 v{}", rel.tag));
    let size = util::download(&rel.url, &tmp).map_err(|e| format!("下载失败：{}", e))?;
    report(3, &format!("下载完成（{}），正在校验…", fmt_kb(size)));
    if size < 300 * 1024 {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("下载的文件不对（只有 {} 字节）", size));
    }
    let mut head = [0u8; 2];
    let is_pe = std::fs::File::open(&tmp)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|_| &head == b"MZ")
        .unwrap_or(false);
    if !is_pe {
        let _ = std::fs::remove_file(&tmp);
        return Err("下载下来的不是可执行文件（拿到的是网页或报错页）".into());
    }
    report(4, "校验通过，正在替换程序…");

    // 当前 exe 在跑着，删不掉但能改名；改完把新版顶上去
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "doubao-img".into());
    let bak = dir.join(format!("{}.old.exe", stem));
    let _ = std::fs::remove_file(&bak);
    std::fs::rename(&exe, &bak).map_err(|e| format!("把旧版改名失败：{}", e))?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        // 换不回去就还原，别把程序弄没了
        let _ = std::fs::rename(&bak, &exe);
        return Err(format!("换上新版本失败：{}", e));
    }
    util::log(&format!("[更新] 已换到 v{}，准备重启", rel.tag));
    report(5, &format!("已换到 v{}，正在重启…", rel.tag));

    // 交给系统 shell 启动：新进程挂到 explorer 名下，
    // 不会跟着本进程的宿主一起被回收（有些启动器会连带子进程一起清掉）
    let started = shell_start(&exe);
    if !started {
        return Err("新版放好了，但没启动起来，请手动双击运行".into());
    }
    Ok(json!({ "ok": true, "restarting": true, "latest": rel.tag, "current": cur }))
}

fn fmt_kb(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else {
        format!("{} KB", n / 1024)
    }
}

// ---------- 异步更新：接口立刻返回，进度存全局，前端轮询 ----------
static UPD_STATE: OnceLock<std::sync::Mutex<(u8, String)>> = OnceLock::new();

fn upd_state() -> &'static std::sync::Mutex<(u8, String)> {
    UPD_STATE.get_or_init(|| std::sync::Mutex::new((0, String::new())))
}

/// 0 = 空闲；1..=6 进行中/完成；9 = 失败
fn upd_running() -> bool {
    matches!(upd_state().lock().unwrap().0, 1..=5)
}

fn upd_status() -> (u8, String) {
    let s = upd_state().lock().unwrap();
    (s.0, s.1.clone())
}

fn upd_start() -> Result<(), String> {
    {
        let mut s = upd_state().lock().unwrap();
        if matches!(s.0, 1..=5) {
            return Err("已有更新在进行中".into());
        }
        *s = (1, "准备开始…".into());
    }
    thread::Builder::new()
        .name("updater".into())
        .spawn(|| {
            let report = |stage: u8, msg: &str| {
                let mut s = upd_state().lock().unwrap();
                *s = (stage, msg.to_string());
                util::log(&format!("[更新][{}] {}", stage, msg));
            };
            match do_update(&report) {
                Ok(v) => {
                    let restart = v.get("restarting").and_then(|x| x.as_bool()).unwrap_or(false);
                    if restart {
                        // 给前端一点时间看到“正在重启”再退
                        thread::sleep(Duration::from_millis(1500));
                        webview::remove_tray();
                        util::log("[更新] 旧进程退出");
                        std::process::exit(0);
                    }
                    // 已是最新：状态留在 6，前端自己收尾
                }
                Err(e) => {
                    util::log(&format!("[更新] 失败：{}", e));
                    let mut s = upd_state().lock().unwrap();
                    *s = (9, e);
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("更新线程启动失败：{}", e))
}

static PANEL: &str = include_str!("panel.html");

/// 注入给 WebView2 的页面内容
pub fn panel_html() -> String {
    PANEL.to_string()
}
