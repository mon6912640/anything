//! ae.exe —— anything 的命令行接口（类似 Everything 的 es.exe），供脚本和第三方程序调用。
//!
//! anything 正在运行时通过命名管道查询（实时、毫秒级）；
//! 没有运行时自动读取上次保存的索引文件（-offline 可强制）。

use std::fs::File;
use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::process::ExitCode;

use anything::config::{Config, index_path};
use anything::ipc::{self, Request};
use anything::query::{self, Query, SortCol};
use anything::store;
use anything::util::filetime_to_local;

use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};

const USAGE: &str = "ae —— anything 命令行搜索（类似 Everything 的 es.exe）

用法: ae [选项] [搜索词...]

搜索词语法与 anything 界面相同，例如：
  ae *.psd                      ae ext:log dm:today
  ae \"my report\" !tmp           ae size:>100mb file:
  ae xmwj                       （拼音首字母）

选项：
  -n, -max-results <N>   最多输出 N 条
  -o, -offset <N>        跳过前 N 条
  -s, -sort <列>         排序：name（默认）| path | size | date | none
  -sort-name -sort-path -sort-size -sort-date   同上的简写
  -desc / -asc           倒序 / 正序
  -path <目录>           只搜索该目录下
  -regex <正则>          按正则匹配文件名
  -count                 只输出匹配数量
  -size                  输出大小列（字节）
  -dm                    输出修改时间列
  -name                  只输出文件名（不含路径）
  -csv | -tsv | -json    输出格式（默认每行一个完整路径）
  -0                     每条以 \\0 结尾（配合 xargs -0）
  -out <文件>            写入文件（UTF-8）而不是标准输出
  -ansi                  按系统代码页（中文系统为 GBK）输出，兼容老程序
  -offline               不连接 anything，直接读索引文件
  -status                显示索引状态
  -rescan                让正在运行的 anything 重建索引
  -h, -help              显示本帮助
  --                     之后的参数都当作搜索词

退出码：0 成功，1 参数错误，2 无法获取索引";

#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    Txt,
    Csv,
    Tsv,
    Json,
}

struct Opts {
    req: Request,
    fmt: Fmt,
    size: bool,
    dm: bool,
    name_only: bool,
    count: bool,
    nul: bool,
    out: Option<String>,
    ansi: bool,
    offline: bool,
    status: bool,
    rescan: bool,
}

struct Record<'a> {
    dir: bool,
    size: u64,
    mtime: u64,
    path: &'a str,
}

fn quote_term(s: &str) -> String {
    if s.chars().any(char::is_whitespace) && !s.contains('"') { format!("\"{s}\"") } else { s.to_string() }
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts {
        req: Request::default(),
        fmt: Fmt::Txt,
        size: false,
        dm: false,
        name_only: false,
        count: false,
        nul: false,
        out: None,
        ansi: false,
        offline: false,
        status: false,
        rescan: false,
    };
    let mut terms: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    let mut only_terms = false;
    while let Some(a) = args.next() {
        if only_terms || !(a.starts_with('-') && a.len() > 1) {
            terms.push(quote_term(&a));
            continue;
        }
        let key = a.trim_start_matches('-').to_ascii_lowercase();
        let mut val = |name: &str| args.next().ok_or_else(|| format!("{name} 需要一个参数"));
        match key.as_str() {
            "" => only_terms = true,
            "n" | "max-results" => o.req.max = val(&a)?.parse().map_err(|_| "-n 需要数字".to_string())?,
            "o" | "offset" => o.req.offset = val(&a)?.parse().map_err(|_| "-o 需要数字".to_string())?,
            "s" | "sort" => {
                let v = val(&a)?;
                o.req.sort = SortCol::parse(&v).ok_or_else(|| format!("未知的排序列: {v}"))?;
            }
            "sort-name" => o.req.sort = SortCol::Name,
            "sort-path" => o.req.sort = SortCol::Path,
            "sort-size" => o.req.sort = SortCol::Size,
            "sort-date" | "sort-dm" => o.req.sort = SortCol::Date,
            "desc" | "sort-descending" => o.req.desc = true,
            "asc" | "sort-ascending" => o.req.desc = false,
            "path" | "p" => terms.push(format!("\"path:{}\\\"", val(&a)?.trim_end_matches('\\'))),
            "regex" | "r" => terms.push(format!("\"regex:{}\"", val(&a)?)),
            "count" | "get-result-count" => o.count = true,
            "size" => o.size = true,
            "dm" | "date-modified" => o.dm = true,
            "name" => o.name_only = true,
            "csv" => o.fmt = Fmt::Csv,
            "tsv" => o.fmt = Fmt::Tsv,
            "json" => o.fmt = Fmt::Json,
            "txt" => o.fmt = Fmt::Txt,
            "0" => o.nul = true,
            "out" | "export" => o.out = Some(val(&a)?),
            "ansi" => o.ansi = true,
            "offline" => o.offline = true,
            "status" => o.status = true,
            "rescan" | "reindex" => o.rescan = true,
            "h" | "help" | "?" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("未知选项: {a}（ae -help 查看用法）")),
        }
    }
    o.req.q = terms.join(" ");
    if o.count {
        o.req.max = 0;
    }
    Ok(o)
}

fn iso_time(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    match filetime_to_local(ft) {
        Some(t) => format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond),
        None => String::new(),
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// 输出端：按需转为系统代码页
struct Sink {
    w: Box<dyn Write>,
    ansi: bool,
    wbuf: Vec<u16>,
    abuf: Vec<u8>,
}

impl Sink {
    fn put(&mut self, s: &str) -> io::Result<()> {
        if !self.ansi {
            return self.w.write_all(s.as_bytes());
        }
        self.wbuf.clear();
        self.wbuf.extend(s.encode_utf16());
        if self.wbuf.is_empty() {
            return Ok(());
        }
        self.abuf.resize(self.wbuf.len() * 4 + 4, 0);
        let n = unsafe {
            WideCharToMultiByte(CP_ACP, 0, self.wbuf.as_ptr(), self.wbuf.len() as i32, self.abuf.as_mut_ptr(), self.abuf.len() as i32, std::ptr::null(), std::ptr::null_mut())
        };
        self.w.write_all(&self.abuf[..n.max(0) as usize])
    }
}

struct Printer {
    sink: Sink,
    o: OptsView,
    first: bool,
}

#[derive(Clone, Copy)]
struct OptsView {
    fmt: Fmt,
    size: bool,
    dm: bool,
    name_only: bool,
    nul: bool,
}

impl Printer {
    fn begin(&mut self) -> io::Result<()> {
        let o = self.o;
        match o.fmt {
            Fmt::Json => self.sink.put("[\n"),
            Fmt::Csv | Fmt::Tsv => {
                let sep = if o.fmt == Fmt::Csv { "," } else { "\t" };
                let mut h = vec![if o.name_only { "Name" } else { "Path" }];
                if o.size {
                    h.push("Size");
                }
                if o.dm {
                    h.push("DateModified");
                }
                h.push("Type");
                self.sink.put(&(h.join(sep) + "\r\n"))
            }
            Fmt::Txt => Ok(()),
        }
    }

    fn record(&mut self, r: &Record) -> io::Result<()> {
        let o = self.o;
        let shown = if o.name_only { r.path.trim_end_matches('\\').rsplit('\\').next().unwrap_or(r.path) } else { r.path };
        let kind = if r.dir { "folder" } else { "file" };
        let end = if o.nul { "\0" } else { "\r\n" };
        let line = match o.fmt {
            Fmt::Txt => {
                let mut s = String::new();
                if o.size {
                    s.push_str(&format!("{:>15}  ", if r.dir { String::new() } else { r.size.to_string() }));
                }
                if o.dm {
                    s.push_str(&format!("{:<19}  ", iso_time(r.mtime)));
                }
                s.push_str(shown);
                s.push_str(end);
                s
            }
            Fmt::Csv | Fmt::Tsv => {
                let csv = o.fmt == Fmt::Csv;
                let f = |s: &str| if csv { csv_field(s) } else { s.to_string() };
                let mut cols = vec![f(shown)];
                if o.size {
                    cols.push(if r.dir { String::new() } else { r.size.to_string() });
                }
                if o.dm {
                    cols.push(iso_time(r.mtime));
                }
                cols.push(kind.to_string());
                cols.join(if csv { "," } else { "\t" }) + end
            }
            Fmt::Json => {
                let sep = if self.first { "  " } else { ",\n  " };
                let mut s = format!("{sep}{{\"path\":{}", json_str(r.path));
                if o.name_only {
                    s.push_str(&format!(",\"name\":{}", json_str(shown)));
                }
                s.push_str(&format!(",\"type\":\"{kind}\",\"size\":{},\"date_modified\":\"{}\"}}", if r.dir { 0 } else { r.size }, iso_time(r.mtime)));
                s
            }
        };
        self.first = false;
        self.sink.put(&line)
    }

    fn end(&mut self) -> io::Result<()> {
        if self.o.fmt == Fmt::Json {
            self.sink.put(if self.first { "]\n" } else { "\n]\n" })?;
        }
        self.sink.w.flush()
    }
}

fn run() -> Result<(), (u8, String)> {
    let o = parse_args().map_err(|e| (1, e))?;

    if o.status || o.rescan {
        let req = Request { cmd: (if o.rescan { "rescan" } else { "status" }).into(), ..Request::default() };
        let mut r = ipc::request(&req).ok_or((2, "anything 没有运行".to_string()))?.map_err(|e| (2, e.to_string()))?;
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| (2, e.to_string()))?;
        if o.rescan {
            println!("已通知 anything 重建索引");
        } else {
            println!("{}", ipc::describe_status(line.trim_end()));
        }
        return Ok(());
    }

    let to_console = o.out.is_none() && io::stdout().is_terminal();
    let w: Box<dyn Write> = match &o.out {
        Some(p) => Box::new(BufWriter::with_capacity(1 << 20, File::create(p).map_err(|e| (1, format!("无法创建 {p}: {e}")))?)),
        None => Box::new(BufWriter::with_capacity(1 << 16, io::stdout().lock())),
    };
    // 控制台本身支持 Unicode，只有重定向/管道输出时 -ansi 才有意义
    let sink = Sink { w, ansi: o.ansi && !to_console && o.out.is_none(), wbuf: Vec::new(), abuf: Vec::new() };
    let view = OptsView { fmt: o.fmt, size: o.size, dm: o.dm, name_only: o.name_only, nul: o.nul };
    let mut pr = Printer { sink, o: view, first: true };
    let io_err = |e: io::Error| {
        if e.kind() == io::ErrorKind::BrokenPipe { (0, String::new()) } else { (2, e.to_string()) }
    };

    // 优先查询正在运行的 anything
    if !o.offline
        && let Some(r) = ipc::request(&o.req) {
            let mut r = r.map_err(|e| (2, e.to_string()))?;
            let mut line = String::new();
            r.read_line(&mut line).map_err(|e| (2, e.to_string()))?;
            let head: Vec<&str> = line.trim_end().split('\t').collect();
            if head.first() != Some(&"OK") {
                return Err((1, head.get(1).unwrap_or(&"查询失败").to_string()));
            }
            let total: usize = head.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            if o.count {
                println!("{total}");
                return Ok(());
            }
            pr.begin().map_err(io_err)?;
            loop {
                line.clear();
                if r.read_line(&mut line).map_err(|e| (2, e.to_string()))? == 0 {
                    break;
                }
                let l = line.trim_end_matches(['\r', '\n']);
                let mut it = l.splitn(4, '\t');
                let (Some(k), Some(sz), Some(mt), Some(path)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
                let rec = Record { dir: k == "d", size: sz.parse().unwrap_or(0), mtime: mt.parse().unwrap_or(0), path };
                pr.record(&rec).map_err(io_err)?;
            }
            return pr.end().map_err(io_err);
        }

    // 离线：直接读索引文件
    let cfg = Config::load();
    let mut ix = store::load(&index_path()).map_err(|e| (2, format!("anything 没有运行，且无法读取索引文件 {}: {e}", index_path().display())))?;
    if !o.offline {
        eprintln!("提示：anything 没有运行，结果来自上次保存的索引");
    }
    let q = Query::parse(&o.req.q, cfg.pinyin).map_err(|e| (1, e))?;
    let mut ids = query::search(&ix, &q, &|| false).unwrap_or_default();
    if o.count {
        println!("{}", ids.len());
        return Ok(());
    }
    if matches!(o.req.sort, SortCol::Name | SortCol::Path) && ids.len() > 1 {
        let (rn, rp) = ix.compute_ranks();
        ix.rank_name = rn;
        ix.rank_path = rp;
    }
    query::sort(&ix, &mut ids, o.req.sort, o.req.desc);
    pr.begin().map_err(io_err)?;
    let end = o.req.offset.saturating_add(o.req.max).min(ids.len());
    for &id in ids.get(o.req.offset..end).unwrap_or(&[]) {
        let p = ix.full_path(id);
        let rec = Record { dir: ix.is_dir(id), size: ix.size[id as usize], mtime: ix.mtime[id as usize], path: &p };
        pr.record(&rec).map_err(io_err)?;
    }
    pr.end().map_err(io_err)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((0, _)) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("ae: {msg}");
            ExitCode::from(code)
        }
    }
}
