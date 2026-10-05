/*
 * demo.c —— 测试程序（C 宿主版）：链接 downloadcore.lib，验证"库能被 C 程序装进去用"。
 *
 * 与 Rust 版示例（target\release\get.exe）功能对齐：
 *   - 启动显示版本号 + 编译时间戳
 *   - 提示输入网址（也可用命令行参数直接给），下到当前目录
 *   - 全程写详细日志到 download.log
 *
 * 编译（MSVC）：
 *   cd cdemo && build.bat
 * 运行：
 *   demo.exe                （交互式，提示输入网址）
 *   demo.exe <url> [输出目录]
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <assert.h>
#ifdef _WIN32
#include <windows.h>
#endif
#include "downloadcore.h"

/* 布局必须与 Rust 侧一致；对不上就编译报错，避免"静默内存错乱"。 */
static_assert(sizeof(dc_progress) == 32, "dc_progress layout mismatch");
static_assert(sizeof(dc_request) == 48, "dc_request layout mismatch");
static_assert(sizeof(dc_config) == 64, "dc_config layout mismatch");
static_assert(sizeof(dc_result) == 40, "dc_result layout mismatch");

/* 字段偏移也要锁死（只锁大小会漏掉"同类型字段被对调"） */
static_assert(offsetof(dc_config, min_part_size) == 8, "dc_config.min_part_size offset");
static_assert(offsetof(dc_config, buffer_size) == 16, "dc_config.buffer_size offset");
static_assert(offsetof(dc_config, idle_timeout_ms) == 20, "dc_config.idle_timeout_ms offset");
static_assert(offsetof(dc_config, max_retries) == 24, "dc_config.max_retries offset");
static_assert(offsetof(dc_config, retry_delay_ms) == 28, "dc_config.retry_delay_ms offset");
static_assert(offsetof(dc_request, expected_sha256) == 40, "dc_request.expected_sha256 offset");
static_assert(offsetof(dc_progress, total) == 8, "dc_progress.total offset");

static FILE* g_log = NULL;

/* 回调会被多个工人线程并发调用：日志与"每秒一条"的节流都要加锁 */
#ifdef _WIN32
static CRITICAL_SECTION g_log_cs;
static CRITICAL_SECTION g_prog_cs;
#define LOCK(cs)   EnterCriticalSection(&(cs))
#define UNLOCK(cs) LeaveCriticalSection(&(cs))
#else
#define LOCK(cs)   ((void)0)
#define UNLOCK(cs) ((void)0)
#endif

#ifdef _WIN32
static UINT g_old_cp = 0;
static void restore_console_cp(void) { SetConsoleOutputCP(g_old_cp); }
#endif

static void ts_now(char* out, size_t n) {
    time_t t = time(NULL);
    struct tm tmv;
    localtime_s(&tmv, &t);
    strftime(out, n, "%Y-%m-%d %H:%M:%S", &tmv);
}

/* 详细日志：控制台看 INFO 及以上，文件记全部（含 DEBUG）。 */
static void logline(const char* level, const char* msg) {
    char ts[32];
    ts_now(ts, sizeof ts);
    LOCK(g_log_cs);
    if (g_log) {
        fprintf(g_log, "%s [%s] %s\n", ts, level, msg);
        fflush(g_log);
    }
    if (strcmp(level, "DEBUG") != 0) {
        printf("%s [%s] %s\n", ts, level, msg);
        fflush(stdout);
    }
    UNLOCK(g_log_cs);
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
    char buf[64];
    (void)ud;
    snprintf(buf, sizeof buf, "状态：%s", status_name(status));
    logline("INFO", buf);
}

static void on_log(void* ud, int level, const char* message) {
    static const char* names[] = {"DEBUG", "INFO", "WARN", "ERROR"};
    const char* n = (level >= 0 && level <= 3) ? names[level] : "?";
    (void)ud;
    logline(n, message);
}

static void on_progress(void* ud, const dc_progress* p) {
    static time_t last = 0;
    time_t now = time(NULL);
    char buf[160];
    (void)ud;
    LOCK(g_prog_cs);
    int emit = (now != last); /* 每秒最多一条（多线程下也要原子地判断/更新） */
    if (emit) last = now;
    UNLOCK(g_prog_cs);
    if (!emit) return;
    double pct = p->total > 0 ? (double)p->downloaded / (double)p->total * 100.0 : 0.0;
    snprintf(buf, sizeof buf, "进度：%.1f%%  已下 %.2f/%.2f MB  %.2f MB/s  分段 %zu",
             pct,
             (double)p->downloaded / 1048576.0,
             (double)p->total / 1048576.0,
             (double)p->speed / 1048576.0,
             p->parts);
    logline("INFO", buf);
}

/* 返回 0 成功，非 0 失败 */
static int do_download(dc_engine* e, const char* url, const char* dir) {
    dc_request req;
    dc_result  res;
    char*      err = NULL;
    char       target[4096];
    int        rc;

    memset(&req, 0, sizeof(req));
    memset(&res, 0, sizeof(res));
    req.url = url;
    /* 核心不替宿主取名：这里自己取 URL 最后一段做文件名，拼到 dir 下 */
    {
        const char* q = strchr(url, '?');
        const char* end = q ? q : url + strlen(url);
        const char* slash = NULL;
        const char* p;
        for (p = url; p < end; p++) {
            if (*p == '/') slash = p;
        }
        if (slash && slash + 1 < end) {
            size_t len = (size_t)(end - (slash + 1));
            if (len > 255) len = 255;
            snprintf(target, sizeof target, "%s/%.*s", dir, (int)len, slash + 1);
        } else {
            snprintf(target, sizeof target, "%s/download.bin", dir);
        }
    }
    req.target_file = target;

    logline("INFO", "开始下载：");
    logline("INFO", url);

    rc = dc_engine_download(e, &req, on_progress, on_status, on_log, NULL, &res, &err);
    if (rc == 0) {
        char buf[256];
        snprintf(buf, sizeof buf, "下载成功：%s（%lld 字节，平均 %.2f MB/s，分段 %zu）",
                 res.path ? res.path : "(null)", (long long)res.size,
                 (double)res.speed / 1048576.0, res.parts);
        logline("INFO", buf);
        printf(">>> 下载完成：%s\n", res.path ? res.path : "(null)");
    } else {
        char buf[160];
        snprintf(buf, sizeof buf, "下载失败（错误码 %d）：%s", rc, err ? err : "(no message)");
        logline("ERROR", buf);
        printf(">>> 下载失败（错误码 %d）！详细原因见 download.log。\n", rc);
    }
    dc_result_free(&res);
    if (err) dc_string_free(err);
    return rc;
}

int main(int argc, char** argv) {
    char line[4096];
    dc_engine* e = NULL;
    int rc = 0;

#ifdef _WIN32
    /* 控制台默认是 936 码页，直接输出 UTF-8 会乱码；临时切 UTF-8，退出时还原 */
    g_old_cp = GetConsoleOutputCP();
    SetConsoleOutputCP(CP_UTF8);
    atexit(restore_console_cp);
    InitializeCriticalSection(&g_log_cs);
    InitializeCriticalSection(&g_prog_cs);
#endif

    /* 打开日志：新文件写 UTF-8 BOM，方便记事本识别中文 */
    {
        FILE* probe = fopen("download.log", "rb");
        int fresh = (probe == NULL);
        if (probe) fclose(probe);
        g_log = fopen("download.log", "ab");
        if (g_log && fresh) fwrite("\xEF\xBB\xBF", 1, 3, g_log);
    }

    printf("==============================================\n");
    printf("  简单下载器（C 宿主测试版）  version=%s  build=%s\n", dc_version(), dc_build_stamp());
    printf("==============================================\n");
    printf("文件保存到：当前目录\n");
    printf("详细日志：  download.log\n");
    printf("提示：直接回车退出。\n\n");
    {
        char buf[128];
        snprintf(buf, sizeof buf, "程序启动 version=%s build=%s（C 宿主测试版）", dc_version(), dc_build_stamp());
        logline("INFO", buf);
    }

    e = dc_engine_new(NULL); /* NULL = 用默认配置 */
    if (!e) {
        logline("ERROR", "创建引擎失败");
        printf(">>> 创建引擎失败。\n");
        return 1;
    }

    if (argc >= 2) {
        rc = do_download(e, argv[1], argc >= 3 ? argv[2] : ".");
    } else {
        for (;;) {
            printf("请输入下载网址（直接回车退出）：");
            fflush(stdout);
            if (!fgets(line, sizeof line, stdin)) break;
            size_t n = strlen(line);
            while (n > 0 && (line[n-1] == '\n' || line[n-1] == '\r')) line[--n] = '\0';
            if (n == 0) {
                logline("INFO", "用户选择退出");
                printf("已退出。\n");
                break;
            }
            do_download(e, line, ".");
            printf("\n");
        }
    }

    dc_engine_free(e);
    if (g_log) fclose(g_log);
    return rc;
}
