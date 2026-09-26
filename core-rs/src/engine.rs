use crate::client::{HttpClient, Source};
use crate::config::Config;
use crate::errors::{fatal, retryable, Error, ErrorKind, Result, ERR_TOO_MANY_FAILURES};
use crate::limiter::Limiter;
use crate::part::{Part, SAFETY_STEP};
use crate::split::split_to_range;
use crate::store::{self, PartState, ResumeState, STATE_FILE_NAME};
use crate::types::{Callbacks, DownloadResult, Progress, Request, Status};
use crate::util::{filename_from_url, job_key, part_file_name, sanitize_name, Lock};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SLOW_WINDOW: Duration = Duration::from_secs(5);
const SLOW_MIN_BYTES: i64 = 512 << 10;
const SLOW_REMAINING_MIN: i64 = 256 << 10;
const MAX_SLOW_RECONNECTS: usize = 8;
// 绝对"卡死"线：低于它就重开（与整体快慢无关）
const STUCK_RATE: f64 = 20.0 * 1024.0;
// "公平份额"线：整体每连接超过它，才谈得上"这条被饿着"（避免争抢时误判）
const FAIR_RATE: f64 = 150.0 * 1024.0;

/// 下载核心。只负责把一个 URL 下成一个文件。
pub struct Engine {
    cfg: Config,
    rt: tokio::runtime::Runtime,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        let cfg = cfg.normalized();
        // 运行时只建一次，反复下载复用（避免每次重建线程池的开销）
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.max_threads.clamp(4, 64))
            .enable_all()
            .build()
            .expect("无法创建 tokio 运行时");
        Engine { cfg, rt }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// 下载一个文件。阻塞式接口。
    /// 取消 / 暂停通过 `Request` 里的共享标志（可从别的线程调用）。
    pub fn download(&self, req: Request, cbs: Callbacks) -> Result<DownloadResult> {
        if req.url.is_empty() {
            return Err(fatal("request", "URL 为空"));
        }
        self.rt.block_on(self.run(req, cbs))
    }

    async fn run(&self, req: Request, cbs: Callbacks) -> Result<DownloadResult> {
        let limiter = Arc::new(Limiter::new(self.cfg.max_speed));
        let shared = Arc::new(Shared::new(cbs, req.cancel.clone(), req.pause.clone()));
        *shared.req_url.lock() = req.url.clone();
        let start = Instant::now();

        shared.status(Status::Probing);
        // 建来源池（主地址 + 镜像/多 IP）并探路
        let (http, pi) = match HttpClient::build(&self.cfg, &req.url, &req.mirrors).await {
            Ok(x) => x,
            Err(e) => {
                shared.status(Status::Failed);
                return Err(e);
            }
        };
        let http = Arc::new(http);
        let _ = shared.http.set(http);
        let _ = shared.headers.set(req.headers.clone());
        shared.logf(
            "INFO",
            format!(
                "下载来源：{} 个（主地址 + 镜像/多 IP）",
                shared.http.get().map(|h| h.source_count()).unwrap_or(1)
            ),
        );
        let size = pi.size.max(0);
        shared.total.store(size, Ordering::SeqCst);
        *shared.etag.lock() = pi.etag.clone();
        *shared.last_mod.lock() = pi.last_modified.clone();

        // 确定最终路径
        let (final_path, temp_dir) = self.setup_paths(&req, &pi)?;
        let marker = PathBuf::from(format!("{}{}", final_path.display(), self.cfg.incomplete_suffix));
        shared.logf(
            "INFO",
            format!(
                "探路完成：大小={size} 字节，支持分段={}，ETag=\"{}\"，最后修改=\"{}\"",
                pi.range_ok, pi.etag, pi.last_modified
            ),
        );
        shared.logf("INFO", format!("最终文件：{}", final_path.display()));

        // 服务器不支持分段（或大小未知）：退化为单线程整文件下载
        if !pi.range_ok || size <= 0 {
            shared.logf("INFO", "模式：单线程（服务器不支持分段或大小未知）");
            shared.status(Status::Downloading);
            if let Err(e) = self.download_whole(&shared, &marker).await
            {
                shared.status(Status::Failed);
                return Err(e);
            }
            if let Err(e) = move_into_place(&marker, &final_path) {
                shared.status(Status::Failed);
                return Err(e);
            }
            shared.status(Status::Completed);
            return Ok(DownloadResult {
                path: final_path.display().to_string(),
                size: shared.downloaded.load(Ordering::SeqCst).max(size),
                speed: speed_of(shared.downloaded.load(Ordering::SeqCst), start.elapsed()),
                parts: 0,
                range_ok: false,
            });
        }

        shared.logf(
            "INFO",
            format!(
                "模式：分段下载（开局 {} 路，最多 {} 路，最小段 {} 字节）",
                self.cfg.initial_threads, self.cfg.max_threads, self.cfg.min_part_size
            ),
        );

        // 分段模式
        self.prepare_parts(&shared, &pi, &temp_dir)?;

        shared.status(Status::Downloading);

        let mut joinset = tokio::task::JoinSet::new();
        let mut last_state_save = Instant::now();
        let mut rate_t = Instant::now();
        let mut rate_bytes = 0i64;
        // 自适应并发：从 initial 起步，每 2 秒按实测速度微调；关闭则固定用 max
        let mut target = if self.cfg.adaptive_threads {
            self.cfg.initial_threads.max(1)
        } else {
            self.cfg.max_threads
        };
        let mut last_ctrl = Instant::now();

        loop {
            if shared.is_canceled() {
                break;
            }
            if shared.first_err.lock().is_some() {
                shared.cancel.store(true, Ordering::SeqCst);
                break;
            }
            // 维护"整体速度 / 活跃连接数"，供慢连接做相对判断 + 自适应并发
            {
                let now = Instant::now();
                let dt = now.duration_since(rate_t).as_secs_f64();
                if dt >= 0.5 {
                    let bytes = shared.downloaded.load(Ordering::SeqCst);
                    shared
                        .global_rate
                        .store(((bytes - rate_bytes) as f64 / dt) as i64, Ordering::SeqCst);
                    shared.active.store(joinset.len(), Ordering::SeqCst);
                    rate_t = now;
                    rate_bytes = bytes;
                }
                // 自适应（可选）：只增不减地"爬坡"到目标，不做速度反馈 → 不会抖动
                if self.cfg.adaptive_threads && now.duration_since(last_ctrl) >= Duration::from_secs(2) {
                    if target < self.cfg.max_threads {
                        let step = (target / 4).max(1);
                        target = (target + step).min(self.cfg.max_threads);
                        shared.logf("DEBUG", format!("自适应并发：爬坡 → 目标 {target} 路"));
                    }
                    last_ctrl = now;
                }
            }
            // 用空闲名额补工人（目标并发由自适应调整）
            while joinset.len() < target {
                let next = { shared.queue.lock().pop_front() };
                let part = match next {
                    Some(p) => p,
                    None => match split_one(&shared, &self.cfg) {
                        Some(np) => np,
                        None => break,
                    },
                };
                spawn_part(&mut joinset, &shared, &limiter, &self.cfg, &temp_dir, part);
            }
            if joinset.is_empty() && shared.queue.lock().is_empty() {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(200), joinset.join_next()).await {
                Ok(_) => {}
                Err(_) => {
                    if last_state_save.elapsed() >= Duration::from_secs(1) {
                        last_state_save = Instant::now();
                        shared.save_state(&temp_dir);
                    }
                }
            }
        }

        if let Some(e) = shared.first_err.lock().take() {
            shared.save_state(&temp_dir);
            shared.logf("ERROR", format!("任务失败：{e}"));
            shared.status(Status::Failed);
            return Err(e);
        }
        if shared.is_canceled() {
            shared.save_state(&temp_dir);
            shared.logf("WARN", "任务被取消，已下进度已保留，可稍后续传");
            shared.status(Status::Canceled);
            return Err(Error::canceled());
        }

        // 全部下完 → 拼装
        let parts_snapshot: Vec<Part> = { shared.parts.lock().iter().map(|p| p.lock().clone()).collect() };
        let n_segments = parts_snapshot.len();
        let ranges = crate::assemble::parts_for_assemble(&parts_snapshot);
        shared.logf(
            "INFO",
            format!("所有分段下载完毕，开始拼装 {n_segments} 段 → {}", final_path.display()),
        );
        shared.status(Status::Assembling);
        if let Err(e) = crate::assemble::assemble(&temp_dir, &ranges, &marker, &final_path) {
            shared.logf("ERROR", format!("拼装失败：{e}"));
            shared.status(Status::Failed);
            return Err(e);
        }
        let _ = fs::remove_dir_all(&temp_dir);
        shared.status(Status::Completed);

        let downloaded = shared.downloaded.load(Ordering::SeqCst);
        let speed = speed_of(downloaded, start.elapsed());
        shared.logf(
            "INFO",
            format!(
                "下载完成：{}，共 {} 字节，用时 {:.2} 秒，平均 {:.2} MB/s，分段 {}",
                final_path.display(),
                downloaded,
                start.elapsed().as_secs_f64(),
                speed as f64 / 1024.0 / 1024.0,
                n_segments
            ),
        );
        if let Some(cb) = &shared.cbs.on_progress {
            cb(Progress { downloaded: size, total: size, speed, parts: n_segments });
        }
        Ok(DownloadResult {
            path: final_path.display().to_string(),
            size: if size > 0 { size } else { downloaded },
            speed,
            parts: n_segments,
            range_ok: true,
        })
    }

    fn setup_paths(
        &self,
        req: &Request,
        pi: &crate::client::ProbeInfo,
    ) -> Result<(PathBuf, PathBuf)> {
        let final_path = match &req.target_file {
            Some(f) if !f.is_empty() => PathBuf::from(f),
            _ => {
                let mut name = pi.file_name.clone();
                if name.is_empty() {
                    name = filename_from_url(&req.url);
                }
                if name.is_empty() {
                    name = "download.bin".to_string();
                }
                let dir = req
                    .target_dir
                    .clone()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| ".".to_string());
                PathBuf::from(dir).join(sanitize_name(&name))
            }
        };
        let abs = std::path::absolute(&final_path).unwrap_or_else(|_| final_path.clone());
        let key = job_key(&abs.display().to_string());
        let temp_dir = self.cfg.temp_dir.join(key);
        if let Some(parent) = final_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| fatal("mkdir", format!("创建目录失败: {e}")))?;
            }
        }
        Ok((final_path, temp_dir))
    }

    fn prepare_parts(
        &self,
        shared: &Arc<Shared>,
        pi: &crate::client::ProbeInfo,
        temp_dir: &Path,
    ) -> Result<()> {
        fs::create_dir_all(temp_dir).map_err(|e| fatal("mkdir", format!("创建临时目录失败: {e}")))?;

        if let Some(st) = store::load_state_file(&temp_dir.join(STATE_FILE_NAME)) {
            if st.version == 1
                && st.url == shared.url()
                && st.total == pi.size
                && st.etag == pi.etag
                && st.last_modified == pi.last_modified
                && !st.parts.is_empty()
                && part_files_usable(temp_dir, &st.parts)
            {
                let mut parts = shared.parts.lock();
                let mut queue = shared.queue.lock();
                for ps in &st.parts {
                    if ps.from < 0 || ps.to >= pi.size || ps.current < ps.from {
                        continue;
                    }
                    let p = Part::new_with(ps.from, ps.to, ps.current.min(ps.to + 1));
                    let arc = Arc::new(Lock::new(p));
                    if !arc.lock().done() {
                        queue.push_back(arc.clone());
                    }
                    parts.push(arc);
                }
                if !parts.is_empty() {
                    shared.logf("INFO", format!("发现可续传记录：共 {} 段，继续下载未完成的部分", parts.len()));
                    return Ok(());
                }
            } else {
                shared.logf("WARN", "续传记录与服务器对不上（文件可能已变化），改为重新下载");
            }
        }

        // 全新开始
        let _ = fs::remove_dir_all(temp_dir);
        fs::create_dir_all(temp_dir).map_err(|e| fatal("mkdir", format!("创建临时目录失败: {e}")))?;

        let ranges = if pi.size > self.cfg.min_part_size {
            split_to_range(pi.size, self.cfg.min_part_size, self.cfg.initial_threads)
        } else {
            vec![(0, pi.size - 1)]
        };
        {
            let mut parts = shared.parts.lock();
            let mut queue = shared.queue.lock();
            for (from, to) in &ranges {
                let arc = Arc::new(Lock::new(Part::new(*from, *to)));
                parts.push(arc.clone());
                queue.push_back(arc);
            }
        }
        shared.logf("INFO", format!("全新开始：切成 {} 段", ranges.len()));
        Ok(())
    }

    async fn download_whole(
        &self,
        shared: &Arc<Shared>,
        marker: &Path,
    ) -> Result<()> {
        let mut last_err: Option<Error> = None;
        for attempt in 0..=self.cfg.max_retries {
            if shared.is_canceled() {
                return Err(Error::canceled());
            }
            if attempt > 0 {
                tokio::time::sleep(self.cfg.retry_delay).await;
            }
            match self.download_whole_once(shared, marker).await {
                Ok(()) => return Ok(()),
                Err(e) if e.is_retryable() => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(fatal(
            "whole",
            format!("{ERR_TOO_MANY_FAILURES}: {}", last_err.map(|e| e.to_string()).unwrap_or_default()),
        ))
    }

    async fn download_whole_once(
        &self,
        shared: &Arc<Shared>,
        marker: &Path,
    ) -> Result<()> {
        let (http, source) = shared.pick_source();
        let headers = shared.headers();
        let mut resp = http.open_plain(&source, &headers).await?;
        let mut file = File::create(marker).map_err(|e| fatal("create", format!("创建文件失败: {e}")))?;
        shared.downloaded.store(0, Ordering::SeqCst);
        loop {
            if shared.is_canceled() {
                return Err(Error::canceled());
            }
            if shared.is_paused() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            match resp.chunk().await {
                Ok(Some(b)) => {
                    file.write_all(&b).map_err(|e| fatal("write", format!("{e}")))?;
                    shared.add_downloaded(b.len() as i64);
                }
                Ok(None) => return Ok(()),
                Err(e) => {
                    return Err(retryable("read", format!("{e}")));
                }
            }
        }
    }
}

/// 一次下载任务的共享状态。
struct Shared {
    req_url: Lock<String>,
    cbs: Callbacks,
    parts: Lock<Vec<Arc<Lock<Part>>>>,
    queue: Lock<VecDeque<Arc<Lock<Part>>>>,
    first_err: Lock<Option<Error>>,
    downloaded: AtomicI64,
    cancel: AtomicBool,
    external: Option<Arc<AtomicBool>>,
    external_pause: Option<Arc<AtomicBool>>,
    total: AtomicI64,
    // 整体速度（字节/秒）与活跃连接数，供"相对判断"使用
    global_rate: AtomicI64,
    active: AtomicUsize,
    etag: Lock<String>,
    last_mod: Lock<String>,
    prog: Lock<ProgState>,
    log_mu: Lock<()>,
    // 来源池（主地址 + 镜像/多 IP）与轮转计数
    http: std::sync::OnceLock<Arc<HttpClient>>,
    headers: std::sync::OnceLock<Vec<(String, String)>>,
    src_next: AtomicUsize,
}

struct ProgState {
    last_emit: Instant,
    last_emit_bytes: i64,
}

impl Shared {
    fn new(cbs: Callbacks, external: Option<Arc<AtomicBool>>, external_pause: Option<Arc<AtomicBool>>) -> Self {
        Shared {
            req_url: Lock::new(String::new()),
            cbs,
            parts: Lock::new(Vec::new()),
            queue: Lock::new(VecDeque::new()),
            first_err: Lock::new(None),
            downloaded: AtomicI64::new(0),
            cancel: AtomicBool::new(false),
            external,
            external_pause,
            total: AtomicI64::new(0),
            global_rate: AtomicI64::new(0),
            active: AtomicUsize::new(0),
            etag: Lock::new(String::new()),
            last_mod: Lock::new(String::new()),
            prog: Lock::new(ProgState { last_emit: Instant::now(), last_emit_bytes: 0 }),
            log_mu: Lock::new(()),
            http: std::sync::OnceLock::new(),
            headers: std::sync::OnceLock::new(),
            src_next: AtomicUsize::new(0),
        }
    }

    fn url(&self) -> String {
        self.req_url.lock().clone()
    }

    fn is_canceled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
            || self
                .external
                .as_ref()
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(false)
    }

    fn is_paused(&self) -> bool {
        self.external_pause
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// 轮换取一个下载来源（多 IP / 镜像之间轮转）
    fn pick_source(&self) -> (Arc<HttpClient>, Source) {
        let http = self.http.get().expect("http 未初始化").clone();
        let n = http.source_count().max(1);
        let idx = self.src_next.fetch_add(1, Ordering::SeqCst) % n;
        let s = http.source(idx).clone();
        (http, s)
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.headers.get().cloned().unwrap_or_default()
    }

    fn status(&self, s: Status) {
        if let Some(cb) = &self.cbs.on_status {
            cb(s);
        }
    }

    fn logf(&self, level: &'static str, msg: impl Into<String>) {
        if let Some(cb) = &self.cbs.on_log {
            let _g = self.log_mu.lock();
            cb(crate::types::LogEntry { level, message: msg.into() });
        }
    }

    fn add_downloaded(&self, n: i64) {
        let total = self.downloaded.fetch_add(n, Ordering::SeqCst) + n;
        let Some(cb) = &self.cbs.on_progress else { return };
        let now = Instant::now();
        let mut g = self.prog.lock();
        if now.duration_since(g.last_emit) < Duration::from_millis(200) {
            return;
        }
        let dt = now.duration_since(g.last_emit).as_secs_f64();
        let delta = total - g.last_emit_bytes;
        g.last_emit = now;
        g.last_emit_bytes = total;
        drop(g);
        let speed = if dt > 0.0 { (delta as f64 / dt) as i64 } else { 0 };
        let parts = self.parts.lock().len();
        cb(Progress { downloaded: total, total: self.total.load(Ordering::SeqCst), speed, parts });
    }

    fn save_state(&self, temp_dir: &Path) {
        let parts: Vec<PartState> = self
            .parts
            .lock()
            .iter()
            .map(|p| {
                let p = p.lock();
                let mut cur = p.current;
                if cur > p.to {
                    cur = p.to + 1;
                }
                PartState { from: p.from, to: p.to, current: cur }
            })
            .collect();
        let st = ResumeState {
            version: 1,
            url: self.url(),
            total: self.total.load(Ordering::SeqCst),
            etag: self.etag.lock().clone(),
            last_modified: self.last_mod.lock().clone(),
            parts,
        };
        if let Err(e) = store::save_state_file(&temp_dir.join(STATE_FILE_NAME), &st) {
            self.logf("WARN", format!("保存续传记录失败：{e}"));
        }
    }
}

fn speed_of(bytes: i64, elapsed: Duration) -> i64 {
    let s = elapsed.as_secs_f64();
    if s > 0.0 {
        (bytes as f64 / s) as i64
    } else {
        0
    }
}

fn move_into_place(src: &Path, final_path: &Path) -> Result<()> {
    let _ = fs::remove_file(final_path);
    fs::rename(src, final_path).map_err(|e| fatal("rename", format!("改名失败: {e}")))
}

/// 校验续传记录里的临时文件是否还在、是否够长。
fn part_files_usable(temp_dir: &Path, states: &[PartState]) -> bool {
    for ps in states {
        let mut need = ps.current - ps.from;
        if ps.current > ps.to {
            need = ps.to - ps.from + 1;
        }
        if need <= 0 {
            continue;
        }
        match fs::metadata(part_file_name(temp_dir, ps.from)) {
            Ok(m) if (m.len() as i64) >= need => {}
            _ => return false,
        }
    }
    true
}

/// 从所有段里挑"剩下活最多"的那段来分裂。
fn split_one(shared: &Arc<Shared>, cfg: &Config) -> Option<Arc<Lock<Part>>> {
    let min_delta = cfg.min_part_size.max(SAFETY_STEP);
    // 先选出目标段（锁的作用域到 block 结束就释放，避免重复加锁死锁）
    let best = {
        let parts = shared.parts.lock();
        let mut best: Option<Arc<Lock<Part>>> = None;
        let mut best_delta = 0i64;
        for p in parts.iter() {
            let d = p.lock().splittable_delta();
            if d >= min_delta && d > best_delta {
                best_delta = d;
                best = Some(p.clone());
            }
        }
        best
    };
    let best = best?;
    let np = best.lock().split_at_least(min_delta)?;
    let (from, to) = (np.from, np.to);
    let arc = Arc::new(Lock::new(np));
    shared.parts.lock().push(arc.clone());
    shared.logf("INFO", format!("动态分段：拆分最大段，新增 [{from}..={to}]"));
    Some(arc)
}

fn spawn_part(
    joinset: &mut tokio::task::JoinSet<()>,
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    temp_dir: &Path,
    part: Arc<Lock<Part>>,
) {
    let s = shared.clone();
    let l = limiter.clone();
    let cfg = cfg.clone();
    let dir = temp_dir.to_path_buf();
    let (from, to) = {
        let p = part.lock();
        (p.from, p.to)
    };
    s.logf("DEBUG", format!("工人启动：段 [{from}, {to}]"));
    joinset.spawn(async move {
        let res = run_part(&s, &l, &cfg, &dir, &part).await;
        match res {
            Ok(()) => s.logf("DEBUG", format!("工人结束（完成）：段 [{from}, {to}]")),
            Err(e) if e.kind == ErrorKind::Canceled => {}
            Err(e) => {
                s.logf("WARN", format!("工人结束（出错）：段 [{from}, {to}]，错误={e}"));
                let mut fe = s.first_err.lock();
                if fe.is_none() {
                    *fe = Some(e);
                }
                drop(fe);
                s.cancel.store(true, Ordering::SeqCst);
            }
        }
    });
}

async fn run_part(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    temp_dir: &Path,
    part: &Arc<Lock<Part>>,
) -> Result<()> {
    let mut last_err: Option<Error> = None;
    for attempt in 0..=cfg.max_retries {
        if shared.is_canceled() {
            return Err(Error::canceled());
        }
        if attempt > 0 {
            tokio::time::sleep(cfg.retry_delay).await;
        }
        let (from, to, cur) = {
            let p = part.lock();
            (p.from, p.to, p.current)
        };
        match download_part_once(shared, limiter, cfg, temp_dir, part).await {
            Ok(()) => {
                if part.lock().done() {
                    return Ok(());
                }
                last_err = Some(retryable("part", "连接提前结束"));
            }
            Err(e) if e.kind == ErrorKind::Retryable => {
                shared.logf(
                    "WARN",
                    format!("本段下载出错，准备重试（第 {} 次）：段 [{from}, {to}]，已到 {cur}，错误={e}", attempt + 1),
                );
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(fatal(
        "part",
        format!("{ERR_TOO_MANY_FAILURES}: {}", last_err.map(|e| e.to_string()).unwrap_or_default()),
    ))
}

async fn download_part_once(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    temp_dir: &Path,
    part: &Arc<Lock<Part>>,
) -> Result<()> {
    let from = part.lock().from;
    let mut file = open_part_file(temp_dir, part)?;
    let mut reconnects = 0usize;
    loop {
        let (to, current) = {
            let p = part.lock();
            (p.to, p.current)
        };
        if current > to {
            return Ok(());
        }
        // 每次（重）连接都轮换一个来源：多 IP / 镜像之间轮转
        let (http, source) = shared.pick_source();
        let headers = shared.headers();
        let resp = match http.open_range(&source, &headers, current, to).await {
            Ok(r) => r,
            Err(e) => {
                shared.logf(
                    "WARN",
                    format!(
                        "打开分段连接失败（来源 {}）：段 [{from}, {to}]，从 {current} 开始，错误={e}",
                        source.label
                    ),
                );
                return Err(e);
            }
        };
        file.seek(SeekFrom::Start((current - from) as u64))
            .map_err(|e| fatal("seek", format!("{e}")))?;
        let watch = reconnects < MAX_SLOW_RECONNECTS;
        match pump(shared, limiter, cfg, part, &mut file, resp, watch).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind == ErrorKind::Slow => {
                reconnects += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn pump(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    part: &Arc<Lock<Part>>,
    file: &mut File,
    mut resp: reqwest::Response,
    watch_slow: bool,
) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_off = 0usize;
    let mut window_start = Instant::now();
    let mut window_bytes = 0i64;
    let _ = cfg;

    loop {
        if shared.is_canceled() {
            return Err(Error::canceled());
        }
        if shared.is_paused() {
            // 暂停：连接保持、不计数；恢复后继续
            window_start = Instant::now();
            window_bytes = 0;
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let (to, current, safe_zone) = {
            let p = part.lock();
            (p.to, p.current, p.safe_zone)
        };
        if current > to {
            return Ok(());
        }
        let allowed = safe_zone + 1 - current;
        if allowed <= 0 {
            let mut p = part.lock();
            if !p.extend_safe_zone() {
                if p.current > p.to {
                    return Ok(());
                }
                return Err(retryable("part", "无法推进安全区"));
            }
            continue;
        }

        if pending_off >= pending.len() {
            match resp.chunk().await {
                Ok(Some(b)) => {
                    pending = b.to_vec();
                    pending_off = 0;
                    if pending.is_empty() {
                        continue;
                    }
                }
                Ok(None) => {
                    if part.lock().done() {
                        return Ok(());
                    }
                    return Err(retryable("read", "连接提前结束"));
                }
                Err(e) => {
                    if e.is_timeout() {
                        shared.logf("WARN", "连接卡住（空闲超时）");
                    }
                    return Err(retryable("read", format!("{e:?}")));
                }
            }
        }

        let avail = (pending.len() - pending_off) as i64;
        let n = avail.min(allowed).max(0) as usize;
        if n == 0 {
            continue;
        }
        file.write_all(&pending[pending_off..pending_off + n])
            .map_err(|e| fatal("write", format!("{e}")))?;
        pending_off += n;
        part.lock().advance(n as i64);
        shared.add_downloaded(n as i64);
        window_bytes += n as i64;
        if limiter
            .wait(n as i64, &|| shared.is_canceled())
            .is_err()
        {
            return Err(Error::canceled());
        }

        if watch_slow && window_start.elapsed() >= SLOW_WINDOW {
            let secs = window_start.elapsed().as_secs_f64();
            let remaining = {
                let p = part.lock();
                p.to - p.current + 1
            };
            if remaining > SLOW_REMAINING_MIN && secs > 0.0 {
                let conn_rate = window_bytes as f64 / secs;
                let global = shared.global_rate.load(Ordering::SeqCst) as f64;
                let active = shared.active.load(Ordering::SeqCst).max(1) as f64;
                let per_conn = global / active;
                // 只有"绝对卡死"，或"整体不慢但这只被明显饿着"才重开；
                // 争抢时大家一样慢 → per_conn 低 → 不折腾。
                let stuck = conn_rate < STUCK_RATE;
                let starved = per_conn > FAIR_RATE
                    && conn_rate < per_conn * 0.4
                    && conn_rate < SLOW_MIN_BYTES as f64 / secs;
                if stuck || starved {
                    shared.logf(
                        "WARN",
                        format!(
                            "连接过慢（{:.0} 秒仅下 {} KB，整体 {:.2} MB/s，还剩 {} KB），重开连接",
                            secs,
                            window_bytes / 1024,
                            global / 1024.0 / 1024.0,
                            remaining / 1024
                        ),
                    );
                    return Err(Error { kind: ErrorKind::Slow, op: "slow", message: String::new() });
                }
            }
            window_start = Instant::now();
            window_bytes = 0;
        }
    }
}

fn open_part_file(temp_dir: &Path, part: &Arc<Lock<Part>>) -> Result<File> {
    let (from, to) = {
        let p = part.lock();
        (p.from, p.to)
    };
    let path = part_file_name(temp_dir, from);
    let fresh = !path.exists();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| fatal("open", format!("打开分段文件失败: {e}")))?;
    if fresh {
        let len = (to - from + 1).max(0) as u64;
        file.set_len(len)
            .map_err(|e| fatal("preallocate", format!("预分配失败: {e}")))?;
    }
    Ok(file)
}
