// 杂活：日志、时间、下载、缓存统计与清理、ego 定位、Edge 定位。
use crate::cfg::Config;
use md5::{Digest, Md5};
use std::fs;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- 日志 ----------
// GUI 子系统没有控制台，日志必须落文件。同时留一份内存环形缓冲给面板看。
static LOG_RING: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn ring() -> &'static Mutex<Vec<String>> {
    LOG_RING.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn log(msg: &str) {
    let line = format!("[{}] {}", hhmmss(), msg);
    if let Ok(mut g) = ring().lock() {
        g.push(line.clone());
        if g.len() > 300 {
            g.remove(0);
        }
    }
    let path = crate::base_dir().join("service.log");
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 1024 * 1024 {
            let _ = fs::rename(&path, crate::base_dir().join("service.old.log"));
        }
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", line);
    }
}

pub fn recent_logs(n: usize) -> Vec<String> {
    ring().lock().map(|g| g.iter().rev().take(n).rev().cloned().collect()).unwrap_or_default()
}

fn hhmmss() -> String {
    // 用本地时间：Windows 下按 UTC+8 的常见场景直接算，避免引入 chrono
    let secs = now_ms() / 1000;
    let t = (secs + 8 * 3600) % 86400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

// ---------- 图片文件 ----------
pub struct ImgInfo {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

pub fn list_images(dir: &Path) -> Vec<ImgInfo> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let low = name.to_lowercase();
            if !(low.ends_with(".png") || low.ends_with(".jpg") || low.ends_with(".jpeg") || low.ends_with(".webp")
                || low.ends_with(".mp4") || low.ends_with(".webm"))
            {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            out.push(ImgInfo { name, size: meta.len(), mtime });
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

pub fn stats(dir: &Path) -> (u64, u64) {
    let list = list_images(dir);
    (list.len() as u64, list.iter().map(|i| i.size).sum())
}

/// 先按天数、再按张数、最后按体积，都从最旧的开始删。
pub fn auto_clean(dir: &Path, cfg: &Config) {
    let mut list = list_images(dir);
    if list.is_empty() {
        return;
    }
    let now = now_ms();

    if cfg.keep_days > 0 {
        let edge = now.saturating_sub(cfg.keep_days * 86_400_000);
        list.retain(|i| {
            if i.mtime < edge {
                let _ = fs::remove_file(dir.join(&i.name));
                false
            } else {
                true
            }
        });
    }

    let old_first = {
        let mut v: Vec<&ImgInfo> = list.iter().collect();
        v.sort_by(|a, b| a.mtime.cmp(&b.mtime));
        v
    };

    if cfg.max_files > 0 && (list.len() as u64) > cfg.max_files {
        let drop_n = list.len() as u64 - cfg.max_files;
        for it in old_first.iter().take(drop_n as usize) {
            let _ = fs::remove_file(dir.join(&it.name));
        }
        list = list_images(dir);
    }

    if cfg.max_mb > 0 {
        let limit = cfg.max_mb * 1024 * 1024;
        let mut total: u64 = list.iter().map(|i| i.size).sum();
        if total > limit {
            let mut v: Vec<&ImgInfo> = list.iter().collect();
            v.sort_by(|a, b| a.mtime.cmp(&b.mtime));
            for it in v {
                if total <= limit {
                    break;
                }
                if fs::remove_file(dir.join(&it.name)).is_ok() {
                    total = total.saturating_sub(it.size);
                }
            }
        }
    }
}

pub fn clear_all(dir: &Path) -> u64 {
    let list = list_images(dir);
    let mut n = 0;
    for it in &list {
        if fs::remove_file(dir.join(&it.name)).is_ok() {
            n += 1;
        }
    }
    n
}

// ---------- 下载 ----------
/// 走系统自带 curl（Win10 1803 起都有），失败再退 PowerShell。
pub fn download(url: &str, dest: &Path) -> Result<u64, String> {
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    let d = dest.to_string_lossy().to_string();

    // 用 output 而不是 status：curl 的 stderr 要留下来，不然失败只报「下载失败」，没法定位
    let mut detail = String::new();
    match Command::new("curl")
        .args(["-sS", "-L", "--max-time", "120", "--retry", "2", "-o", &d, url])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => {
            if !o.status.success() {
                let e = String::from_utf8_lossy(&o.stderr);
                detail = format!("curl 退出码 {:?}：{}", o.status.code(), e.chars().take(200).collect::<String>());
            } else if !dest.exists() {
                detail = "curl 返回成功但文件没落地".into();
            }
        }
        Err(e) => detail = format!("curl 启动失败：{}", e),
    }

    if !detail.is_empty() || !dest.exists() {
        let ps = format!(
            "try{{Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing}}catch{{exit 1}}",
            url.replace('\'', "''"),
            d.replace('\'', "''")
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    match fs::metadata(dest) {
        Ok(m) if m.len() > 0 => Ok(m.len()),
        _ => Err(if detail.is_empty() {
            "下载失败".to_string()
        } else {
            format!("下载失败（{}）", detail)
        }),
    }
}

pub fn md5_name(url: &str) -> String {
    let mut h = Md5::new();
    h.update(url.as_bytes());
    let hex = format!("{:x}", h.finalize());
    let ext = if url.split('?').next().unwrap_or("").to_lowercase().ends_with(".png") {
        "png"
    } else {
        "jpg"
    };
    format!("{}.{}", &hex[..16], ext)
}

/// 同 md5_name，但扩展名按 URL 推断，推断不出用 fallback（视频用 mp4）
pub fn md5_name_ext(url: &str, fallback: &str) -> String {
    let mut h = Md5::new();
    h.update(url.as_bytes());
    let hex = format!("{:x}", h.finalize());
    let path = url.split('?').next().unwrap_or("").to_lowercase();
    let ext = ["mp4", "webm", "mov", "png", "jpg", "jpeg", "webp"]
        .iter()
        .find(|e| path.ends_with(&format!(".{}", e)))
        .map(|e| e.to_string())
        .unwrap_or_else(|| fallback.to_string());
    format!("{}.{}", &hex[..16], ext)
}

// ---------- ego-browser 定位 ----------
pub fn find_ego(cfg: &Config) -> String {
    if !cfg.ego_path.trim().is_empty() {
        let p = PathBuf::from(cfg.ego_path.trim());
        if p.exists() {
            return p.to_string_lossy().to_string();
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        cands.push(PathBuf::from(&local).join(".local/share/ego/active_version_dir/ego-browser.exe"));
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        cands.push(PathBuf::from(&home).join(".local/share/ego/active_version_dir/ego-browser.exe"));
    }
    for root_key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(root) = std::env::var(root_key) {
            let base = PathBuf::from(root).join("Citro Labs").join("ego lite").join("Application");
            if let Ok(rd) = fs::read_dir(&base) {
                let mut versions: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                versions.sort();
                versions.reverse();
                for v in versions {
                    cands.push(v.join("ego-browser.exe"));
                }
            }
        }
    }
    for c in cands {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    "ego-browser".to_string()
}

pub fn ego_available(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------- Edge ----------
// ---------- 浏览器 ----------
/// 按优先级列出本机可用的 Chromium 内核浏览器（都支持 --app 独立窗口）
pub fn browser_candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if p.exists() && !v.iter().any(|x| x == &p) {
            v.push(p);
        }
    };
    let bases = ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"];
    let rels = [
        "Microsoft/Edge/Application/msedge.exe",
        "Google/Chrome/Application/chrome.exe",
        "360Chrome/Chrome/Application/360chrome.exe",
        "360/360se6/Application/360se.exe",
        "BraveSoftware/Brave-Browser/Application/brave.exe",
        "Vivaldi/Application/vivaldi.exe",
    ];
    for key in bases {
        if let Ok(base) = std::env::var(key) {
            for rel in rels {
                add(PathBuf::from(&base).join(rel));
            }
        }
    }
    v
}

pub fn edge_path() -> Option<PathBuf> {
    let mut cands = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
        cands.push(PathBuf::from(pf).join("Microsoft/Edge/Application/msedge.exe"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        cands.push(PathBuf::from(pf).join("Microsoft/Edge/Application/msedge.exe"));
    }
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        cands.push(PathBuf::from(la).join("Microsoft/Edge/Application/msedge.exe"));
    }
    cands.into_iter().find(|p| p.exists())
}

pub fn open_url(url: &str) {
    let _ = Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}
