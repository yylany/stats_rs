use std::fs::{self, DirEntry};
use std::io;
use std::path::Path;
use std::time::Duration;

pub fn clean_old_files(folder_path: &str, max_ts: Duration) -> anyhow::Result<()> {
    let folder = Path::new(folder_path);
    if !folder.is_dir() {
        return Err(anyhow::anyhow!("Provided path is not a directory"));
    }

    let now = std::time::SystemTime::now();

    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        if let Ok(metadata) = entry.metadata() {
            if let Ok(created_time) = metadata.created() {
                if now.duration_since(created_time)?.gt(&max_ts) {
                    delete_file(&entry)?;
                }
            }
        }
    }

    Ok(())
}

fn delete_file(entry: &DirEntry) -> io::Result<()> {
    let path = entry.path();
    if path.is_file() {
        println!("Deleting timeout file: {:?}", path);
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    #[test]
    fn test_clean_old_files() {
        clean_old_files(
            "/Users/yaoyonglong/Desktop/doc/work/vida/rust/terminals/general_spider/data/stats",
            Duration::from_secs(30),
        )
        .unwrap();
    }
}
