//! 编译资源（图标、manifest、版本信息）并链接进 anything.exe 和 ae.exe。
//! 使用 Windows SDK 自带的 rc.exe；找不到就直接编译失败（避免产出没图标的 exe）。

use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC_EXE") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut vers: Vec<PathBuf> = std::fs::read_dir(base).ok()?.flatten().map(|e| e.path()).filter(|p| p.join(r"x64\rc.exe").is_file()).collect();
    vers.sort();
    vers.pop().map(|p| p.join(r"x64\rc.exe"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=res");
    println!("cargo:rerun-if-env-changed=RC_EXE");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("anything.res");
    // 找不到 rc.exe 就直接编译失败：悄悄降级会产出一个没图标、没版本信息的 exe，
    // 这种包发出去很难被发现，宁可在这里报错。
    let rc = find_rc().unwrap_or_else(|| {
        panic!(
            "未找到 Windows SDK 的 rc.exe，无法把图标和版本信息嵌入 exe。\n\
             请安装 Visual Studio Build Tools 的 Windows SDK 组件，\n\
             或用环境变量 RC_EXE 指定 rc.exe 的完整路径后重新编译。"
        )
    });
    let status = Command::new(&rc)
        .args(["/nologo", "/fo"])
        .arg(&out)
        .arg("anything.rc")
        .current_dir("res")
        .status()
        .unwrap_or_else(|e| panic!("无法运行 {}: {e}", rc.display()));
    if !status.success() {
        panic!("rc.exe 编译资源失败（{}），详见上面的输出", rc.display());
    }
    for bin in ["anything", "ae"] {
        println!("cargo:rustc-link-arg-bin={bin}={}", out.display());
    }
}
