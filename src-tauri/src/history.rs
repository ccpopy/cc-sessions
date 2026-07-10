use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use serde_json::Value;

use crate::atomic_file;
use crate::error::{AppError, AppResult};

const SESSION_ID_KEYS: [&str; 3] = ["sessionId", "session_id", "id"];

fn regular_file_exists(path: &Path, label: &str) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::path_safety::metadata_is_link_or_reparse(&metadata) =>
        {
            Ok(true)
        }
        Ok(_) => Err(AppError::Path(format!(
            "{label} 不是普通文件: {}",
            path.to_string_lossy()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn line_session_id(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(line).ok()?;
    for key in SESSION_ID_KEYS {
        if let Some(id) = value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

pub fn line_matches_session(line: &str, id: &str) -> bool {
    line_session_id(line).as_deref() == Some(id)
}

pub fn collect_lines_for_ids(
    history_path: &Path,
    ids: &HashSet<String>,
) -> AppResult<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if ids.is_empty() || !regular_file_exists(history_path, "history")? {
        return Ok(out);
    }

    let file = File::open(history_path)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some(id) = line_session_id(&line) {
            if ids.contains(&id) {
                out.entry(id).or_default().push(line);
            }
        }
    }
    Ok(out)
}

pub fn write_lines(path: &Path, lines: &[String]) -> AppResult<u32> {
    if lines.is_empty() {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existed = regular_file_exists(path, "history")?;
    let expected = existed
        .then(|| atomic_file::fingerprint(path))
        .transpose()?;
    let writer = |file: &mut File| -> AppResult<()> {
        for line in lines {
            writeln!(file, "{}", line)?;
        }
        Ok(())
    };
    if let Some(expected) = expected.as_ref() {
        atomic_file::replace_with_writer_if_unchanged(path, expected, writer)?;
    } else {
        atomic_file::create_with_writer_if_absent(path, writer)?;
    }
    Ok(lines.len() as u32)
}

pub fn append_lines(history_path: &Path, id: &str, lines: &[String]) -> AppResult<u32> {
    if lines.is_empty() {
        return Ok(0);
    }
    if let Some(parent) = history_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existed = regular_file_exists(history_path, "history")?;
    let expected = existed
        .then(|| atomic_file::fingerprint(history_path))
        .transpose()?;
    let mut existing: HashSet<String> = HashSet::new();
    let mut output = Vec::new();
    if existed {
        for line in BufReader::new(File::open(history_path)?).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                existing.insert(line.clone());
            }
            output.push(line);
        }
    }

    let mut added = 0u32;
    for line in lines {
        if !line_matches_session(line, id) {
            continue;
        }
        if existing.insert(line.clone()) {
            output.push(line.clone());
            added += 1;
        }
    }
    if added == 0 {
        return Ok(0);
    }
    let writer = |file: &mut File| -> AppResult<()> {
        for line in &output {
            writeln!(file, "{line}")?;
        }
        Ok(())
    };
    if let Some(expected) = expected.as_ref() {
        atomic_file::replace_with_writer_if_unchanged(history_path, expected, writer)?;
    } else {
        atomic_file::create_with_writer_if_absent(history_path, writer)?;
    }
    Ok(added)
}

pub fn append_from_file(history_path: &Path, source_path: &Path, id: &str) -> AppResult<u32> {
    if !regular_file_exists(source_path, "history source")? {
        return Ok(0);
    }
    let lines = BufReader::new(File::open(source_path)?)
        .lines()
        .collect::<Result<Vec<_>, _>>()?;
    append_lines(history_path, id, &lines)
}

pub fn filter_file(path: &Path, id: &str) -> AppResult<u32> {
    let ids = HashSet::from([id.to_string()]);
    let mut removed = filter_file_for_ids(path, &ids)?;
    Ok(removed.remove(id).unwrap_or(0))
}

pub fn filter_file_for_ids(path: &Path, ids: &HashSet<String>) -> AppResult<HashMap<String, u32>> {
    let mut removed = HashMap::new();
    if ids.is_empty() || !regular_file_exists(path, "history")? {
        return Ok(removed);
    }

    let expected = atomic_file::fingerprint(path)?;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let matched_id = line_session_id(&line).filter(|id| ids.contains(id));
        if let Some(id) = matched_id {
            *removed.entry(id).or_insert(0) += 1;
        }
    }

    if removed.is_empty() {
        return Ok(removed);
    }

    atomic_file::replace_with_writer_if_unchanged(path, &expected, |temp| {
        let source = File::open(path)?;
        let mut writer = BufWriter::new(temp);
        for line in BufReader::new(source).lines() {
            let line = line?;
            let matched = line_session_id(&line).is_some_and(|id| ids.contains(&id));
            if !matched {
                writeln!(writer, "{line}")?;
            }
        }
        writer.flush()?;
        Ok(())
    })?;
    Ok(removed)
}
