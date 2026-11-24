//! 开发用：性能测试与自测（不用分发）
//!   anything-bench scan [根目录...]      扫描、存取、搜索计时
//!   anything-bench live                  实时更新正确性自测
//!   anything-bench stress [条目数]       合成大索引，对比新旧排序（默认 100 万）

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use anything::config::Config;
use anything::engine::apply;
use anything::index::Index;
use anything::query::{self, Query, SortCol};
use anything::scan::scan_roots;
use anything::store;
use anything::util::group_digits;
use anything::watch::Watchers;
use rayon::prelude::*;

fn private_mb() -> String {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut _, c.cb);
        format!("私有内存 {}，工作集 {}", mb(c.PrivateUsage), mb(c.WorkingSetSize))
    }
}

fn mb(b: usize) -> String {
    format!("{:.1} MB", b as f64 / 1048576.0)
}

fn run_query(ix: &Index, q: &str, sort: SortCol) -> (usize, f64, f64) {
    let query = Query::parse(q, true).unwrap();
    let t = Instant::now();
    let mut r = query::search(ix, &query, &|| false).unwrap();
    let ms_search = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    query::sort(ix, &mut r, sort, false);
    (r.len(), ms_search, t.elapsed().as_secs_f64() * 1000.0)
}

fn scan(args: &[String]) {
    let cfg = Config::load();
    let roots: Vec<String> = if args.is_empty() { cfg.effective_roots() } else { args.iter().map(|s| s.trim_end_matches('\\').to_string()).collect() };
    for r in &roots {
        println!("根目录 {r}: 固态={:?} 线程={}", anything::scan::is_ssd(r), anything::scan::threads_for(r, &cfg));
    }
    let prog = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let mut ix = scan_roots(&roots, &cfg, &prog, &cancel);
    let secs = t.elapsed().as_secs_f64();
    println!("扫描: {} 个对象, {:.2} 秒, {:.0} 个/秒", group_digits(ix.len() as u64), secs, ix.len() as f64 / secs);
    println!("  含拼音名字: {}", group_digits(ix.py_owner.len() as u64));
    let t = Instant::now();
    let (rn, rp) = ix.compute_ranks();
    ix.set_ranks(rn, rp);
    println!("排名计算: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    println!("内存估算: {}  (每条 {:.0} 字节)", mb(ix.mem_bytes()), ix.mem_bytes() as f64 / ix.len().max(1) as f64);

    let path = std::env::temp_dir().join("anything_bench.bin");
    let t = Instant::now();
    store::save(&ix, &path).unwrap();
    let size = std::fs::metadata(&path).unwrap().len() as usize;
    println!("保存: {:.0} ms, 文件 {}", t.elapsed().as_secs_f64() * 1000.0, mb(size));
    let t = Instant::now();
    let mut li = store::load(&path).unwrap();
    println!("加载: {:.0} ms (含重建查找表)", t.elapsed().as_secs_f64() * 1000.0);
    assert_eq!(li.len(), ix.len());
    let (rn, rp) = li.compute_ranks();
    li.set_ranks(rn, rp);
    drop(ix);
    println!("仅保留加载后的索引: {}", private_mb());

    println!("\n{:<34} {:>12} {:>10} {:>10}", "查询", "结果数", "搜索ms", "排序ms");
    let qs = [
        "a",
        "e",
        "readme",
        "config.json",
        "*.dll",
        "ext:png;jpg;gif",
        "windows\\ system32",
        "file: size:>100mb",
        "dm:today",
        "regex:^ab.*\\.txt$",
        "!a !e",
        "wj",
        "xm",
        "",
    ];
    for q in qs {
        // 预热一次再计时
        run_query(&li, q, SortCol::Name);
        let (n, ms, sms) = run_query(&li, q, SortCol::Name);
        println!("{:<34} {:>12} {:>10.1} {:>10.1}", format!("{q:?}"), group_digits(n as u64), ms, sms);
    }
    let (n, ms, sms) = run_query(&li, "e", SortCol::Path);
    println!("{:<34} {:>12} {:>10.1} {:>10.1}", "\"e\" 按路径排序", group_digits(n as u64), ms, sms);
    let (n, ms, sms) = run_query(&li, "e", SortCol::Size);
    println!("{:<34} {:>12} {:>10.1} {:>10.1}", "\"e\" 按大小排序", group_digits(n as u64), ms, sms);
    let _ = std::fs::remove_file(path);
}

fn names(ix: &Index, q: &str) -> Vec<String> {
    let query = Query::parse(q, true).unwrap();
    let mut r = query::search(ix, &query, &|| false).unwrap();
    query::sort(ix, &mut r, SortCol::Name, false);
    r.iter().map(|&i| ix.full_path(i)).collect()
}

fn check(ok: bool, what: &str) -> bool {
    println!("  [{}] {}", if ok { "通过" } else { "失败" }, what);
    ok
}

fn live() {
    let base = std::env::temp_dir().join("anything_live_test");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("老目录\\子目录")).unwrap();
    std::fs::write(base.join("老目录\\子目录\\深层文件.txt"), b"x").unwrap();
    std::fs::write(base.join("keep.txt"), b"x").unwrap();
    let root = base.to_string_lossy().to_string();
    let mut cfg = Config::default();
    cfg.threads = 2;
    let prog = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let mut ix = scan_roots(std::slice::from_ref(&root), &cfg, &prog, &cancel);
    println!("初始索引: {} 个对象", ix.alive());

    let (tx, rx) = channel();
    let w = Watchers::start(std::slice::from_ref(&root), tx);
    std::thread::sleep(Duration::from_millis(300));

    // 一系列文件操作
    std::fs::write(base.join("新建文件.md"), b"hello").unwrap();
    std::fs::create_dir_all(base.join("新目录\\a\\b")).unwrap();
    std::fs::write(base.join("新目录\\a\\b\\c.rs"), b"fn").unwrap();
    std::fs::rename(base.join("老目录"), base.join("改名目录")).unwrap();
    std::fs::remove_file(base.join("keep.txt")).unwrap();
    std::fs::write(base.join("新建文件.md"), vec![0u8; 5000]).unwrap();
    // 从外面“移入”一个已有内容的目录
    let outside = std::env::temp_dir().join("anything_live_outside");
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(outside.join("inner")).unwrap();
    std::fs::write(outside.join("inner\\moved.dat"), b"m").unwrap();
    std::fs::rename(&outside, base.join("移入目录")).unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let mut evs = Vec::new();
    while let Ok(e) = rx.try_recv() {
        evs.push(e);
    }
    println!("收到事件: {} 条", evs.len());
    apply(&mut ix, &evs, &cfg, HashMap::new());
    w.stop();

    let all = names(&ix, "");
    for p in &all {
        println!("    {p}");
    }
    let has = |s: &str| all.iter().any(|p| p.ends_with(s));
    let mut ok = true;
    ok &= check(has("\\新建文件.md"), "新建文件");
    ok &= check(has("\\新目录\\a\\b\\c.rs"), "新建多层目录及其中文件");
    ok &= check(has("\\改名目录\\子目录\\深层文件.txt"), "目录改名后子项路径跟着变");
    ok &= check(!all.iter().any(|p| p.contains("老目录")), "改名前的旧路径消失");
    ok &= check(!has("\\keep.txt"), "删除文件");
    ok &= check(has("\\移入目录\\inner\\moved.dat"), "从外部移入带内容的目录");
    let md = Query::parse("新建文件", true).unwrap();
    let r = query::search(&ix, &md, &|| false).unwrap();
    ok &= check(r.len() == 1 && ix.size[r[0] as usize] == 5000, "文件修改后大小更新");
    ok &= check(names(&ix, "xjwj").len() == 1, "拼音首字母 xjwj 搜到 新建文件");
    ok &= check(names(&ix, "gmml\\").is_empty() && names(&ix, "改名目录\\ 深层").len() == 1, "路径条件");

    // 存取后结果一致
    let path = std::env::temp_dir().join("anything_live_test.bin");
    store::save(&ix, &path).unwrap();
    let li = store::load(&path).unwrap();
    ok &= check(names(&li, "") == all, "保存再加载后内容一致（已删除条目被清理）");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(&base);
    println!("{}", if ok { "全部通过" } else { "存在失败项" });
    if !ok {
        std::process::exit(1);
    }
}

/// 第二阶段之前的排序实现，用于模拟压测的结果对照和耗时比较。
fn legacy_sort(ix: &Index, ids: &mut Vec<u32>, col: SortCol, desc: bool) {
    if ids.len() > 1 {
        match col {
            SortCol::None => {}
            SortCol::Name | SortCol::Path => {
                let rank = if col == SortCol::Name { &ix.rank_name } else { &ix.rank_path };
                let rl = rank.len() as u32;
                let mut keyed: Vec<u64> = ids
                    .par_iter()
                    .map(|&i| {
                        let r = if i < rl { rank[i as usize] } else { u32::MAX };
                        ((r as u64) << 32) | i as u64
                    })
                    .collect();
                keyed.par_sort_unstable();
                let tail = keyed.partition_point(|&k| (k >> 32) as u32 != u32::MAX);
                for (d, k) in ids.iter_mut().zip(keyed.iter()) {
                    *d = *k as u32;
                }
                ids[tail..].sort_unstable_by(|&a, &b| ix.folded_name(a).cmp(ix.folded_name(b)));
            }
            SortCol::Size | SortCol::Date => {
                let src = if col == SortCol::Size { &ix.size } else { &ix.mtime };
                let mut keyed: Vec<(u64, u32)> = ids.par_iter().map(|&i| (src[i as usize], i)).collect();
                keyed.par_sort_unstable();
                for (d, k) in ids.iter_mut().zip(keyed.iter()) {
                    *d = k.1;
                }
            }
        }
    }
    if desc {
        ids.reverse();
    }
}

/// 纯内存合成大索引，不访问真实文件系统；默认一百万个文件。
fn stress(args: &[String]) {
    let files: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    assert!(files >= 1_000, "压测条目数至少为 1000");
    let dir_count = (files / 4096).clamp(1, 1024);
    let t = Instant::now();
    let mut ix = Index::with_capacity(files + dir_count + 1, files * 24);
    let root = ix.push("Z:", b"z:", None, anything::index::NONE, anything::index::F_DIR, 0, 0);
    let mut dirs = Vec::with_capacity(dir_count);
    for d in 0..dir_count {
        let name = format!("dir{d:04}");
        dirs.push(ix.push(&name, name.as_bytes(), None, root, anything::index::F_DIR, 0, 0));
    }
    for i in 0..files {
        let stem = if i % 10 == 0 { "document" } else { "file" };
        let ext = if i % 5 == 0 { "rs" } else { "txt" };
        // 名称和父目录均与内部 ID 去相关，避免合成数据天然接近排序完成而偏袒旧算法。
        let shuffled = (i * 7919) % files;
        let name = format!("{stem}{shuffled:07}.{ext}");
        ix.push(&name, name.as_bytes(), None, dirs[shuffled % dir_count], 0, (i % 100_000) as u64, i as u64);
    }
    println!("合成索引: {} 个对象，生成 {:.0} ms", group_digits(ix.len() as u64), t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let (rn, rp) = ix.compute_ranks();
    ix.set_ranks(rn, rp);
    println!("排名及有序视图: {:.0} ms；索引内存估算 {}", t.elapsed().as_secs_f64() * 1000.0, mb(ix.mem_bytes()));

    let queries = ["file".to_string(), "document".to_string(), "file0000".to_string(), "dir0001\\ file".to_string(), "ext:txt".to_string(), "!absent".to_string()];
    println!("\n{:<20} {:<6} {:>12} {:>10} {:>10} {:>10} {:>9}", "查询", "排序", "结果数", "搜索ms", "新排序ms", "旧排序ms", "旧/新");
    for text in queries {
        let q = Query::parse(&text, false).unwrap();
        let ids = query::search(&ix, &q, &|| false).unwrap();
        let mut search_ms = f64::INFINITY;
        for _ in 0..3 {
            let t = Instant::now();
            let n = query::search(&ix, &q, &|| false).unwrap().len();
            assert_eq!(n, ids.len());
            search_ms = search_ms.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        for col in [SortCol::Name, SortCol::Path] {
            // 两种实现的正序和倒序必须逐项一致。
            let mut expected = ids.clone();
            legacy_sort(&ix, &mut expected, col, false);
            let mut actual = ids.clone();
            query::sort(&ix, &mut actual, col, false);
            assert_eq!(actual, expected, "正序结果不一致：{text:?} {col:?}");
            query::sort(&ix, &mut actual, col, true);
            expected.reverse();
            assert_eq!(actual, expected, "倒序结果不一致：{text:?} {col:?}");

            let mut new_ms = f64::INFINITY;
            let mut old_ms = f64::INFINITY;
            for _ in 0..3 {
                let mut v = ids.clone();
                let t = Instant::now();
                query::sort(&ix, &mut v, col, false);
                new_ms = new_ms.min(t.elapsed().as_secs_f64() * 1000.0);
                let mut v = ids.clone();
                let t = Instant::now();
                legacy_sort(&ix, &mut v, col, false);
                old_ms = old_ms.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            println!(
                "{:<20} {:<6} {:>12} {:>10.1} {:>10.1} {:>10.1} {:>8.2}x",
                format!("{text:?}"),
                if col == SortCol::Name { "名称" } else { "路径" },
                group_digits(ids.len() as u64),
                search_ms,
                new_ms,
                old_ms,
                old_ms / new_ms.max(0.001),
            );
        }
    }
}

/// 解码图片并输出信息；给出输出文件时把结果叠在棋盘格上存成 BMP，方便目测
fn pic(args: &[String]) {
    use anything::picture;
    picture::com_init();
    let (bw, bh) = (args.get(1).and_then(|s| s.parse().ok()).unwrap_or(600), args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600));
    let Some(path) = args.first() else { return };
    let t = Instant::now();
    match picture::load(path, bw, bh) {
        Ok(p) => {
            println!(
                "框 {}x{} -> {}x{} 原图 {}x{} {} {} alpha={} {:?} {:.1}ms",
                bw, bh, p.w, p.h, p.src_w, p.src_h, p.format, p.detail, p.alpha, p.kind, t.elapsed().as_secs_f64() * 1000.0
            );
            if let Some(out) = args.get(3) {
                let (w, h) = (p.w as usize, p.h as usize);
                let mut img = Vec::with_capacity(w * h * 3);
                for y in (0..h).rev() {
                    for x in 0..w {
                        let s = &p.bgra[(y * w + x) * 4..][..4];
                        let bg = if ((x / 8) + (y / 8)) % 2 == 0 { 255u32 } else { 220 };
                        for c in 0..3 {
                            img.push((s[c] as u32 + bg * (255 - s[3] as u32) / 255).min(255) as u8);
                        }
                    }
                    img.resize(img.len().next_multiple_of(4), 0);
                }
                let mut f = Vec::new();
                f.extend_from_slice(b"BM");
                f.extend_from_slice(&((54 + img.len()) as u32).to_le_bytes());
                f.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
                f.extend_from_slice(&(w as i32).to_le_bytes());
                f.extend_from_slice(&(h as i32).to_le_bytes());
                f.extend_from_slice(&[1, 0, 24, 0]);
                f.extend_from_slice(&[0u8; 24]);
                f.extend_from_slice(&img);
                std::fs::write(out, f).unwrap();
            }
        }
        Err(e) => println!("失败: {e} ({:.1}ms)", t.elapsed().as_secs_f64() * 1000.0),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scan") => scan(&args[1..]),
        Some("live") => live(),
        Some("stress") => stress(&args[1..]),
        Some("pic") => {
            for f in args[1..].iter().filter(|a| a.contains('.') && a.parse::<f64>().is_err() && !a.ends_with(".bmp")) {
                let mut a = vec![f.clone()];
                a.extend(args[1..].iter().filter(|x| x.parse::<u32>().is_ok() || x.ends_with(".bmp")).cloned());
                print!("{f}: ");
                pic(&a);
            }
        }
        _ => eprintln!("用法: anything-bench scan [根目录...] | live | stress [条目数] | pic <图片...> [宽 高] [输出.bmp]"),
    }
}
