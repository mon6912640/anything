//! 引擎：索引的生命周期（加载、重扫、实时更新、排名、保存）和对外的搜索接口。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::{Config, Rescan, index_path};
use crate::index::{F_DIR, F_NOREC, Index, NONE};
use crate::query::{self, Query, SortCol};
use crate::scan::{SubEntry, insert_subtree, is_ssd, join, scan_root, scan_subtree, should_recurse, stat, threads_for};
use crate::store;
use crate::util::{fold, group_digits, pinyin_initials, split_parent};
use crate::watch::{Ev, Info, Watchers};

const ATTR_DIR: u32 = 0x10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notice {
    /// 整个索引被替换（加载完成 / 重扫完成），之前的结果编号失效
    Replaced,
    /// 实时更新改动了索引
    Changed,
}

pub struct Hits {
    pub ids: Vec<u32>,
    pub generation: u64,
    pub millis: f64,
}

pub struct Engine {
    cfg: RwLock<Arc<Config>>,
    pub index: RwLock<Index>,
    /// 索引整体替换的次数
    pub generation: AtomicU64,
    /// 实时更新的次数
    pub changes: AtomicU64,
    pub scanning: AtomicBool,
    pub scan_progress: AtomicUsize,
    scan_cancel: AtomicBool,
    /// 重扫期间收到的事件，扫完后补到新索引上；同时作为“应用变更”的串行锁
    replay: Mutex<Option<Vec<Ev>>>,
    pending_roots: Mutex<Vec<String>>,
    rescan_after: Mutex<Vec<String>>,
    watchers: Mutex<Option<Watchers>>,
    tx: Sender<Ev>,
    rx: Mutex<Option<Receiver<Ev>>>,
    status: Mutex<String>,
    dirty: AtomicBool,
    save_lock: Mutex<()>,
    notify: Mutex<Option<Box<dyn Fn(Notice) + Send + Sync>>>,
    pub ready: AtomicBool,
}

impl Engine {
    pub fn new(cfg: Config) -> Arc<Engine> {
        let (tx, rx) = channel();
        Arc::new(Engine {
            cfg: RwLock::new(Arc::new(cfg)),
            index: RwLock::new(Index::new()),
            generation: AtomicU64::new(1),
            changes: AtomicU64::new(0),
            scanning: AtomicBool::new(false),
            scan_progress: AtomicUsize::new(0),
            scan_cancel: AtomicBool::new(false),
            replay: Mutex::new(None),
            pending_roots: Mutex::new(Vec::new()),
            rescan_after: Mutex::new(Vec::new()),
            watchers: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
            status: Mutex::new(String::new()),
            dirty: AtomicBool::new(false),
            save_lock: Mutex::new(()),
            notify: Mutex::new(None),
            ready: AtomicBool::new(false),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    pub fn set_notify(&self, f: Box<dyn Fn(Notice) + Send + Sync>) {
        *self.notify.lock().unwrap() = Some(f);
    }

    fn notify(&self, n: Notice) {
        if let Some(f) = self.notify.lock().unwrap().as_ref() {
            f(n);
        }
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }

    /// 后台启动：加载索引 → 开始监视 → （按配置）全量重扫
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("anything-init".into())
            .spawn(move || me.init())
            .unwrap();
    }

    fn init(self: Arc<Self>) {
        self.set_status("正在加载索引…");
        let t = Instant::now();
        let loaded = match store::load(&index_path()) {
            Ok(ix) => {
                let n = ix.alive();
                self.replace_index(ix);
                self.set_status(format!("已加载索引：{} 个对象（{:.1} 秒）", group_digits(n as u64), t.elapsed().as_secs_f64()));
                true
            }
            Err(_) => false,
        };
        if loaded {
            self.spawn_rank();
        }
        let rx = self.rx.lock().unwrap().take().unwrap();
        let me = self.clone();
        std::thread::Builder::new().name("anything-update".into()).spawn(move || me.update_loop(rx)).unwrap();
        let me = self.clone();
        std::thread::Builder::new().name("anything-maint".into()).spawn(move || me.maint_loop()).unwrap();
        self.restart_watchers();
        let cfg = self.cfg();
        let roots = cfg.effective_roots();
        let have: Vec<Vec<u8>> = {
            let ix = self.index.read().unwrap();
            (0..ix.len() as u32).filter(|&i| ix.parent[i as usize] == NONE && !ix.is_deleted(i)).map(|i| ix.folded_name(i).to_vec()).collect()
        };
        // 索引里还没有的根目录（首次运行、设置里新加的）必须扫；其余按 rescan_on_start
        let todo: Vec<String> = roots
            .into_iter()
            .filter(|r| {
                !have.contains(&fold(r))
                    || match cfg.rescan_on_start {
                        Rescan::All => true,
                        Rescan::Ssd => is_ssd(r) != Some(false),
                        Rescan::Off => false,
                    }
            })
            .collect();
        if !todo.is_empty() {
            self.rescan_roots(todo);
        }
    }

    fn replace_index(&self, ix: Index) {
        let old = {
            let mut g = self.index.write().unwrap();
            let old = std::mem::replace(&mut *g, ix);
            self.generation.fetch_add(1, SeqCst);
            old
        };
        drop(old);
        self.ready.store(true, SeqCst);
        self.notify(Notice::Replaced);
    }

    fn restart_watchers(&self) {
        let roots = self.cfg().effective_roots();
        let mut w = self.watchers.lock().unwrap();
        if let Some(old) = w.take() {
            old.stop();
        }
        *w = Some(Watchers::start(&roots, self.tx.clone()));
    }

    /// 重新读取配置文件：重新监视并重扫
    pub fn reload_config(self: &Arc<Self>) {
        *self.cfg.write().unwrap() = Arc::new(Config::load());
        self.restart_watchers();
        if self.scanning.load(SeqCst) {
            self.scan_cancel.store(true, SeqCst);
            // 等当前扫描退出后再开始新的
            let me = self.clone();
            std::thread::spawn(move || {
                while me.scanning.load(SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                me.rescan();
            });
        } else {
            self.rescan();
        }
    }

    /// 后台重扫所有根目录；扫描期间旧索引照常可搜，每个盘扫完原子替换
    pub fn rescan(self: &Arc<Self>) {
        self.rescan_roots(self.cfg().effective_roots());
    }

    /// 重扫指定的根目录（其余根目录保持不变）。正在扫描时排队，等这一轮结束再扫。
    pub fn rescan_roots(self: &Arc<Self>, roots: Vec<String>) {
        if roots.is_empty() {
            return;
        }
        if self.scanning.swap(true, SeqCst) {
            let mut q = self.rescan_after.lock().unwrap();
            for r in roots {
                if !q.contains(&r) {
                    q.push(r);
                }
            }
            return;
        }
        let me = self.clone();
        std::thread::Builder::new().name("anything-rescan".into()).spawn(move || me.do_rescan(roots)).unwrap();
    }

    fn do_rescan(self: &Arc<Self>, roots: Vec<String>) {
        *self.replay.lock().unwrap() = Some(Vec::new());
        let cfg = self.cfg();
        let all_roots = cfg.effective_roots();
        self.scan_progress.store(0, SeqCst);
        self.scan_cancel.store(false, SeqCst);
        *self.pending_roots.lock().unwrap() = roots.clone();
        let t = Instant::now();
        // 每个根目录独立扫描（各自的线程池），哪个先扫完就先并入索引：
        // 固态盘几十秒就能搜，不必等机械盘
        let (tx, rx) = channel::<(String, Index)>();
        for r in &roots {
            let (me, cfg, tx, r) = (self.clone(), cfg.clone(), tx.clone(), r.clone());
            std::thread::Builder::new()
                .name(format!("anything-rescan-{r}"))
                .spawn(move || {
                    let ix = scan_root(&r, &cfg, threads_for(&r, &cfg), &me.scan_progress, &me.scan_cancel, (0, 0));
                    let _ = tx.send((r, ix));
                })
                .unwrap();
        }
        drop(tx);
        let mut done = 0;
        while let Ok((root, part)) = rx.recv() {
            if self.scan_cancel.load(SeqCst) {
                continue;
            }
            done += 1;
            let n = part.len();
            self.publish_root(&root, part, &all_roots, done == roots.len(), &cfg);
            self.pending_roots.lock().unwrap().retain(|r| r != &root);
            self.set_status(format!("{root} 扫描完成：{} 个对象，用时 {:.1} 秒", group_digits(n as u64), t.elapsed().as_secs_f64()));
            self.spawn_rank();
        }
        self.pending_roots.lock().unwrap().clear();
        if self.scan_cancel.load(SeqCst) {
            *self.replay.lock().unwrap() = None;
            self.scanning.store(false, SeqCst);
            self.set_status("扫描已取消");
            return;
        }
        let n = self.index.read().unwrap().alive();
        self.set_status(format!("索引完成：{} 个对象，用时 {:.1} 秒", group_digits(n as u64), t.elapsed().as_secs_f64()));
        self.scanning.store(false, SeqCst);
        self.dirty.store(true, SeqCst);
        self.save();
        let again = std::mem::take(&mut *self.rescan_after.lock().unwrap());
        self.rescan_roots(again);
    }

    /// 用新扫描的一个根目录替换当前索引中的同名根目录
    fn publish_root(&self, root: &str, part: Index, roots: &[String], last: bool, cfg: &Config) {
        // 持有 replay 锁期间实时更新暂停，保证替换过程中不丢事件
        let mut replay = self.replay.lock().unwrap();
        let mut ix = {
            let cur = self.index.read().unwrap();
            let rf = fold(root);
            let wanted: Vec<Vec<u8>> = roots.iter().map(|r| fold(r)).collect();
            cur.extract(|name| {
                let f = fold(name);
                f != rf && wanted.contains(&f)
            })
        };
        ix.append(&part);
        drop(part);
        ix.rebuild_map();
        // 扫描期间这个根目录下发生的变化补上去
        let evs: Vec<Ev> = replay
            .as_ref()
            .map(|v| v.iter().filter(|e| e.root().eq_ignore_ascii_case(root)).cloned().collect())
            .unwrap_or_default();
        apply(&mut ix, &evs, cfg, HashMap::new());
        if last {
            *replay = None;
        }
        self.replace_index(ix);
    }

    /// 还在扫描中的根目录
    pub fn pending_roots(&self) -> Vec<String> {
        self.pending_roots.lock().unwrap().clone()
    }

    fn update_loop(self: Arc<Self>, rx: Receiver<Ev>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(e) => {
                        batch.push(e);
                        if batch.len() >= 200_000 {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            self.apply_batch(batch);
        }
    }

    fn apply_batch(self: &Arc<Self>, mut evs: Vec<Ev>) {
        let cfg = self.cfg();
        let mut overflow: Vec<String> = Vec::new();
        for e in &evs {
            if let Ev::Overflow { root } = e {
                if !overflow.iter().any(|r| r.as_str() == &**root) {
                    overflow.push(root.to_string());
                }
            }
        }
        evs.retain(|e| !matches!(e, Ev::Overflow { .. }));
        if !evs.is_empty() {
            // 新出现的目录先在锁外扫描子树，缩短写锁时间
            let mut pre: HashMap<usize, Vec<SubEntry>> = HashMap::new();
            for (k, e) in evs.iter().enumerate() {
                if let Ev::Add { root, rel, info: Some(i) } = e {
                    if i.attr & ATTR_DIR != 0 && should_recurse(i.attr, i.tag) {
                        let full = join(root, rel);
                        if !cfg.is_excluded_path(&full) {
                            pre.insert(k, scan_subtree(&full, &cfg));
                        }
                    }
                }
            }
            let mut replay = self.replay.lock().unwrap();
            {
                let mut ix = self.index.write().unwrap();
                apply(&mut ix, &evs, &cfg, pre);
            }
            if let Some(r) = replay.as_mut() {
                r.extend(evs);
            }
            drop(replay);
            self.changes.fetch_add(1, SeqCst);
            self.dirty.store(true, SeqCst);
            self.notify(Notice::Changed);
        }
        if !overflow.is_empty() {
            // 事件缓冲区溢出丢了变化，只重扫出问题的盘
            self.rescan_roots(overflow);
        }
    }

    fn spawn_rank(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || me.rank_now());
    }

    /// 重新计算排序名次（读锁下计算，写锁下替换）
    fn rank_now(&self) {
        let (generation, rn, rp) = {
            let ix = self.index.read().unwrap();
            let g = self.generation.load(SeqCst);
            let (rn, rp) = ix.compute_ranks();
            (g, rn, rp)
        };
        let mut ix = self.index.write().unwrap();
        if self.generation.load(SeqCst) == generation && rn.len() <= ix.len() {
            ix.rank_name = rn;
            ix.rank_path = rp;
        }
    }

    fn maint_loop(self: Arc<Self>) {
        let mut last_save = Instant::now();
        let mut last_rank = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if self.scanning.load(SeqCst) {
                continue;
            }
            // 新条目暂未参与排名也能正确排序（走较慢的直接比较），所以不必频繁重算
            let unranked = self.index.read().unwrap().unranked();
            if unranked >= 50_000 || (unranked > 0 && last_rank.elapsed() >= Duration::from_secs(600)) {
                self.rank_now();
                last_rank = Instant::now();
            }
            if self.dirty.load(SeqCst) && last_save.elapsed() >= Duration::from_secs(600) {
                self.save();
                last_save = Instant::now();
            }
        }
    }

    pub fn save(&self) {
        let _g = self.save_lock.lock().unwrap();
        let _ = std::fs::create_dir_all(crate::config::data_dir());
        self.dirty.store(false, SeqCst);
        let ok = {
            let ix = self.index.read().unwrap();
            !ix.is_empty() && store::save(&ix, &index_path()).is_ok()
        };
        if !ok {
            self.dirty.store(true, SeqCst);
        }
    }

    /// 退出前调用
    pub fn shutdown(&self) {
        self.scan_cancel.store(true, SeqCst);
        if let Some(w) = self.watchers.lock().unwrap().take() {
            w.stop();
        }
        if self.dirty.load(SeqCst) && !self.scanning.load(SeqCst) {
            self.save();
        }
    }

    pub fn search(&self, text: &str, col: SortCol, desc: bool, cancel: &(dyn Fn() -> bool + Sync)) -> Result<Option<Hits>, String> {
        let q = Query::parse(text, self.cfg().pinyin)?;
        let t = Instant::now();
        let ix = self.index.read().unwrap();
        let generation = self.generation.load(SeqCst);
        let Some(mut ids) = query::search(&ix, &q, cancel) else { return Ok(None) };
        if cancel() {
            return Ok(None);
        }
        query::sort(&ix, &mut ids, col, desc);
        Ok(Some(Hits { ids, generation, millis: t.elapsed().as_secs_f64() * 1000.0 }))
    }
}

fn find(ix: &Index, root: &str, rel: &str) -> Option<u32> {
    let (d, n) = split_parent(rel);
    let p = ix.resolve(root, d)?;
    ix.lookup(p, &fold(n))
}

fn add_path(ix: &mut Index, cfg: &Config, root: &str, rel: &str, info: Option<Info>, pre: Option<Vec<SubEntry>>) {
    let (d, n) = split_parent(rel);
    if n.is_empty() {
        return;
    }
    let dir_full = if d.is_empty() { root.to_string() } else { join(root, d) };
    if cfg.is_excluded_path(&dir_full) {
        return;
    }
    let Some(p) = ix.resolve(root, d) else { return };
    let full = join(&dir_full, n);
    let info = match info {
        Some(i) => i,
        None => match stat(&full) {
            Some(r) => Info::from(r),
            None => return,
        },
    };
    let folded = fold(n);
    let is_dir = info.attr & ATTR_DIR != 0;
    if let Some(e) = ix.lookup(p, &folded) {
        ix.mtime[e as usize] = info.mtime;
        if !is_dir {
            ix.size[e as usize] = info.size;
        }
        return;
    }
    let rec = is_dir && should_recurse(info.attr, info.tag) && !cfg.is_excluded_dir(&full, &folded);
    let flags = if is_dir { if rec { F_DIR } else { F_DIR | F_NOREC } } else { 0 };
    let py = if cfg.pinyin { pinyin_initials(n) } else { None };
    let id = ix.push(n, &folded, py.as_deref(), p, flags, if is_dir { 0 } else { info.size }, info.mtime);
    if rec {
        let subs = pre.unwrap_or_else(|| scan_subtree(&full, cfg));
        insert_subtree(ix, id, subs);
    }
}

/// 把一批文件系统事件应用到索引
pub fn apply(ix: &mut Index, evs: &[Ev], cfg: &Config, mut pre: HashMap<usize, Vec<SubEntry>>) {
    let mut need_prop = false;
    for (k, ev) in evs.iter().enumerate() {
        match ev {
            Ev::Add { root, rel, info } => add_path(ix, cfg, root, rel, *info, pre.remove(&k)),
            Ev::Del { root, rel } => {
                if let Some(e) = find(ix, root, rel) {
                    need_prop |= ix.is_dir(e);
                    ix.mark_deleted(e);
                }
            }
            Ev::Mod { root, rel, info } => {
                if let Some(e) = find(ix, root, rel) {
                    let info = info.or_else(|| stat(&join(root, rel)).map(Info::from));
                    if let Some(i) = info {
                        ix.mtime[e as usize] = i.mtime;
                        if !ix.is_dir(e) {
                            ix.size[e as usize] = i.size;
                        }
                    }
                }
            }
            Ev::Ren { root, old, new, info } => {
                let e = find(ix, root, old);
                let (nd, nn) = split_parent(new);
                let nd_full = if nd.is_empty() { root.to_string() } else { join(root, nd) };
                let np = if nn.is_empty() || cfg.is_excluded_path(&nd_full) { None } else { ix.resolve(root, nd) };
                match (e, np) {
                    (Some(e), Some(np)) => {
                        let folded = fold(nn);
                        if let Some(x) = ix.lookup(np, &folded) {
                            if x != e {
                                need_prop |= ix.is_dir(x);
                                ix.mark_deleted(x); // 覆盖了同名项
                            }
                        }
                        let py = if cfg.pinyin { pinyin_initials(nn) } else { None };
                        let (fl, size, mtime) = (ix.flags[e as usize], ix.size[e as usize], ix.mtime[e as usize]);
                        let mtime = info.map(|i| i.mtime).unwrap_or(mtime);
                        let id = ix.push(nn, &folded, py.as_deref(), np, fl, size, mtime);
                        if fl & F_DIR != 0 {
                            ix.repoint_children(e, id);
                        }
                        ix.mark_deleted(e);
                    }
                    (Some(e), None) => {
                        need_prop |= ix.is_dir(e);
                        ix.mark_deleted(e);
                    }
                    (None, Some(_)) => add_path(ix, cfg, root, new, *info, None),
                    (None, None) => {}
                }
            }
            Ev::Overflow { .. } => {}
        }
    }
    if need_prop {
        ix.propagate_deletes();
    }
}
