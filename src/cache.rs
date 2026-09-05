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

    /// Whether the cache holds anything, without parsing it.
    ///
    /// The flush loop only ever asks "is there work?", so it must not pay for
    /// deserializing the backlog to answer. A missing file counts as empty:
    /// the cache is created lazily on first append.
    pub fn is_empty(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(m) => m.len() == 0,
            Err(_) => true,
        }
    }
}

/// Durable store for buckets whose write to ClickHouse has not succeeded yet.
///
/// Events already survive a restart via `DiskCache`; buckets did not. They were
/// held only in an in-memory `Vec`, so a bridge that was restarted while
/// ClickHouse was unreachable dropped them silently -- and watchers call
/// createBucket() once at startup and never retry, so the bucket row was then
/// missing forever while that bucket's events kept arriving. `aw_buckets` ended
/// up disagreeing with `aw_events` (buckets with thousands of events and no row).
///
/// Unlike the event cache this rewrites the whole file rather than appending:
/// buckets are few, keyed by id, and repeatedly re-queueing the same handful
/// must not grow a log without bound.
pub struct BucketCache {
    path: PathBuf,
}

impl BucketCache {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Replace the stored set. An empty slice removes the file.
    pub fn write_all(&self, buckets: &[aw_models::Bucket]) -> io::Result<()> {
        if buckets.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }

        // Write to a sibling then rename, so a crash mid-write cannot leave a
        // half-serialized file that read_all would silently drop entries from.
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let file = File::create(&tmp)?;
            let mut w = BufWriter::new(file);
            for b in buckets {
                writeln!(w, "{}", serde_json::to_string(b)?)?;
            }
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }

    pub fn read_all(&self) -> io::Result<Vec<aw_models::Bucket>> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<aw_models::Bucket>(&line) {
                Ok(b) => out.push(b),
                Err(e) => tracing::warn!("Skipping malformed pending-bucket line: {}", e),
            }
        }
        Ok(out)
    }
}
