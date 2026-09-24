package downloadcore

import (
	"io"
	"os"
	"sort"
)

// assemble 把所有分段临时文件按 from 顺序拼成成品。
//
// 只拷贝每段"实际下到的字节数"（current - from），
// 这样即使之前预分配过更大的文件、后来又被分裂缩小，也不会把多出来的 0 拷进去。
func (j *job) assemble() error {
	parts := make([]*part, len(j.parts))
	copy(parts, j.parts)
	sort.Slice(parts, func(a, b int) bool { return parts[a].from < parts[b].from })

	// 单段且正好覆盖整个文件：直接改名，省一次整文件拷贝
	if len(parts) == 1 && parts[0].from == 0 {
		_, _, cur, _ := parts[0].snapshot()
		if cur == j.total {
			return j.moveIntoPlace(partFileName(j.tempDir, 0))
		}
	}

	out, err := os.Create(j.marker)
	if err != nil {
		return fatal("assemble", err)
	}
	buf := make([]byte, 1<<20)
	for _, p := range parts {
		from, _, cur, _ := p.snapshot()
		length := cur - from
		if length <= 0 {
			continue
		}
		src, err := os.Open(partFileName(j.tempDir, from))
		if err != nil {
			out.Close()
			return fatal("assemble", err)
		}
		if _, err := io.CopyBuffer(out, io.LimitReader(src, length), buf); err != nil {
			src.Close()
			out.Close()
			return fatal("assemble", err)
		}
		src.Close()
	}
	if err := out.Sync(); err != nil {
		out.Close()
		return fatal("assemble", err)
	}
	if err := out.Close(); err != nil {
		return fatal("assemble", err)
	}
	return j.moveIntoPlace(j.marker)
}

// moveIntoPlace 把半成品改名成最终文件：下完才改名，用户不会把半成品当成成品。
func (j *job) moveIntoPlace(src string) error {
	_ = os.Remove(j.final)
	if err := os.Rename(src, j.final); err != nil {
		return fatal("rename", err)
	}
	return nil
}
