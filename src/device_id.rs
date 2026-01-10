use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

/// Get data directory, with fallback for Android/restricted environments
fn get_data_dir() -> PathBuf {
    // Check env override first (useful for Android)
    if let Ok(dir) = std::env::var("AW_DATA_DIR") {
        return PathBuf::from(dir);
    }

    // Try standard project dirs
    if let Some(dirs) = ProjectDirs::from("net", "activitywatch", "activitywatch") {
        let data_dir = dirs.data_dir().to_path_buf();
        if fs::create_dir_all(&data_dir).is_ok() {
            return data_dir;
        }
    }

    // Fallback to current directory
    PathBuf::from(".")
}

/// Get or create device ID (compatible with aw-server-rust location)
pub fn get_or_create() -> String {
    // Allow override via env var (for Android/no-filesystem mode)
    if let Ok(id) = std::env::var("AW_DEVICE_ID") {
        return id;
    }

    let data_dir = get_data_dir();
    let path = data_dir.join("device_id");

    if path.exists() {
        if let Ok(id) = fs::read_to_string(&path) {
            return id.trim().to_string();
        }
    }

    let id = Uuid::new_v4().hyphenated().to_string();
    let _ = fs::write(&path, &id); // Ignore write errors
    id
}
