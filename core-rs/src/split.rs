/// 把 [0, size) 切成尽量均匀的若干段（闭区间）。
///
/// 段数同时受 `min_part_size`（段不能太小）和 `max_part_count`（段数上限）约束。
/// 余数摊到前几段，保证各段长度最多相差 1 字节。
pub fn split_to_range(size: i64, min_part_size: i64, max_part_count: usize) -> Vec<(i64, i64)> {
    if size <= 0 {
        return Vec::new();
    }
    let min_part_size = min_part_size.max(1);
    let max_part_count = max_part_count.max(1) as i64;

    // 至少能切出多少个"最小块"
    let min_parts = (size + min_part_size - 1) / min_part_size;
    let actual = max_part_count.min(min_parts.max(1));

    let ideal = size / actual;
    let rem = size % actual;

    let mut ranges = Vec::with_capacity(actual as usize);
    let mut start = 0i64;
    for i in 0..actual {
        let mut length = ideal;
        if i < rem {
            length += 1;
        }
        let end = start + length - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_evenly_and_covers_all() {
        let r = split_to_range(10, 3, 4);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].0, 0);
        assert_eq!(r.last().unwrap().1, 9);
        let sum: i64 = r.iter().map(|(a, b)| b - a + 1).sum();
        assert_eq!(sum, 10);
        for i in 1..r.len() {
            assert_eq!(r[i].0, r[i - 1].1 + 1, "段之间必须连续");
        }
    }

    #[test]
    fn respects_min_part_size() {
        // 只有 2 字节，最小块 3 字节 → 只能切 1 段
        let r = split_to_range(2, 3, 8);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], (0, 1));
    }
}
