//! Read Codex's plain/compressed rollout representations without changing storage.
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The stable `.jsonl` path for either representation of a rollout.
pub fn logical_path(path: &Path) -> PathBuf {
    if is_compressed(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

/// Resolve a regular physical file, preferring the plain sibling when both exist.
pub fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let plain = logical_path(path);
    let mut compressed = plain.as_os_str().to_os_string();
    compressed.push(".zst");
    for candidate in [plain, PathBuf::from(compressed)] {
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return Ok(candidate),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "rollout file and compressed sibling are unavailable",
    ))
}

/// Stream decoded JSONL. Retry only missing-file races during Codex representation changes.
pub fn open_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    open_reader_with(path, |resolved| File::open(resolved), std::thread::sleep)
}

fn open_reader_with(
    path: &Path,
    mut open_file: impl FnMut(&Path) -> io::Result<File>,
    mut wait: impl FnMut(Duration),
) -> io::Result<Box<dyn BufRead>> {
    for attempt in 0..4 {
        // A missing logical rollout is not a publication race. Only retry if
        // the physical file disappeared after it was successfully resolved.
        let resolved = resolve_path(path)?;
        let file = match open_file(&resolved) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound && attempt < 3 => {
                wait(Duration::from_millis(50));
                continue;
            }
            Err(error) => return Err(error),
        };
        if is_compressed(&resolved) {
            return Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
                file,
            )?)));
        }
        return Ok(Box::new(BufReader::new(file)));
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::make_temp_dir;

    #[test]
    fn missing_representations_return_without_waiting() {
        let directory = make_temp_dir("cockpit-rollout-missing");
        let missing = directory.join("rollout.jsonl");
        let mut waits = Vec::new();
        let result = open_reader_with(
            &missing,
            |_| panic!("must not open an unresolved file"),
            |delay| waits.push(delay),
        );
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::NotFound);
        assert!(waits.is_empty(), "missing files must not be retried");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retries_a_representation_change_between_resolution_and_open() {
        let directory = make_temp_dir("cockpit-rollout-race");
        let plain = directory.join("rollout.jsonl");
        let compressed = plain.with_extension("jsonl.zst");
        let content = b"{\"type\":\"session_meta\"}\n";
        fs::write(&plain, content).unwrap();
        let mut opens = 0;
        let mut waits = Vec::new();
        let mut reader = open_reader_with(
            &plain,
            |resolved| {
                opens += 1;
                if opens == 1 {
                    assert_eq!(resolved, plain);
                    fs::write(
                        &compressed,
                        zstd::stream::encode_all(&content[..], 0).unwrap(),
                    )
                    .unwrap();
                    fs::remove_file(&plain).unwrap();
                }
                File::open(resolved)
            },
            |delay| waits.push(delay),
        )
        .unwrap();
        let mut decoded = String::new();
        reader.read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded.as_bytes(), content);
        assert_eq!(opens, 2);
        assert_eq!(waits, [Duration::from_millis(50)]);
        assert!(!plain.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
