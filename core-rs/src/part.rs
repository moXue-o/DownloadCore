/// "安全区"一次推进的步长，也是允许分裂所需的最小剩余量。
/// 与 AB Download Manager 的 SAFE_ZONE_SIZE（128 * 8192 = 1 MiB）一致。
pub const SAFETY_STEP: i64 = 1 << 20;

/// 一个闭区间 [from, to] 的下载任务。
///
/// 核心是"安全区"（safe_zone）设计：
///   - 只允许把数据读到 safe_zone 为止；
///   - safe_zone 随下载逐步向前推进；
///   - 当 safe_zone 之后还剩一大块（>= SAFETY_STEP）没被"认领"时，
///     就能把这块一分为二，派新的 part —— 这就是"动态分段"；
///   - 分裂只动 safe_zone 之外的部分，绝不会和正在读取的区间冲突。
#[derive(Debug, Clone)]
pub struct Part {
    pub from: i64,
    pub to: i64,
    pub current: i64,
    pub safe_zone: i64,
}

impl Part {
    pub fn new(from: i64, to: i64) -> Self {
        Part { from, to, current: from, safe_zone: from - 1 }
    }

    /// 用于续传：带上已经下到的位置。
    pub fn new_with(from: i64, to: i64, current: i64) -> Self {
        let mut p = Part::new(from, to);
        if current > from {
            p.current = current;
            p.safe_zone = current - 1;
        }
        p
    }

    pub fn done(&self) -> bool {
        self.current > self.to
    }

    pub fn advance(&mut self, n: i64) {
        self.current += n;
    }

    /// 推进安全区，返回是否推进了。
    pub fn extend_safe_zone(&mut self) -> bool {
        let remaining = self.to - self.current + 1;
        if remaining <= 0 {
            return false;
        }
        let step = remaining.min(SAFETY_STEP);
        let old = self.safe_zone;
        let ns = (old + step).min(self.to);
        if ns == old {
            return false;
        }
        self.safe_zone = ns;
        true
    }

    /// 返回当前允许读取的字节数；不够用时尝试推进安全区。
    #[allow(dead_code)]
    pub fn how_much_can_read(&mut self, want: i64) -> i64 {
        let mut rem = (self.safe_zone + 1 - self.current).max(0);
        if rem < want && self.extend_safe_zone() {
            rem = (self.safe_zone + 1 - self.current).max(0);
        }
        rem
    }

    /// 还没被"认领"、可以拿去分裂的区域大小。
    pub fn splittable_delta(&self) -> i64 {
        (self.to - self.safe_zone).max(0)
    }

    /// 只在"未认领区域 >= min_delta"时才分裂；返回新的后半段。
    pub fn split_at_least(&mut self, min_delta: i64) -> Option<Part> {
        let min_delta = min_delta.max(SAFETY_STEP);
        if self.to - self.safe_zone < min_delta {
            return None;
        }
        let delta = self.to - self.safe_zone;
        // 向上取整的一半
        let mid = self.safe_zone + delta / 2 + delta % 2;
        if mid + 1 > self.to {
            return None;
        }
        let np = Part::new(mid + 1, self.to);
        self.to = mid;
        Some(np)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_prefix_and_safe_zone() {
        let orig_to = 10 * SAFETY_STEP - 1;
        let mut p = Part::new(0, orig_to);
        let safe_before = p.safe_zone;
        let np = p.split_at_least(SAFETY_STEP).expect("应当可以分裂");
        assert_eq!(np.from, p.to + 1, "两段必须首尾相接");
        assert_eq!(np.to, orig_to, "后半段应接到原末尾");
        assert!(p.to >= safe_before, "分裂后前半段不能小于安全区");
    }

    #[test]
    fn split_at_least_enforced() {
        let mut p = Part::new(0, SAFETY_STEP / 2 - 1);
        assert!(p.split_at_least(SAFETY_STEP).is_none());
    }
}
