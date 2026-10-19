//! 编译资源（图标、manifest、版本信息）并链接进 anything.exe 和 ae.exe。
//! 使用 Windows SDK 自带的 rc.exe；找不到时退回只嵌入 manifest。

use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC_EXE") {
        return Some(PathBuf::from(p));
    }
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut vers: Vec<PathBuf> = std::fs::read_dir(base).ok()?.flatten().map(|e| e.path()).filter(|p| p.join(r"x64\rc.exe").is_file()).collect();
    vers.sort();
    vers.pop().map(|p| p.join(r"x64\rc.exe"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=res");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("anything.res");
    let ok = find_rc().is_some_and(|rc| {
        Command::new(rc)
            .args(["/nologo", "/fo"])
            .arg(&out)
            .arg("anything.rc")
            .current_dir("res")
            .status()
            .is_ok_and(|s| s.success())
    });
    for bin in ["anything", "ae"] {
        if ok {
            println!("cargo:rustc-link-arg-bin={bin}={}", out.display());
        } else {
            println!("cargo:warning=未找到 rc.exe，exe 将没有图标");
            let m = Path::new("res/anything.manifest").canonicalize().unwrap();
            println!("cargo:rustc-link-arg-bin={bin}=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bin={bin}=/MANIFESTINPUT:{}", m.display());
        }
    }
}
