use directories::ProjectDirs;
use std::fs;
use uuid::Uuid;

/// Get or create device ID (compatible with aw-server-rust location)
pub fn get_or_create() -> String {
    let dirs = ProjectDirs::from("net", "activitywatch", "activitywatch")
        .expect("Failed to get project dirs");

    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir).ok();

    let path = data_dir.join("device_id");

    if path.exists() {
        fs::read_to_string(&path).unwrap().trim().to_string()
    } else {
        let id = Uuid::new_v4().hyphenated().to_string();
        fs::write(&path, &id).unwrap();
        id
    }
}
