use aw_models::Event;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Serialize, Deserialize)]
pub struct CachedEvent {
    #[serde(default)]
    pub device_id: String,
    pub bucket_id: String,
    pub bucket_type: String,
    pub hostname: String,
    pub event: Event,
    #[serde(default)]
    pub version: u64,
}

pub struct DiskCache {
    path: PathBuf,
    file: Mutex<BufWriter<File>>,
}

impl DiskCache {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;

        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn append(&self, events: &[(String, String, String, String, Event, u64)]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut file = self.file.lock().unwrap();
        for (device_id, bucket_id, bucket_type, hostname, event, version) in events {
            let cached = CachedEvent {
                device_id: device_id.clone(),
                bucket_id: bucket_id.clone(),
                bucket_type: bucket_type.clone(),
                hostname: hostname.clone(),
                event: event.clone(),
                version: *version,
            };
            let line = serde_json::to_string(&cached)?;
            writeln!(file, "{}", line)?;
        }
        file.flush()?;
        // fsync for durability
        file.get_ref().sync_all()?;
        Ok(())
    }

    pub fn read_all(&self) -> io::Result<Vec<(String, String, String, String, Event, u64)>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<CachedEvent>(&line) {
                Ok(cached) => {
                    events.push((
                        cached.device_id,
                        cached.bucket_id,
                        cached.bucket_type,
                        cached.hostname,
                        cached.event,
                        cached.version,
                    ));
                }
                Err(e) => {
                    tracing::warn!("Skipping malformed cache line: {}", e);
                }
            }
        }

        Ok(events)
    }

    pub fn clear(&self) -> io::Result<()> {
        let mut file = self.file.lock().unwrap();
        // Truncate by seeking to start and setting length to 0
        file.get_mut().set_len(0)?;
        file.get_mut().seek(SeekFrom::Start(0))?;
        file.get_ref().sync_all()?;
        Ok(())
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        let metadata = std::fs::metadata(&self.path)?;
        Ok(metadata.len() == 0)
    }
}
