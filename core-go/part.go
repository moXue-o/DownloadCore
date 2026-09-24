package downloadcore

import "sync"

// safetyStep 是"安全区"一次推进的步长，也是允许分裂所需的最小剩余量。
// 与 AB Download Manager 的 SAFE_ZONE_SIZE（128 * 8192 = 1 MiB）一致。
const safetyStep int64 = 1 << 20

// part 表示一个闭区间 [from, to] 的下载任务。
//
// 核心是"安全区"（safeZone）设计：
//   - part 只允许把数据读到 safeZone 为止；
//   - safeZone 随着下载逐步向前推进；
//   - 当 safeZone 之后还有一大块（>= safetyStep）没被"认领"时，就能把这块
//     一分为二，派新的 part 去下 —— 这就是"动态分段"；
//   - 分裂只动 safeZone 之外的部分，绝不会和正在读取的区间冲突。
//
// 因此"边下边分、实时加人"是安全、无重叠、无空洞的。
type part struct {
	mu       sync.Mutex
	from     int64
	to       int64 // 闭区间，可能被分裂缩小
	current  int64 // 下一个要读取的偏移
	safeZone int64 // 已经"认领"到的最大偏移（闭区间）
}

func newPart(from, to int64) *part {
	return &part{from: from, to: to, current: from, safeZone: from - 1}
}

// newPartFrom 用于续传：带上已经下到的位置。
func newPartFrom(from, to, current int64) *part {
	p := newPart(from, to)
	if current > from {
		p.current = current
		p.safeZone = current - 1
	}
	return p
}

// snapshot 一次性读出关键字段，避免多次加锁。
func (p *part) snapshot() (from, to, current, safeZone int64) {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.from, p.to, p.current, p.safeZone
}

func (p *part) done() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.current > p.to
}

// advance 把 current 向前推进 n 字节。
func (p *part) advance(n int64) {
	p.mu.Lock()
	p.current += n
	p.mu.Unlock()
}

// howMuchCanRead 返回当前允许读取的字节数；不够用时尝试推进 safeZone。
func (p *part) howMuchCanRead(want int64) int64 {
	p.mu.Lock()
	defer p.mu.Unlock()
	rem := p.safeZone + 1 - p.current
	if rem < 0 {
		rem = 0
	}
	if rem < want {
		if p.extendSafeZoneLocked() {
			rem = p.safeZone + 1 - p.current
			if rem < 0 {
				rem = 0
			}
		}
	}
	return rem
}

// extendSafeZone 是对外版本（自己加锁）。
func (p *part) extendSafeZone() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.extendSafeZoneLocked()
}

func (p *part) extendSafeZoneLocked() bool {
	remaining := p.to - p.current + 1
	if remaining <= 0 {
		return false
	}
	step := remaining
	if step > safetyStep {
		step = safetyStep
	}
	old := p.safeZone
	ns := old + step
	if ns > p.to {
		ns = p.to
	}
	if ns == old {
		return false
	}
	p.safeZone = ns
	return true
}

// splittableDelta 返回这个 part 还没被"认领"、可以拿去分裂的区域大小。
func (p *part) splittableDelta() int64 {
	p.mu.Lock()
	defer p.mu.Unlock()
	d := p.to - p.safeZone
	if d < 0 {
		return 0
	}
	return d
}

// canSplit 判断这个 part 现在能不能再切一刀（至少还有 safetyStep 未认领）。
func (p *part) canSplit() bool {
	return p.splittableDelta() >= safetyStep
}

// split 以默认阈值分裂（保留给测试与简单调用）。
func (p *part) split() *part {
	return p.splitAtLeast(safetyStep)
}

// splitAtLeast 只在"未认领区域 >= minDelta"时才分裂，
// 这样能避免切出越来越小的碎片段（碎片段会导致连接反复重建、速度上不去）。
func (p *part) splitAtLeast(minDelta int64) *part {
	p.mu.Lock()
	defer p.mu.Unlock()
	if minDelta < safetyStep {
		minDelta = safetyStep
	}
	if p.to-p.safeZone < minDelta {
		return nil
	}
	delta := p.to - p.safeZone
	// 向上取整的一半
	mid := p.safeZone + delta/2 + delta%2
	if mid+1 > p.to {
		return nil
	}
	np := newPart(mid+1, p.to)
	p.to = mid
	return np
}
