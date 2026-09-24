package downloadcore

import (
	neturl "net/url"
	"path"
	"path/filepath"
	"strings"
)

// fileNameFromURL 从网址路径里猜文件名。
func fileNameFromURL(raw string) string {
	u, err := neturl.Parse(raw)
	if err != nil {
		return ""
	}
	name := path.Base(u.Path)
	if name == "." || name == "/" || name == "" {
		return ""
	}
	if decoded, err := neturl.PathUnescape(name); err == nil {
		name = decoded
	}
	return name
}

// sanitizeName 去掉文件名里操作系统的非法字符，避免建不出文件。
func sanitizeName(name string) string {
	name = filepath.Base(name)
	replacer := strings.NewReplacer(
		"\\", "_", "/", "_", ":", "_", "*", "_",
		"?", "_", `"`, "_", "<", "_", ">", "_", "|", "_",
	)
	name = replacer.Replace(name)
	name = strings.Trim(name, " .")
	if name == "" || name == "." || name == ".." {
		return "download.bin"
	}
	return name
}
