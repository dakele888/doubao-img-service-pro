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

pub const PORT: u16 = 8788;
const HOST: &str = "127.0.0.1";
const GEN_TIMEOUT_MS: u64 = 240_000;
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

    let addr = format!("{}:{}", HOST, PORT);

    // 已经有实例在跑：让它把窗口提到前面，本进程直接退出
    if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(600)).is_ok() {
        util::log("检测到已有实例，唤醒它的窗口");
        poke("/api/show");
        return;
    }

    let cfg = Config::load(&config_file());

    // 免责声明每次都显示；启动密码可以在面板里关掉
    if !auth::unlock(cfg.require_password) {
        util::log("启动验证未通过，退出");
        return;
    }

    let od = out_dir(&cfg);
    let _ = std::fs::create_dir_all(&od);
    util::log(&format!("界面：原生窗口 · 接口 http://{}:{}/", HOST, PORT));
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
    webview::run();
    util::log("窗口已关闭，退出");
}

fn serve() {
    let addr = format!("{}:{}", HOST, PORT);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            util::log(&format!("端口 {} 绑定失败：{}", PORT, e));
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
    if let Ok(mut s) = TcpStream::connect((HOST, PORT)) {
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
    let url = format!("http://{}:{}/", HOST, PORT);
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

struct Req {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
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

    let mut content_length = 0usize;
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
        }
    }

    let mut body = vec![0u8; content_length.min(8 * 1024 * 1024)];
    if content_length > 0 {
        if reader.read_exact(&mut body).is_err() {
            return;
        }
    }

    let req = Req { method, path, body };
    route(&mut stream, req);
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
    if m == "GET" && p == "/api/version" {
        let current = env!("CARGO_PKG_VERSION").to_string();
        let latest = latest_release();
        let has = latest.as_deref().map(|l| is_newer(l, &current)).unwrap_or(false);
        return json_resp(
            stream,
            200,
            &json!({
                "current": current,
                "latest": latest,
                "hasUpdate": has,
                "url": "https://gitee.com/super88888/doubao-img-service/releases",
            }),
        );
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
                } else {
                    "image/jpeg"
                };
                resp(stream, 200, ct, &buf)
            }
            Err(_) => json_resp(stream, 404, &json!({ "error": "not found" })),
        };
    }

    if m == "POST" && (p == "/v1/images/generations" || p == "/v1/images/edits") {
        return generate(stream, &req.body);
    }

    json_resp(stream, 404, &json!({ "error": "not found" }))
}

fn generate(stream: &mut TcpStream, body: &[u8]) {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return json_resp(stream, 400, &json!({ "error": { "message": "请求体不是合法 JSON" } })),
    };
    let prompt = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return json_resp(stream, 400, &json!({ "error": { "message": "prompt 不能为空" } }));
    }
    let want_b64 = v.get("response_format").and_then(|x| x.as_str()) == Some("b64_json");

    let refs = match collect_refs(&v) {
        Ok(r) => r,
        Err(e) => return json_resp(stream, 400, &json!({ "error": { "message": e } })),
    };

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
                format!("http://{}:{}/files/{}", HOST, PORT, res.name)
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
        let script = ego::build_script(prompt, &cfg.page_label, &cfg.conversation_name, GEN_TIMEOUT_MS, refs);
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
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type, authorization\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
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
const MAX_REFS: usize = 3;
const MAX_REF_BYTES: usize = 8 * 1024 * 1024;

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

/// 从请求体里取参考图：支持 data URL、纯 base64、http(s) 地址三种写法。
fn collect_refs(v: &Value) -> Result<Vec<ego::RefImage>, String> {
    let mut raw: Vec<String> = Vec::new();
    for key in ["image", "image_url", "imageUrl", "init_image", "reference_image", "source_image"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                raw.push(s.to_string());
            }
        }
    }
    if let Some(arr) = v.get("images").and_then(|x| x.as_array()) {
        for it in arr {
            if let Some(s) = it.as_str() {
                let s = s.trim();
                if !s.is_empty() {
                    raw.push(s.to_string());
                }
            }
        }
    }
    if raw.len() > MAX_REFS {
        return Err(format!("参考图最多 {} 张", MAX_REFS));
    }

    let mut out = Vec::new();
    for (i, item) in raw.iter().enumerate() {
        let mut declared: Option<String> = None;
        let bytes = if item.starts_with("http://") || item.starts_with("https://") {
            let dir = base_dir().join("_ref");
            let _ = std::fs::create_dir_all(&dir);
            let dest = dir.join(format!("r{}_{}.img", util::now_ms(), i));
            util::download(item, &dest).map_err(|e| format!("参考图下载失败：{}", e))?;
            let b = std::fs::read(&dest).map_err(|e| format!("参考图读取失败：{}", e))?;
            let _ = std::fs::remove_file(&dest);
            b
        } else if let Some(rest) = item.strip_prefix("data:") {
            let (head, payload) = rest.split_once(',').ok_or("参考图的 data URL 格式不对")?;
            declared = head.split(';').next().map(|s| s.to_string());
            let clean: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
            b64_decode(&clean).ok_or("参考图 base64 解不开")?
        } else {
            let clean: String = item.chars().filter(|c| !c.is_whitespace()).collect();
            b64_decode(&clean).ok_or("参考图 base64 解不开")?
        };

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
        out.push((format!("ref{}.{}", i + 1, ext), mime, b64(&bytes)));
    }
    Ok(out)
}

// ---------- 版本检测 ----------
static VER_CACHE: OnceLock<std::sync::Mutex<Option<(u64, Option<String>)>>> = OnceLock::new();

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

/// 查 Gitee 上最新发行版的 tag，10 分钟内复用缓存
fn latest_release() -> Option<String> {
    let m = VER_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(g) = m.lock() {
        if let Some((at, v)) = g.as_ref() {
            if util::now_ms().saturating_sub(*at) < 10 * 60 * 1000 {
                return v.clone();
            }
        }
    }
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "8",
            "-H",
            "User-Agent: doubao-img",
            "https://gitee.com/api/v5/repos/super88888/doubao-img-service/releases/latest",
        ])
        .creation_flags(util::CREATE_NO_WINDOW)
        .output()
        .ok();
    let mut tag = None;
    if let Some(o) = out {
        if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
            tag = v
                .get("tag_name")
                .and_then(|x| x.as_str())
                .map(|s| s.trim_start_matches('v').to_string());
        }
    }
    if let Ok(mut g) = m.lock() {
        *g = Some((util::now_ms(), tag.clone()));
    }
    tag
}

static PANEL: &str = include_str!("panel.html");

/// 注入给 WebView2 的页面内容
pub fn panel_html() -> String {
    PANEL.to_string()
}
