// 编译期把 icon.ico 嵌进 exe（Windows 图标）。
//
// 做法：调用 GNU binutils 的 windres 把 ico 转成 COFF 资源对象，再把这个 .o
// 交给链接器，PE 里就带上 .rsrc 节，资源管理器/任务栏就能显示图标。
//
// 找不到 windres 也不影响编译——只是 exe 没有图标而已（会有一条 warning）。

use std::path::{Path, PathBuf};
use std::process::Command;

fn find_windres() -> Option<PathBuf> {
    // 1) PATH 里的 windres
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["windres.exe", "windres"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    // 2) rustc 的 sysroot 里（部分工具链自带 mingw-w64）
    if let Ok(sysroot) = std::env::var("RUSTC_SYSROOT") {
        let p = Path::new(&sysroot)
            .join("lib/rustlib/x86_64-pc-windows-gnu/bin/windres.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// windres 默认用 cpp 预处理 rc，这里找“原样输出”的替代：
/// 优先用 cat（msys2 / Git Bash 都带），否则用仓库里的 tools\rcpp.bat
fn preprocessor_candidates() -> Vec<String> {
    let mut list = Vec::new();
    if let Ok(v) = std::env::var("DOUBAO_ICO_PREPROCESSOR") {
        if !v.trim().is_empty() {
            list.push(v.trim().to_string());
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["cat.exe", "cat"] {
                if dir.join(name).is_file() {
                    list.push("cat".to_string());
                    break;
                }
            }
            if list.len() > 1 {
                break;
            }
        }
    }
    let bat = Path::new("tools").join("rcpp.bat");
    if bat.is_file() {
        let abs = std::fs::canonicalize(&bat).unwrap_or(bat);
        list.push(format!("cmd /c {}", abs.to_string_lossy()));
    }
    list
}

fn main() {
    println!("cargo:rerun-if-changed=icon.ico");
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let windres = match find_windres() {
        Some(w) => w,
        None => {
            println!("cargo:warning=找不到 windres，exe 不带图标（装 mingw-w64 binutils 可解决）");
            return;
        }
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let obj = out_dir.join("icon_res.o");

    // 逐个候选预处理器试，谁能真的把资源转出来就用谁
    for pre in preprocessor_candidates() {
        let status = Command::new(&windres)
            .arg("icon.rc")
            .arg("-O")
            .arg("coff")
            .arg(format!("--preprocessor={}", pre))
            .arg("-o")
            .arg(&obj)
            // msys2 的 windres 会把 /c 这类参数当成路径转换掉，这里关掉
            .env("MSYS2_ARG_CONV_EXCL", "*")
            .status();
        if matches!(status, Ok(s) if s.success()) && obj.is_file() {
            println!("cargo:rustc-link-arg={}", obj.display());
            return;
        }
    }
    println!("cargo:warning=windres 没能生成图标资源，exe 不带图标（不影响功能）");
}
