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
        }
    }
}

impl Config {
    /// 把明显不合理的参数拉回可用范围。
    pub fn normalized(mut self) -> Config {
        if self.initial_threads == 0 {
            self.initial_threads = 1;
        }
        if self.max_threads < self.initial_threads {
            self.max_threads = self.initial_threads;
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
