use std::time::{SystemTime, UNIX_EPOCH};

/// 编译时把 build 时间戳注入二进制，供程序显示、便于校对版本。
/// - 若外部设置了环境变量 `BUILD_STAMP`（见 build.ps1），就用它；
/// - 否则（例如直接 `cargo build`）自动取当前 UTC 时间。
fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_STAMP");
    let stamp = std::env::var("BUILD_STAMP").unwrap_or_else(|_| now_stamp());
    println!("cargo:rustc-env=BUILD_STAMP={stamp}");
    // 让每次编译都重新执行本脚本，从而拿到新的时间戳
    println!("cargo:rerun-if-changed=build.rs");
}

fn now_stamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, dd) = civil_from_days(days);
    format!("{y:04}{m:02}{dd:02}-{h:02}{mi:02}{s:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
