/*
 * downloadcore.h —— 下载核心的 C 接口
 *
 * 用法（宿主集成三步）：
 *   1. dc_config cfg = dc_config_default();  // 按需改字段
 *      dc_engine* e = dc_engine_new(&cfg);
 *   2. 填 dc_request（url / 落盘位置 / 额外请求头）；
 *   3. 调 dc_engine_download(...)，挂上进度/状态/日志回调。
 *
 * 链接：把 downloadcore.lib 和下列系统库一起链进宿主：
 *   ws2_32.lib userenv.lib bcrypt.lib ntdll.lib advapi32.lib ole32.lib shell32.lib crypt32.lib
 *
 * 内存：本库返回的字符串（err_msg / out_path）用 dc_string_free 释放。
 */
#ifndef DOWNLOADCORE_H
#define DOWNLOADCORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 不透明句柄 */
typedef struct dc_engine dc_engine;

/* 状态，取值必须与 Rust 侧一致 */
typedef enum dc_status {
    DC_STATUS_PENDING = 0,
    DC_STATUS_PROBING = 1,
    DC_STATUS_DOWNLOADING = 2,
    DC_STATUS_ASSEMBLING = 3,
    DC_STATUS_COMPLETED = 4,
    DC_STATUS_FAILED = 5,
    DC_STATUS_CANCELED = 6
} dc_status;

typedef enum dc_log_level {
    DC_LOG_DEBUG = 0,
    DC_LOG_INFO = 1,
    DC_LOG_WARN = 2,
    DC_LOG_ERROR = 3
} dc_log_level;

/* 错误码：dc_engine_download 的返回值（0 = 成功） */
typedef enum dc_error {
    DC_OK = 0,
    DC_ERR_BUSY = 1,            /* 同一句柄已有下载在进行 */
    DC_ERR_CANCELED = 2,        /* 被取消 */
    DC_ERR_RETRY_EXHAUSTED = 3, /* 重试次数用尽 */
    DC_ERR_RANGE = 4,           /* 服务器分段与请求不一致 */
    DC_ERR_HTTP = 5,            /* 服务器返回异常状态 */
    DC_ERR_IO = 6,              /* 文件/磁盘错误 */
    DC_ERR_INVALID = 7,         /* 参数无效 */
    DC_ERR_INTERNAL = 8         /* 其它内部错误 */
} dc_error;

/* 下载结果（用 dc_result_free 释放其中的 path） */
typedef struct dc_result {
    char*   path;      /* 最终文件路径 */
    int64_t size;      /* 文件大小 */
    int64_t speed;     /* 平均速度（字节/秒） */
    size_t  parts;     /* 实际分段数 */
    int     range_ok;  /* 是否支持分段 */
} dc_result;

typedef struct dc_progress {
    int64_t downloaded; /* 已下载字节 */
    int64_t total;      /* 总大小；未知为 0 */
    int64_t speed;      /* 瞬时速度（字节/秒），原始值，平滑由宿主决定 */
    size_t  parts;      /* 当前分段数 */
} dc_progress;

typedef struct dc_config {
    int      initial_threads;   /* 开局工人数；<=0 用默认 */
    int      max_threads;       /* 最多工人数；<=0 用默认 */
    int64_t  min_part_size;     /* 最小分段；<=0 用默认 */
    int      buffer_size;       /* 读写缓冲；<=0 用默认 */
    int      idle_timeout_ms;   /* 空闲超时（毫秒）；<=0 用默认 */
    int      max_retries;       /* 每段最大重试；<0 用默认 */
    int      retry_delay_ms;    /* 重试等待（毫秒）；<=0 用默认 */
    const char* temp_dir;       /* 临时目录；NULL/空 用默认（系统临时目录） */
    const char* incomplete_suffix; /* 未完成后缀；NULL/空 用默认 */
    const char* user_agent;     /* NULL/空 用默认 */
    uint64_t max_speed;         /* 全局限速（字节/秒），0 不限 */
    int      adaptive_threads;  /* 1=并发在 initial..max 之间自动找最优（默认） */
    int      use_multiple_ips;  /* 1=域名解析成多个 IP 并行（默认） */
} dc_config;

typedef struct dc_request {
    const char* url;            /* 必填 */
    const char* target_file;    /* 指定文件名；NULL 则自动取名 */
    const char* target_dir;     /* 自动取名时存到哪个目录；NULL 表示当前目录 */
    const char* const* header_keys;   /* 额外请求头（可 NULL） */
    const char* const* header_values;
    size_t header_count;
    const char* const* mirror_urls;   /* 镜像地址（同一文件），可 NULL */
    size_t mirror_count;
} dc_request;

typedef void (*dc_progress_cb)(void* userdata, const dc_progress* p);
typedef void (*dc_status_cb)(void* userdata, int status);
typedef void (*dc_log_cb)(void* userdata, int level, const char* message);

/* 版本 / 编译时间戳（静态字符串，无需释放） */
const char* dc_version(void);
const char* dc_build_stamp(void);

/* 默认配置 */
dc_config dc_config_default(void);

/* 创建 / 销毁引擎 */
dc_engine* dc_engine_new(const dc_config* cfg);
void       dc_engine_free(dc_engine* engine);

/* 请求取消（可从另一个线程调用） */
void dc_engine_cancel(dc_engine* engine);

/* 暂停 / 恢复（连接保持，可从另一个线程调用） */
void dc_engine_pause(dc_engine* engine);
void dc_engine_resume(dc_engine* engine);

/*
 * 同步下载。返回 0 成功；非 0 为 dc_error 错误码（*err_msg 为错误信息，需 dc_string_free）。
 * 结果写入 *out（可为 NULL），其中 path 需 dc_result_free 释放。
 * 回调函数指针可为 NULL（表示不关心）。
 */
int dc_engine_download(dc_engine* engine,
                       const dc_request* req,
                       dc_progress_cb on_progress,
                       dc_status_cb   on_status,
                       dc_log_cb      on_log,
                       void*          userdata,
                       dc_result*     out,
                       char**         err_msg);

/* 释放由本库返回的字符串 / 结果内容 */
void dc_string_free(char* s);
void dc_result_free(dc_result* r);

#ifdef __cplusplus
}
#endif

#endif /* DOWNLOADCORE_H */
