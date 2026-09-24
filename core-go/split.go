package downloadcore

// splitToRange 把 [0, size) 切成尽量均匀的若干段（闭区间表示）。
//
// 段数同时受两个约束：
//   - minPartSize：段不能太小，否则切出一堆没意义的碎块；
//   - maxPartCount：段数上限（通常等于开局工人数）。
//
// 余数会摊到前几段上，保证各段长度最多相差 1 字节。
func splitToRange(size, minPartSize int64, maxPartCount int) [][2]int64 {
	if size <= 0 {
		return nil
	}
	if minPartSize < 1 {
		minPartSize = 1
	}
	if maxPartCount < 1 {
		maxPartCount = 1
	}

	// 至少能切出多少个"最小块"
	minParts := (size + minPartSize - 1) / minPartSize
	if minParts < 1 {
		minParts = 1
	}
	actual := int64(maxPartCount)
	if actual > minParts {
		actual = minParts
	}

	ideal := size / actual
	rem := size % actual

	ranges := make([][2]int64, 0, actual)
	var start int64
	for i := int64(0); i < actual; i++ {
		length := ideal
		if i < rem {
			length++
		}
		end := start + length - 1
		ranges = append(ranges, [2]int64{start, end})
		start = end + 1
	}
	return ranges
}
