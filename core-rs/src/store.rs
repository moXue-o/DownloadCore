use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartState {
    pub from: i64,
    pub to: i64,
    pub current: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub version: u32,
    pub url: String,
    pub total: i64,
    pub etag: String,
    pub last_modified: String,
    pub parts: Vec<PartState>,
}

pub const STATE_FILE_NAME: &str = "state.json";

/// 原子地写续传记录：先写临时文件，再改名。
/// 这样即使在写入瞬间断电，也不会留下半截坏记录。
pub fn save_state_file(path: &Path, st: &ResumeState) -> std::io::Result<()> {
    let data = serde_json::to_vec(st).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// 读续传记录；不存在或损坏都返回 None。
pub fn load_state_file(path: &Path) -> Option<ResumeState> {
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}
