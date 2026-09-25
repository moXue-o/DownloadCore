use crate::errors::{fatal, Result};
use crate::part::Part;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 把所有分段临时文件按 from 顺序拼成成品。
///
/// 只拷贝每段"实际下到的字节数"（current - from），
/// 这样即使之前预分配过更大的文件、后来又被分裂缩小，也不会把多出来的 0 拷进去。
pub fn assemble(
    temp_dir: &Path,
    parts: &[(i64, i64)], // (from, length)
    marker: &Path,
    final_path: &Path,
) -> Result<()> {
    // 单段且正好覆盖整个文件：直接改名，省一次整文件拷贝
    if parts.len() == 1 && parts[0].0 == 0 {
        return move_into_place(&part_file(temp_dir, parts[0].0), final_path);
    }

    let mut out = fs::File::create(marker)
        .map_err(|e| fatal("assemble", format!("创建临时成品失败: {e}")))?;
    let mut buf = vec![0u8; 1 << 20];
    for (from, length) in parts {
        if *length <= 0 {
            continue;
        }
        let src_path = part_file(temp_dir, *from);
        let mut src = fs::File::open(&src_path)
            .map_err(|e| fatal("assemble", format!("打开分段 {src_path:?} 失败: {e}")))?;
        let mut remaining = *length;
        while remaining > 0 {
            let want = remaining.min(buf.len() as i64) as usize;
            let n = src
                .read(&mut buf[..want])
                .map_err(|e| fatal("assemble", format!("读取分段失败: {e}")))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|e| fatal("assemble", format!("写入成品失败: {e}")))?;
            remaining -= n as i64;
        }
    }
    out.sync_all()
        .map_err(|e| fatal("assemble", format!("刷新成品失败: {e}")))?;
    drop(out);
    move_into_place(marker, final_path)
}

fn part_file(dir: &Path, from: i64) -> PathBuf {
    crate::util::part_file_name(dir, from)
}

/// 把半成品改名成最终文件：下完才改名，用户不会把半成品当成成品。
fn move_into_place(src: &Path, final_path: &Path) -> Result<()> {
    let _ = fs::remove_file(final_path);
    fs::rename(src, final_path).map_err(|e| fatal("rename", format!("改名失败: {e}")))
}

/// 从分段列表算出 (from, length)，供拼装使用。
pub fn parts_for_assemble(parts: &[Part]) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> = parts
        .iter()
        .map(|p| (p.from, (p.current - p.from).max(0)))
        .collect();
    v.sort_by_key(|x| x.0);
    v
}
