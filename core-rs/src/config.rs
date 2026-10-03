use std::path::PathBuf;
use std::time::Duration;

/// 中性客户端身份。
///
/// 实测：把 User-Agent 改成 Chrome 浏览器身份后，部分 CDN 反而返回 403
/// —— 因为 TLS 指纹和 Chrome 不一致，伪装浏览器会被反爬识别。
pub const DEFAULT_USER_AGENT: &str = "downloadcore/0.1";

/// 引擎的全部可调参数。将来对应 C ABI 里的一个配置结构体。
#[derive(Clone, Debug)]
pub struct Config {
    /// 开局铺开的工人数（连接数）
    pub initial_threads: usize,
    /// 动态分段最多能加到几个工人
    pub max_threads: usize,
    /// 最小分段大小；小于它就不再切
    pub min_part_size: i64,
    /// 每个工人的读写缓冲
    pub buffer_size: usize,
    /// 多久没数据进来就判定为卡住
    pub idle_timeout: Duration,
    /// 每一段最多重试次数
    pub max_retries: usize,
    /// 每次重试前的等待
    pub retry_delay: Duration,
    /// 临时文件根目录
    pub temp_dir: PathBuf,
    /// 未完成文件的标记后缀
    pub incomplete_suffix: String,
    /// 默认 User-Agent
    pub user_agent: String,
    /// 全局限速（字节/秒），0 表示不限
    pub max_speed: u64,
    /// 自适应并发：在 initial..max 之间自动找当前网络的最优点（false 则固定用 max_threads）
    pub adaptive_threads: bool,
    /// 把域名解析成多个 IP 并行使用（可绕过"单 IP 限速"）
    pub use_multiple_ips: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            initial_threads: 32,
            max_threads: 32,
            min_part_size: 1 << 20, // 1 MiB
            buffer_size: 256 << 10,
            idle_timeout: Duration::from_secs(15),
            max_retries: 10,
            retry_delay: Duration::from_secs(1),
            // 默认放系统临时目录，不在宿主当前目录留东西
            temp_dir: std::env::temp_dir().join("downloadcore"),
            incomplete_suffix: ".part".to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            max_speed: 0,
            // 默认像 AB 一样用固定并发；需要"只增不减爬坡"时再打开
            adaptive_threads: false,
            use_multiple_ips: true,
        }
    }
}

impl Config {
    /// 把明显不合理的参数拉回可用范围。
    pub fn normalized(mut self) -> Config {
        if self.max_threads == 0 {
            self.max_threads = 1;
        }
        if self.initial_threads == 0 {
            self.initial_threads = 1;
        }
        // 上限保护：防宿主传入超大值导致巨量分段/Vec 分配
        const MAX_WORKERS: usize = 1024;
        if self.initial_threads > MAX_WORKERS {
            self.initial_threads = MAX_WORKERS;
        }
        if self.max_threads > MAX_WORKERS {
            self.max_threads = MAX_WORKERS;
        }
        // 尊重"最多工人数"：initial 不该超过 max（而不是把 max 静默抬上去）
        if self.initial_threads > self.max_threads {
            self.initial_threads = self.max_threads;
        }
        if self.buffer_size == 0 {
            self.buffer_size = 256 << 10;
        }
        if self.min_part_size <= 0 {
            self.min_part_size = 1 << 20;
        }
        if self.retry_delay.is_zero() {
            self.retry_delay = Duration::from_secs(1);
        }
        // 读超时为 0 会让 set_read_timeout 静默失败 → 卡死；拉回默认
        if self.idle_timeout.is_zero() {
            self.idle_timeout = Duration::from_secs(15);
        }
        if self.temp_dir.as_os_str().is_empty() {
            self.temp_dir = std::env::temp_dir().join("downloadcore");
        }
        if self.incomplete_suffix.is_empty() {
            self.incomplete_suffix = ".part".to_string();
        }
        if self.user_agent.is_empty() {
            self.user_agent = DEFAULT_USER_AGENT.to_string();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_threads_is_the_cap() {
        // 宿主只设 max_threads=1（initial 留默认 32）时，不能把 max 静默抬回 32
        let mut c = Config::default();
        c.max_threads = 1;
        let n = c.normalized();
        assert_eq!(n.max_threads, 1);
        assert!(n.initial_threads <= 1);
    }

    #[test]
    fn idle_timeout_zero_is_replaced() {
        let mut c = Config::default();
        c.idle_timeout = Duration::ZERO;
        assert!(!c.normalized().idle_timeout.is_zero());
    }

    #[test]
    fn worker_count_is_capped() {
        let mut c = Config::default();
        c.initial_threads = usize::MAX;
        c.max_threads = usize::MAX;
        let n = c.normalized();
        assert!(n.initial_threads <= 1024);
        assert!(n.max_threads <= 1024);
    }
}
