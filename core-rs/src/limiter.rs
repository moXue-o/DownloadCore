use crate::util::Lock;
use std::time::{Duration, Instant};

struct Inner {
    tokens: f64,
    last: Instant,
}

/// 简单的令牌桶全局限速器，max_speed = 0 表示不限速。
pub struct Limiter {
    max_speed: u64,
    burst: f64,
    inner: Lock<Inner>,
}

impl Limiter {
    pub fn new(max_speed: u64) -> Self {
        let burst = (1 << 20) as f64; // 1 MiB
        Limiter {
            max_speed,
            burst,
            inner: Lock::new(Inner { tokens: burst, last: Instant::now() }),
        }
    }

    /// 阻塞直到允许读取 n 字节，或被取消（返回 Err）。
    pub fn wait(&self, n: i64, canceled: &dyn Fn() -> bool) -> Result<(), ()> {
        if self.max_speed == 0 {
            return Ok(());
        }
        let n = n.max(0) as f64;
        loop {
            if canceled() {
                return Err(());
            }
            let mut g = self.inner.lock();
            let now = Instant::now();
            let elapsed = now.duration_since(g.last).as_secs_f64();
            g.last = now;
            g.tokens += self.max_speed as f64 * elapsed;
            // 单次请求可能超过 burst（比如缓冲调很大）：桶上限至少放到 n，否则永远满足不了
            let cap = self.burst.max(n);
            if g.tokens > cap {
                g.tokens = cap;
            }
            if g.tokens >= n {
                g.tokens -= n;
                return Ok(());
            }
            let need = (n - g.tokens) / self.max_speed as f64;
            drop(g);
            let mut d = Duration::from_secs_f64(need);
            if d < Duration::from_millis(1) {
                d = Duration::from_millis(1);
            }
            std::thread::sleep(d);
        }
    }
}
