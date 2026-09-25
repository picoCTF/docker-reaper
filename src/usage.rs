//! When each image was last seen in use, so the images sweep can evict least recently used
//! first. Docker keeps no such time: the Engine API has no last-used field, and a pull does
//! not set `LastTagTime`.
//!
//! One line per image, `<unix seconds> <image id>`, replaced whole by a rename so a reader
//! never sees a torn file. There is no lock: writers only move stamps forward or drop
//! images that are gone, and a change lost to a concurrent write is made again later.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Image id to the unix time it was last seen in use.
pub(crate) type LastUsed = HashMap<String, u64>;

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads the record. A missing file is an empty record, and a malformed line is skipped.
pub(crate) fn load(path: &Path) -> io::Result<LastUsed> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LastUsed::new()),
        Err(e) => return Err(e),
    };
    let mut record = LastUsed::new();
    for line in text.lines() {
        let Some((secs, id)) = line.split_once(' ') else {
            continue;
        };
        let Ok(secs) = secs.parse() else {
            continue;
        };
        if !id.is_empty() {
            touch(&mut record, id, secs);
        }
    }
    Ok(record)
}

/// Moves an image's stamp forward to `secs`, never back.
pub(crate) fn touch(record: &mut LastUsed, id: &str, secs: u64) {
    let stamp = record.entry(id.to_string()).or_insert(secs);
    *stamp = (*stamp).max(secs);
}

/// Replaces the record on disk.
pub(crate) fn save(path: &Path, record: &LastUsed) -> io::Result<()> {
    let mut entries: Vec<(&String, &u64)> = record.iter().collect();
    entries.sort();
    let mut text = String::new();
    for (id, secs) in entries {
        text.push_str(&format!("{secs} {id}\n"));
    }
    // Per process, so two writers never share a temporary file.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut file = fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("docker-reaper-usage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("image-use")
    }

    #[test]
    fn a_missing_record_is_empty() {
        let path = fixture("missing");
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn round_trips_and_leaves_no_temporary_file() {
        let path = fixture("roundtrip");
        let record = LastUsed::from([
            ("sha256:aaa".to_string(), 100),
            ("sha256:bbb".to_string(), 200),
        ]);
        save(&path, &record).unwrap();
        assert_eq!(load(&path).unwrap(), record);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["image-use"]);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let path = fixture("malformed");
        fs::write(
            &path,
            "100 sha256:aaa\nnot-a-number sha256:bbb\nno-space\n200 \n\n300 sha256:ccc\n",
        )
        .unwrap();
        let record = load(&path).unwrap();
        assert_eq!(
            record,
            LastUsed::from([
                ("sha256:aaa".to_string(), 100),
                ("sha256:ccc".to_string(), 300),
            ])
        );
    }

    #[test]
    fn stamps_only_move_forward() {
        let mut record = LastUsed::new();
        touch(&mut record, "sha256:aaa", 200);
        touch(&mut record, "sha256:aaa", 100);
        assert_eq!(record["sha256:aaa"], 200);
        touch(&mut record, "sha256:aaa", 300);
        assert_eq!(record["sha256:aaa"], 300);
    }

    #[test]
    fn a_duplicated_line_keeps_the_later_stamp() {
        let path = fixture("duplicate");
        fs::write(&path, "300 sha256:aaa\n100 sha256:aaa\n").unwrap();
        assert_eq!(load(&path).unwrap()["sha256:aaa"], 300);
    }
}
