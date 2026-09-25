/*
 * demo.c —— 演示"一个 C 宿主程序如何调用下载核心"。
 *
 * 编译（MSVC）：
 *   cl /nologo /Fe:demo.exe /I..\include demo.c ..\target\release\downloadcore.lib ^
 *      ws2_32.lib userenv.lib bcrypt.lib ntdll.lib advapi32.lib ole32.lib shell32.lib crypt32.lib
 *
 * 运行：
 *   demo.exe <url> [输出目录]
 */
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include "downloadcore.h"

static void on_progress(void* ud, const dc_progress* p) {
    static int tick = 0;
    (void)ud;
    if (++tick % 5 != 0) return; /* 简单节流，免得刷屏 */
    double pct = p->total > 0 ? (double)p->downloaded / (double)p->total * 100.0 : 0.0;
    printf("  [progress] %5.1f%%  %.2f/%.2f MB  %.2f MB/s  parts=%zu\n",
           pct,
           (double)p->downloaded / 1048576.0,
           (double)p->total / 1048576.0,
           (double)p->speed / 1048576.0,
           p->parts);
    fflush(stdout);
}

static const char* status_name(int s) {
    switch (s) {
        case DC_STATUS_PENDING:     return "pending";
        case DC_STATUS_PROBING:     return "probing";
        case DC_STATUS_DOWNLOADING: return "downloading";
        case DC_STATUS_ASSEMBLING:  return "assembling";
        case DC_STATUS_COMPLETED:   return "completed";
        case DC_STATUS_FAILED:      return "failed";
        case DC_STATUS_CANCELED:    return "canceled";
        default:                    return "?";
    }
}

static void on_status(void* ud, int status) {
    (void)ud;
    printf("  [status] %s\n", status_name(status));
    fflush(stdout);
}

static void on_log(void* ud, int level, const char* message) {
    static const char* names[] = {"DEBUG", "INFO", "WARN", "ERROR"};
    const char* n = (level >= 0 && level <= 3) ? names[level] : "?";
    (void)ud;
    printf("  [%-5s] %s\n", n, message);
    fflush(stdout);
}

int main(int argc, char** argv) {
    printf("downloadcore C demo  version=%s  build=%s\n", dc_version(), dc_build_stamp());
    if (argc < 2) {
        printf("usage: demo <url> [output_dir]\n");
        return 2;
    }
    const char* url = argv[1];
    const char* dir = (argc >= 3) ? argv[2] : ".";

    dc_config cfg = dc_config_default();
    dc_engine* e = dc_engine_new(&cfg);
    if (!e) {
        printf("dc_engine_new failed\n");
        return 1;
    }

    dc_request req;
    memset(&req, 0, sizeof(req));
    req.url = url;
    req.target_dir = dir;

    char*    path  = NULL;
    int64_t  size  = 0;
    int64_t  speed = 0;
    size_t   parts = 0;
    char*    err   = NULL;

    printf("downloading: %s\n", url);
    int rc = dc_engine_download(e, &req, on_progress, on_status, on_log, NULL,
                                &path, &size, &speed, &parts, &err);
    if (rc == 0) {
        printf("OK: %s  size=%lld  avg=%.2f MB/s  parts=%zu\n",
               path ? path : "(null)", (long long)size, (double)speed / 1048576.0, parts);
    } else {
        printf("FAILED rc=%d: %s\n", rc, err ? err : "(no message)");
    }

    if (path) dc_string_free(path);
    if (err)  dc_string_free(err);
    dc_engine_free(e);
    return rc;
}
