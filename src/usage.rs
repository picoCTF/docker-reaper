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

/// Reads the record. A missing file is an empty record, and a malformed line is skipped,
/// including one that is not UTF-8: a record that cannot be read is never rewritten, so
/// one bad byte refusing the whole file would turn LRU off for good.
pub(crate) fn load(path: &Path) -> io::Result<LastUsed> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LastUsed::new()),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&bytes);
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

/// Replaces the record on disk. A failed save leaves no temporary file behind.
pub(crate) fn save(path: &Path, record: &LastUsed) -> io::Result<()> {
    let mut entries: Vec<(&String, &u64)> = record.iter().collect();
    entries.sort();
    let mut text = String::new();
    for (id, secs) in entries {
        text.push_str(&format!("{secs} {id}\n"));
    }
    let tmp = tmp_path(path);
    let written = fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Whether a save could write its temporary file, checked by writing and syncing a few bytes
/// and removing it; creating an empty file still succeeds on a full filesystem. Starting a
/// record costs an image list, so a record that could never be saved must not start one on
/// every run.
pub(crate) fn writable(path: &Path) -> io::Result<()> {
    let tmp = tmp_path(path);
    let probe = fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(b"0 probe\n")?;
        file.sync_all()
    });
    let removed = fs::remove_file(&tmp);
    probe.and(removed)
}

/// Per process, so two writers never share a temporary file.
fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(tmp)
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
    fn a_failed_save_leaves_no_temporary_file() {
        let path = fixture("failed-save");
        // A non-empty directory where the record should be: the rename onto it fails.
        fs::create_dir_all(path.join("occupied")).unwrap();
        assert!(save(&path, &LastUsed::from([("sha256:aaa".to_string(), 1)])).is_err());
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["image-use"]);
    }

    #[test]
    fn writable_tells_a_usable_directory_from_a_missing_one() {
        let path = fixture("writable");
        writable(&path).unwrap();
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
        assert!(writable(&path.parent().unwrap().join("missing").join("image-use")).is_err());
    }

    #[test]
    fn a_line_that_is_not_utf8_costs_only_itself() {
        let path = fixture("not-utf8");
        fs::write(
            &path,
            b"100 sha256:aaa\n\xff\xfe 5 junk\n200 sha256:b\xffb\n300 sha256:ccc\n",
        )
        .unwrap();
        let record = load(&path).unwrap();
        assert_eq!(record.get("sha256:aaa"), Some(&100));
        assert_eq!(record.get("sha256:ccc"), Some(&300));
        assert_eq!(
            record.len(),
            3,
            "the mangled id is kept as an id that matches no image"
        );
    }

    #[test]
    fn a_duplicated_line_keeps_the_later_stamp() {
        let path = fixture("duplicate");
        fs::write(&path, "300 sha256:aaa\n100 sha256:aaa\n").unwrap();
        assert_eq!(load(&path).unwrap()["sha256:aaa"], 300);
    }
}
