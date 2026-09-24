package downloadcore

import (
	"crypto/sha256"
	"fmt"
)

func sha256Sum(s string) [32]byte {
	return sha256.Sum256([]byte(s))
}

func hexEncode(b []byte) string {
	const h = "0123456789abcdef"
	out := make([]byte, len(b)*2)
	for i, c := range b {
		out[i*2] = h[c>>4]
		out[i*2+1] = h[c&0x0f]
	}
	return string(out)
}

// padFrom 把起始偏移补零，方便临时文件按名字就能排好序。
func padFrom(from int64) string {
	return fmt.Sprintf("%019d", from)
}

func min64(a, b int64) int64 {
	if a < b {
		return a
	}
	return b
}
