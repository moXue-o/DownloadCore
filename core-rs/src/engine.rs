use crate::backend::{build_pool, Backend, Endpoint, ProbeInfo};
use crate::config::Config;
use crate::errors::{fatal, retryable, Error, ErrorKind, Result, ERR_TOO_MANY_FAILURES};
use crate::limiter::Limiter;
use crate::part::{Part, SAFETY_STEP};
use crate::split::split_to_range;
use crate::store::{self, PartState, ResumeState, STATE_FILE_NAME};
use crate::types::{Callbacks, DownloadResult, Progress, Request, Status};
use crate::util::{filename_from_url, job_key, sanitize_name, Lock};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SLOW_WINDOW: Duration = Duration::from_secs(5);
const SLOW_MIN_BYTES: i64 = 512 << 10;
// 看门狗管到多小的剩余量：收尾的尾巴也别放任一条慢连接慢慢滴
const SLOW_REMAINING_MIN: i64 = 64 << 10;
/// 收尾阶段（活跃工人很少）允许把尾巴切到的最小粒度
const TAIL_MIN: i64 = 128 << 10;
/// 段数上限：防止"尾巴切细"把 parts 无限撑大（超大文件时的内存/序列化/O(n²) 保护）
const MAX_PARTS: usize = 4096;
/// 预分配上限：超过就不 set_len（防恶意服务器谎报超大 total 造成巨额占盘）
const MAX_PREALLOC: i64 = 1 << 40; // 1 TiB

/// 进程内"正在下载的目标文件"登记表：防止同一个目标被并发写坏。
static ACTIVE_TARGETS: std::sync::OnceLock<Lock<HashSet<String>>> = std::sync::OnceLock::new();

struct TargetGuard {
    final_key: String,
    marker_key: String,
}

impl Drop for TargetGuard {
    fn drop(&mut self) {
        if let Some(s) = ACTIVE_TARGETS.get() {
            let mut g = s.lock();
            g.remove(&self.final_key);
            g.remove(&self.marker_key);
        }
    }
}

/// 认领一个目标文件；已被别的下载任务占用则返回 None。
/// 同时登记"最终路径"和"`.part` 标记路径"，防止 A 与 A.part 互为别名时绕过互斥。
/// 返回 `Arc`：工人线程也持有副本，保证"只要还有工人在写，锁就不释放"。
fn acquire_target(final_key: &str, marker_key: &str) -> Option<Arc<TargetGuard>> {
    let set = ACTIVE_TARGETS.get_or_init(|| Lock::new(HashSet::new()));
    let mut g = set.lock();
    if g.contains(final_key) || g.contains(marker_key) {
        return None;
    }
    g.insert(final_key.to_string());
    g.insert(marker_key.to_string());
    Some(Arc::new(TargetGuard {
        final_key: final_key.to_string(),
        marker_key: marker_key.to_string(),
    }))
}
const MAX_SLOW_RECONNECTS: usize = 8;
// 绝对"卡死"线：低于它就重开（与整体快慢无关）
const STUCK_RATE: f64 = 20.0 * 1024.0;
// "公平份额"线：整体每连接超过它，才谈得上"这条被饿着"（避免争抢时误判）
const FAIR_RATE: f64 = 150.0 * 1024.0;

/// 工人线程池：无论正常结束还是 `run` 中途 panic，`Drop` 都先取消再 join，
/// 杜绝"宿主回调 panic → 主线程展开 → 工人被 detach 继续用回调/userdata"。
struct Workers {
    handles: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl Workers {
    fn new(shared: &Arc<Shared>) -> Self {
        Workers { handles: Vec::new(), shared: shared.clone() }
    }
    fn push(&mut self, h: JoinHandle<()>) {
        self.handles.push(h);
    }
    fn reap_finished(&mut self) {
        self.handles.retain(|h| !h.is_finished());
    }
    fn join_all(&mut self) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        self.join_all();
    }
}

/// 下载核心。只负责把一个 URL 下成一个文件。
///
/// 并发模型：**一个工人一条线程**（阻塞式），天然吻合"每段一条连接"的下载语义，
/// 也让"自研后端"和"大框架后端"能套同一个引擎。
pub struct Engine {
    cfg: Config,
    backend: Arc<dyn Backend>,
}

/// 编译期选择网络后端：默认自研（正式版），`--features backend-lts` 换大框架（LTS 版）。
#[cfg(feature = "backend-lts")]
fn make_backend(cfg: &Config) -> Arc<dyn Backend> {
    Arc::new(crate::client::LtsBackend::new(cfg).expect("创建 LTS 后端失败"))
}

#[cfg(not(feature = "backend-lts"))]
fn make_backend(cfg: &Config) -> Arc<dyn Backend> {
    Arc::new(crate::netclient::NetClient::new(cfg.user_agent.clone(), cfg.idle_timeout))
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        let cfg = cfg.normalized();
        let backend = make_backend(&cfg);
        Engine { cfg, backend }
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
        // LTS 后端内部用 block_on / blocking_recv；若宿主自己在异步运行时里调用，
        // 直接跑会 panic。检测到就换到独立线程执行（自研后端无此依赖）。
        #[cfg(feature = "backend-lts")]
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|scope| {
                match scope.spawn(|| self.run(req, cbs)).join() {
                    Ok(r) => r,
                    Err(_) => Err(fatal("internal", "下载线程 panic")),
                }
            });
        }
        self.run(req, cbs)
    }

    fn run(&self, req: Request, cbs: Callbacks) -> Result<DownloadResult> {
        let limiter = Arc::new(Limiter::new(self.cfg.max_speed));
        let shared = Arc::new(Shared::new(cbs, req.cancel.clone(), req.pause.clone()));
        *shared.req_url.lock() = crate::backend::strip_userinfo(&req.url);
        let start = Instant::now();

        shared.status(Status::Probing);
        // 建来源池（主地址 + 镜像/多 IP）并探路
        // 探路用的头：先剥掉用户的 Accept-Encoding（我们只发 identity；LTS 探路也走这里）
        let probe_headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("accept-encoding"))
            .cloned()
            .collect();
        let (eps, pi) = match build_pool(
            self.backend.as_ref(),
            &self.cfg,
            &req.url,
            &req.mirrors,
            &probe_headers,
            &|| shared.is_canceled(),
        ) {
            Ok(x) => x,
            Err(e) => {
                if shared.is_canceled() {
                    shared.logf("WARN", "任务在探路阶段被取消");
                    shared.status(Status::Canceled);
                    return Err(Error::canceled());
                }
                shared.status(Status::Failed);
                return Err(e);
            }
        };
        let n_src = eps.len().max(1);
        let labels: Vec<String> = eps.iter().map(|e| e.label.clone()).collect();
        let _ = shared.src_labels.set(labels);
        let _ = shared.src_bytes.set((0..n_src).map(|_| AtomicI64::new(0)).collect());
        let _ = shared.src_conns.set((0..n_src).map(|_| AtomicUsize::new(0)).collect());
        let _ = shared.endpoints.set(eps);
        let _ = shared.backend.set(self.backend.clone());
        let _ = shared.headers.set(req.headers.clone());
        shared.logf(
            "INFO",
            format!("下载来源：{n_src} 个（主地址 + 镜像/多 IP；网络后端={}）", self.backend.name()),
        );
        let size = pi.size.max(0);
        shared.total.store(size, Ordering::SeqCst);
        *shared.etag.lock() = pi.etag.clone();
        *shared.last_mod.lock() = pi.last_modified.clone();

        // 确定最终路径
        let (final_path, temp_dir) = match self.setup_paths(&req, &pi) {
            Ok(x) => x,
            Err(e) => {
                shared.status(Status::Failed);
                return Err(e);
            }
        };
        let marker = PathBuf::from(format!("{}{}", final_path.display(), self.cfg.incomplete_suffix));
        let _ = shared.out_path.set(marker.clone());

        // 防并发：同一个目标文件同时只允许一个下载任务写（含 `.part` 别名）
        let abs_key = target_key(&final_path);
        let marker_key = target_key(&marker);
        let _target_guard = match acquire_target(&abs_key, &marker_key) {
            Some(g) => g,
            None => {
                shared.logf("ERROR", "目标文件正在被另一个下载任务使用，已拒绝");
                shared.status(Status::Failed);
                return Err(fatal("busy", "目标文件正在被另一个下载任务使用"));
            }
        };
        let _ = shared.target_guard.set(_target_guard.clone());
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
            // 单线程模式不写续传状态：先清掉可能残留的旧分段状态与其输出文件，
            // 免得以后又支持分段时拿旧的状态去"续传"一个已被重写的文件。
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_file(&marker);
            shared.status(Status::Downloading);
            if let Err(e) = self.download_whole(&shared, &limiter, &marker) {
                // 取消要和分段分支一致：报 Canceled，不要报 Failed
                let st = if e.kind == ErrorKind::Canceled { Status::Canceled } else { Status::Failed };
                shared.status(st);
                return Err(e);
            }
            // 整文件模式：若探路给了大小，就核对实际字节，防止被截断还当成功
            let got = shared.downloaded.load(Ordering::SeqCst);
            if size > 0 && got != size {
                shared.logf(
                    "ERROR",
                    format!("下载字节数（{got}）与声明大小（{size}）不符，判定失败（可能被截断）"),
                );
                shared.status(Status::Failed);
                return Err(fatal("whole", "实际下载字节数与声明大小不符（可能被截断）"));
            }
            shared.logf("INFO", format!("网络统计：{}", self.backend.stats()));
            if let Err(e) = move_into_place(&marker, &final_path) {
                shared.status(Status::Failed);
                return Err(e);
            }
            shared.status(Status::Completed);
            return Ok(DownloadResult {
                path: final_path.display().to_string(),
                size: shared.downloaded.load(Ordering::SeqCst).max(size),
                speed: speed_of(shared.session_bytes.load(Ordering::SeqCst), start.elapsed()),
                parts: 0,
                range_ok: pi.range_ok,
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
        if let Err(e) = self.prepare_parts(&shared, &pi, &temp_dir, &marker) {
            shared.status(Status::Failed);
            return Err(e);
        }

        shared.status(Status::Downloading);

        let mut workers = Workers::new(&shared);
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
        // 诊断统计
        let mut last_stat = Instant::now();
        let mut prev_total = 0i64;
        let mut prev_src: Vec<i64> = Vec::new();

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
                    shared.active.store(shared.running.load(Ordering::SeqCst), Ordering::SeqCst);
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
            // 每 2 秒打一条"统计"：活跃数、整体速度、各来源用量（诊断腰斩/波动用）
            {
                let now = Instant::now();
                let dt = now.duration_since(last_stat).as_secs_f64();
                if dt >= 2.0 {
                    let total = shared.downloaded.load(Ordering::SeqCst);
                    let overall = (total - prev_total) as f64 / dt / 1048576.0;
                    let mut s = format!(
                        "统计：活跃 {}/{}  整体 {:.2} MB/s  分段 {}",
                        shared.running.load(Ordering::SeqCst),
                        target,
                        overall,
                        shared.parts.lock().len()
                    );
                    if let (Some(b), Some(l)) = (shared.src_bytes.get(), shared.src_labels.get()) {
                        if prev_src.len() != b.len() {
                            prev_src = vec![0; b.len()];
                        }
                        for (i, c) in b.iter().enumerate() {
                            let cur = c.load(Ordering::SeqCst);
                            let r = (cur - prev_src[i]) as f64 / dt / 1048576.0;
                            let conns = shared
                                .src_conns
                                .get()
                                .and_then(|v| v.get(i))
                                .map(|x| x.load(Ordering::SeqCst))
                                .unwrap_or(0);
                            s.push_str(&format!(
                                "  |  #{} {} {:.1}MB {:.2}MB/s conns={}",
                                i,
                                l.get(i).cloned().unwrap_or_default(),
                                cur as f64 / 1048576.0,
                                r,
                                conns
                            ));
                        }
                        prev_src = b.iter().map(|c| c.load(Ordering::SeqCst)).collect();
                    }
                    shared.logf("INFO", s);
                    last_stat = now;
                    prev_total = total;
                }
            }
            // 用空闲名额补工人（目标并发由自适应调整）
            while shared.running.load(Ordering::SeqCst) < target {
                let next = { shared.queue.lock().pop_front() };
                let part = match next {
                    Some(p) => p,
                    None => match split_one(&shared, &self.cfg) {
                        Some(np) => np,
                        None => break,
                    },
                };
                spawn_part(&mut workers, &shared, &limiter, &self.cfg, &marker, part);
            }
            if shared.running.load(Ordering::SeqCst) == 0 && shared.queue.lock().is_empty() {
                break;
            }
            // 回收已结束的线程
            workers.reap_finished();
            if last_state_save.elapsed() >= Duration::from_secs(1) {
                last_state_save = Instant::now();
                shared.save_state(&temp_dir);
            }
            thread::sleep(Duration::from_millis(200));
        }

        // 收工：等所有工人退出（取消 / 出错时它们会很快看到标志）
        workers.join_all();
        shared.logf("INFO", format!("网络统计：{}", self.backend.stats()));

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

        // 全部下完 → 直接把 .part 改名成正式文件（无拼装）
        let n_segments = { shared.parts.lock().len() };
        shared.status(Status::Assembling);
        if let Err(e) = move_into_place(&marker, &final_path) {
            shared.logf("ERROR", format!("改名失败：{e}"));
            shared.status(Status::Failed);
            return Err(e);
        }
        let _ = fs::remove_dir_all(&temp_dir);
        shared.status(Status::Completed);

        let downloaded = shared.downloaded.load(Ordering::SeqCst);
        let speed = speed_of(shared.session_bytes.load(Ordering::SeqCst), start.elapsed());
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

    fn setup_paths(&self, req: &Request, pi: &ProbeInfo) -> Result<(PathBuf, PathBuf)> {
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
        let key = target_key(&final_path);
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
        pi: &ProbeInfo,
        temp_dir: &Path,
        marker: &Path,
    ) -> Result<()> {
        create_dir_private(temp_dir).map_err(|e| fatal("mkdir", format!("创建临时目录失败: {e}")))?;

        if let Some(st) = store::load_state_file(&temp_dir.join(STATE_FILE_NAME)) {
            let has_validator = !pi.etag.is_empty() || !pi.last_modified.is_empty();
            // 输出文件长度校验：预分配过就要求满长；超大文件（跳过预分配）只要求不超过声明长
            let marker_len = marker.metadata().map(|m| m.len()).unwrap_or(0);
            let marker_len_ok = if pi.size <= MAX_PREALLOC {
                marker_len == pi.size as u64
            } else {
                marker_len <= pi.size as u64
            };
            let matches = st.version == 1
                && st.url == shared.url()
                && st.total == pi.size
                && st.etag == pi.etag
                && st.last_modified == pi.last_modified
                && !st.parts.is_empty()
                && marker.exists();
            let usable = matches && has_validator && marker_len_ok && parts_cover(&st.parts, pi.size);
            if usable {
                let mut parts = shared.parts.lock();
                let mut queue = shared.queue.lock();
                for ps in &st.parts {
                    let p = Part::new_with(ps.from, ps.to, ps.current.min(ps.to + 1));
                    let arc = Arc::new(Lock::new(p));
                    if !arc.lock().done() {
                        queue.push_back(arc.clone());
                    }
                    parts.push(arc);
                }
                if !parts.is_empty() {
                    // 回填"已完成字节"，让进度从正确位置起算（速度只看本次会话）
                    let done: i64 = st
                        .parts
                        .iter()
                        .map(|p| (p.current.min(p.to + 1) - p.from).max(0))
                        .sum();
                    shared.downloaded.store(done, Ordering::SeqCst);
                    shared.prog.lock().last_emit_bytes = done;
                    shared.logf(
                        "INFO",
                        format!("发现可续传记录：共 {} 段（已完成 {} 字节），继续下载未完成的部分", parts.len(), done),
                    );
                    return Ok(());
                }
            } else if matches && !has_validator {
                shared.logf(
                    "WARN",
                    "服务器未提供 ETag/Last-Modified，无法确认文件未变；为安全起见重新下载",
                );
            } else if matches {
                shared.logf("WARN", "续传记录与输出文件对不上（长度/覆盖不符），为安全起见重新下载");
            } else {
                shared.logf("WARN", "续传记录与服务器对不上（文件可能已变化），改为重新下载");
            }
        }

        // 全新开始：清空临时目录，建好输出文件并按总大小预分配
        let _ = fs::remove_dir_all(temp_dir);
        create_dir_private(temp_dir).map_err(|e| fatal("mkdir", format!("创建临时目录失败: {e}")))?;
        let _ = fs::remove_file(temp_dir.join(STATE_FILE_NAME)); // remove_dir_all 失败时兜底
        if let Some(parent) = marker.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        {
            let f = File::create(marker).map_err(|e| fatal("create", format!("创建输出文件失败: {e}")))?;
            if pi.size > 0 && pi.size <= MAX_PREALLOC {
                f.set_len(pi.size as u64)
                    .map_err(|e| fatal("preallocate", format!("预分配失败: {e}")))?;
            } else if pi.size > MAX_PREALLOC {
                shared.logf(
                    "WARN",
                    format!("声明大小异常大（{} 字节），跳过预分配（改为边下边增长）", pi.size),
                );
            }
        }

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

    fn download_whole(&self, shared: &Arc<Shared>, limiter: &Arc<Limiter>, marker: &Path) -> Result<()> {
        let mut last_err: Option<Error> = None;
        for attempt in 0..=self.cfg.max_retries {
            if shared.is_canceled() {
                return Err(Error::canceled());
            }
            if attempt > 0 {
                thread::sleep(self.cfg.retry_delay);
            }
            match self.download_whole_once(shared, limiter, marker) {
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

    fn download_whole_once(&self, shared: &Arc<Shared>, limiter: &Arc<Limiter>, marker: &Path) -> Result<()> {
        let (be, ep, src_idx) = shared.pick_source();
        let headers = shared.headers_for(&ep.url);
        let mut body = be.open_plain(&ep, &headers)?;
        let mut file = File::create(marker).map_err(|e| fatal("create", format!("创建文件失败: {e}")))?;
        shared.downloaded.store(0, Ordering::SeqCst);
        let mut buf = vec![0u8; self.cfg.buffer_size.max(64 * 1024)];
        loop {
            if shared.is_canceled() {
                return Err(Error::canceled());
            }
            if shared.is_paused() {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            match body.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    file.write_all(&buf[..n]).map_err(|e| fatal("write", format!("{e}")))?;
                    shared.add_downloaded(n as i64);
                    shared.add_src_bytes(src_idx, n as i64);
                    // 单线程模式也要限速（以前漏了）
                    if limiter.wait(n as i64, &|| shared.is_canceled()).is_err() {
                        return Err(Error::canceled());
                    }
                }
                Err(e) => return Err(retryable("read", format!("{e}"))),
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
    /// 本次会话真正新下的字节（进度要含续传已完成量，速度只看本次）
    session_bytes: AtomicI64,
    cancel: AtomicBool,
    external: Option<Arc<AtomicBool>>,
    external_pause: Option<Arc<AtomicBool>>,
    total: AtomicI64,
    // 整体速度（字节/秒）与活跃连接数，供"相对判断"使用
    global_rate: AtomicI64,
    active: AtomicUsize,
    // 真正在跑的任务数（用于补齐并发 + 统计）
    running: AtomicUsize,
    etag: Lock<String>,
    last_mod: Lock<String>,
    prog: Lock<ProgState>,
    log_mu: Lock<()>,
    // 网络后端 + 来源池（主地址 + 镜像/多 IP）与轮转计数
    backend: std::sync::OnceLock<Arc<dyn Backend>>,
    endpoints: std::sync::OnceLock<Vec<Endpoint>>,
    headers: std::sync::OnceLock<Vec<(String, String)>>,
    src_next: AtomicUsize,
    // 诊断用：每个来源的标签、累计字节、连接次数
    src_labels: std::sync::OnceLock<Vec<String>>,
    src_bytes: std::sync::OnceLock<Vec<AtomicI64>>,
    src_conns: std::sync::OnceLock<Vec<AtomicUsize>>,
    /// 目标文件锁：共享里也存一份，工人线程各持一份，防止主线程 panic 后提前放锁
    target_guard: std::sync::OnceLock<Arc<TargetGuard>>,
    /// 输出文件（.part）路径：save_state 前先 fsync 它，避免"状态超前于数据"
    out_path: std::sync::OnceLock<PathBuf>,
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
            session_bytes: AtomicI64::new(0),
            cancel: AtomicBool::new(false),
            external,
            external_pause,
            total: AtomicI64::new(0),
            global_rate: AtomicI64::new(0),
            active: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            etag: Lock::new(String::new()),
            last_mod: Lock::new(String::new()),
            prog: Lock::new(ProgState { last_emit: Instant::now(), last_emit_bytes: 0 }),
            log_mu: Lock::new(()),
            backend: std::sync::OnceLock::new(),
            endpoints: std::sync::OnceLock::new(),
            headers: std::sync::OnceLock::new(),
            src_next: AtomicUsize::new(0),
            src_labels: std::sync::OnceLock::new(),
            src_bytes: std::sync::OnceLock::new(),
            src_conns: std::sync::OnceLock::new(),
            target_guard: std::sync::OnceLock::new(),
            out_path: std::sync::OnceLock::new(),
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

    /// 轮换取一个下载来源（多 IP / 镜像之间轮转），返回其下标供统计
    fn pick_source(&self) -> (Arc<dyn Backend>, Endpoint, usize) {
        let be = self.backend.get().expect("backend 未初始化").clone();
        let eps = self.endpoints.get().expect("endpoints 未初始化");
        let n = eps.len().max(1);
        let idx = self.src_next.fetch_add(1, Ordering::SeqCst) % n;
        let ep = eps[idx].clone();
        if let Some(v) = self.src_conns.get() {
            if let Some(c) = v.get(idx) {
                c.fetch_add(1, Ordering::SeqCst);
            }
        }
        (be, ep, idx)
    }

    fn add_src_bytes(&self, idx: usize, n: i64) {
        if let Some(v) = self.src_bytes.get() {
            if let Some(c) = v.get(idx) {
                c.fetch_add(n, Ordering::SeqCst);
            }
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.headers.get().cloned().unwrap_or_default()
    }

    /// 取"给某个来源用的请求头"：跨域来源要剥掉敏感头（Authorization/Cookie 等）；
    /// 另外一律丢掉用户的 Accept-Encoding（我们只发 identity，免得服务器回压缩体）。
    fn headers_for(&self, url: &str) -> Vec<(String, String)> {
        let same = crate::backend::same_origin(&self.url(), url);
        let h = self.headers();
        let mut out =
            if same { h } else { crate::backend::redact_sensitive(&h) };
        out.retain(|(k, _)| !k.eq_ignore_ascii_case("accept-encoding"));
        out
    }

    /// 有"身份证"时用 `If-Range` 的值（优先 ETag，其次 Last-Modified）。
    fn if_range_value(&self) -> Option<String> {
        let e = self.etag.lock().clone();
        // 弱校验器（W/"..."）不能用于 If-Range（RFC 7233），退而用 Last-Modified
        if !e.is_empty() && !e.trim_start().starts_with("W/") {
            return Some(e);
        }
        let m = self.last_mod.lock().clone();
        if !m.is_empty() {
            Some(m)
        } else {
            None
        }
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
        self.session_bytes.fetch_add(n, Ordering::SeqCst);
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
        // 1) 先取快照（读各段 current）
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
        // 2) 再 fsync 输出文件：保证"快照覆盖的字节"都已落盘
        //    （顺序不能反：先 fsync 再取快照，仍会有"快照超前于落盘"的窗口）
        if let Some(p) = self.out_path.get() {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) {
                let _ = f.sync_all();
            }
        }
        // 3) 最后写状态文件（状态永远 ≤ 已落盘数据，最坏只是重下一点）
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
    // 改名之前先把数据落盘，避免"文件已改名、内容还在页缓存"的掉电风险
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(src) {
        let _ = f.sync_all();
    }
    // 直接改名（覆盖已存在的目标）。不要"先删后改"——那样中途失败会把旧文件也搞没。
    fs::rename(src, final_path).map_err(|e| fatal("rename", format!("改名失败: {e}")))
}

/// 建目录（Unix 下权限收紧为 0700，避免临时目录被同机其他用户读写/投毒）。
fn create_dir_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// 把最终路径归一化成稳定 key：Windows 大小写不敏感 → 统一小写，
/// 避免同一个文件因大小写不同被当成两个（并发写坏 / 临时目录分裂）。
fn target_key(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let s = abs.display().to_string();
    let s = if cfg!(windows) { s.to_lowercase() } else { s };
    job_key(&s)
}

/// 续传记录是否完整覆盖 [0, size-1]（无缺口、无越界）。不完整就宁可重下。
fn parts_cover(parts: &[PartState], size: i64) -> bool {
    if size <= 0 || parts.is_empty() {
        return false;
    }
    let mut v: Vec<(i64, i64)> = parts.iter().map(|p| (p.from, p.to)).collect();
    v.sort_unstable();
    let mut expect = 0i64;
    for (f, t) in v {
        if f != expect || t < f || t >= size {
            return false;
        }
        expect = t + 1;
    }
    expect == size
}

/// 从所有段里挑"剩下活最多"的那段来分裂。
fn split_one(shared: &Arc<Shared>, cfg: &Config) -> Option<Arc<Lock<Part>>> {
    // 段数封顶：别让超大切细把 parts 撑爆
    if shared.parts.lock().len() >= MAX_PARTS {
        return None;
    }
    // 收尾阶段（活跃工人很少）时，允许把尾巴切得更细，
    // 免得最后几 MB 只剩一条被限速的连接慢慢滴。
    let running = shared.running.load(Ordering::SeqCst);
    let min_delta = if running <= 2 { TAIL_MIN } else { cfg.min_part_size.max(SAFETY_STEP) };
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
    workers: &mut Workers,
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    out_file: &Path,
    part: Arc<Lock<Part>>,
) {
    let s = shared.clone();
    let l = limiter.clone();
    let cfg = cfg.clone();
    let dir = out_file.to_path_buf();
    let tg = shared.target_guard.get().cloned();
    let (from, to) = {
        let p = part.lock();
        (p.from, p.to)
    };
    s.logf("DEBUG", format!("工人启动：段 [{from}, {to}]"));
    s.running.fetch_add(1, Ordering::SeqCst);
    let handle = thread::spawn(move || {
        // 只要还有工人在写，就攥着目标文件锁（即便主线程已 panic 退出）
        let _tg = tg;
        // 计数守卫：无论正常结束还是 panic，都保证把 running 减回去（否则主循环会永久挂死）
        struct RunningGuard(Arc<Shared>);
        impl Drop for RunningGuard {
            fn drop(&mut self) {
                self.0.running.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _running = RunningGuard(s.clone());
        // panic 不能穿透到"没人接"的地方：捕获后当普通错误处理
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_part(&s, &l, &cfg, &dir, &part)
        }));
        match res {
            Ok(Ok(())) => s.logf("DEBUG", format!("工人结束（完成）：段 [{from}, {to}]")),
            Ok(Err(e)) if e.kind == ErrorKind::Canceled => {}
            Ok(Err(e)) => {
                // 先把"失败+取消"落实，再打日志：万一宿主的日志回调 panic，也不会丢错误状态
                let msg = format!("工人结束（出错）：段 [{from}, {to}]，错误={e}");
                {
                    let mut fe = s.first_err.lock();
                    if fe.is_none() {
                        *fe = Some(e);
                    }
                }
                s.cancel.store(true, Ordering::SeqCst);
                s.logf("WARN", msg);
            }
            Err(_) => {
                {
                    let mut fe = s.first_err.lock();
                    if fe.is_none() {
                        *fe = Some(fatal("internal", "下载线程 panic"));
                    }
                }
                s.cancel.store(true, Ordering::SeqCst);
                s.logf("ERROR", format!("工人 panic（已捕获）：段 [{from}, {to}]"));
            }
        }
    });
    workers.push(handle);
}

fn run_part(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    out_file: &Path,
    part: &Arc<Lock<Part>>,
) -> Result<()> {
    let mut last_err: Option<Error> = None;
    for attempt in 0..=cfg.max_retries {
        if shared.is_canceled() {
            return Err(Error::canceled());
        }
        if attempt > 0 {
            thread::sleep(cfg.retry_delay);
        }
        let (from, to, cur) = {
            let p = part.lock();
            (p.from, p.to, p.current)
        };
        match download_part_once(shared, limiter, cfg, out_file, part) {
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

fn download_part_once(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    out_file: &Path,
    part: &Arc<Lock<Part>>,
) -> Result<()> {
    let from = part.lock().from;
    let mut file = open_output_file(out_file)?;
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
        let (be, ep, src_idx) = shared.pick_source();
        let mut headers = shared.headers_for(&ep.url);
        // 带身份证：内容若在下载期间变了/多来源不一致，服务器会改回 200，我们据此报错而不是拼错
        if let Some(v) = shared.if_range_value() {
            headers.push(("If-Range".to_string(), v));
        }
        let body = match be.open_range(&ep, &headers, current, to) {
            Ok(b) => b,
            Err(e) => {
                shared.logf(
                    "WARN",
                    format!(
                        "打开分段连接失败（来源 {}）：段 [{from}, {to}]，从 {current} 开始，错误={e}",
                        ep.label
                    ),
                );
                return Err(e);
            }
        };
        let watch = reconnects < MAX_SLOW_RECONNECTS;
        match pump(shared, limiter, cfg, part, &mut file, body, watch, src_idx) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind == ErrorKind::Slow => {
                reconnects += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pump(
    shared: &Arc<Shared>,
    limiter: &Arc<Limiter>,
    cfg: &Config,
    part: &Arc<Lock<Part>>,
    file: &mut File,
    mut body: Box<dyn Read + Send>,
    watch_slow: bool,
    src_idx: usize,
) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut pending_off = 0usize;
    let mut window_start = Instant::now();
    let mut window_bytes = 0i64;
    let mut buf = vec![0u8; cfg.buffer_size.max(64 * 1024)];

    loop {
        if shared.is_canceled() {
            return Err(Error::canceled());
        }
        if shared.is_paused() {
            // 暂停：连接保持、不计数；恢复后继续
            window_start = Instant::now();
            window_bytes = 0;
            thread::sleep(Duration::from_millis(50));
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
            match body.read(&mut buf) {
                Ok(0) => {
                    if part.lock().done() {
                        return Ok(());
                    }
                    return Err(retryable("read", "连接提前结束"));
                }
                Ok(n) => {
                    pending.clear();
                    pending.extend_from_slice(&buf[..n]);
                    pending_off = 0;
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
                        shared.logf("WARN", "连接卡住（空闲超时）");
                    }
                    return Err(retryable("read", format!("{e}")));
                }
            }
        }

        let avail = (pending.len() - pending_off) as i64;
        let n = avail.min(allowed).max(0) as usize;
        if n == 0 {
            continue;
        }
        crate::util::write_all_at(file, &pending[pending_off..pending_off + n], current as u64)
            .map_err(|e| fatal("write", format!("{e}")))?;
        pending_off += n;
        part.lock().advance(n as i64);
        shared.add_downloaded(n as i64);
        shared.add_src_bytes(src_idx, n as i64);
        window_bytes += n as i64;
        if limiter
            .wait(n as i64, &|| shared.is_canceled())
            .is_err()
        {
            return Err(Error::canceled());
        }
        // 限速时，睡眠是"故意的"，不能算进慢连接窗口——否则会被自己限速判成"卡死"而乱重连
        if cfg.max_speed > 0 {
            window_start = Instant::now();
            window_bytes = 0;
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

fn open_output_file(marker: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker)
        .map_err(|e| fatal("open", format!("打开输出文件失败: {e}")))
}
