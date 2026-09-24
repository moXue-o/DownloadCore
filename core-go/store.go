package downloadcore

import (
	"encoding/json"
	"os"
	"path/filepath"
)

// partState 是单个分段的续传信息。
type partState struct {
	From    int64 `json:"from"`
	To      int64 `json:"to"`
	Current int64 `json:"current"`
}

// resumeState 是整份续传记录。
type resumeState struct {
	Version      int         `json:"version"`
	URL          string      `json:"url"`
	Total        int64       `json:"total"`
	ETag         string      `json:"etag"`
	LastModified string      `json:"last_modified"`
	Parts        []partState `json:"parts"`
}

const stateFileName = "state.json"

// saveStateFile 原子地写续传记录：先写临时文件，再改名。
// 这样即使在写入瞬间断电，也不会留下半截坏记录。
func saveStateFile(path string, st *resumeState) error {
	data, err := json.Marshal(st)
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	// 尽量把数据刷到磁盘，再原子改名
	if f, err := os.OpenFile(tmp, os.O_RDWR, 0); err == nil {
		_ = f.Sync()
		_ = f.Close()
	}
	return os.Rename(tmp, path)
}

// loadStateFile 读续传记录；不存在或损坏都返回 nil。
func loadStateFile(path string) (*resumeState, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var st resumeState
	if err := json.Unmarshal(data, &st); err != nil {
		return nil, err
	}
	return &st, nil
}

// jobKey 用目标文件的绝对路径算一个稳定的目录名，保证"同一个目标文件 → 同一份续传状态"。
func jobKey(targetAbs string) string {
	sum := sha256Sum(targetAbs)
	return hexEncode(sum[:8])
}

func partFileName(dir string, from int64) string {
	return filepath.Join(dir, padFrom(from))
}
