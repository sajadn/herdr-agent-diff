use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::revision_source;
use crate::model::{CurrentFile, INLINE_TEXT_LIMIT, TextEligibility};
use crate::snapshot::safe_read;

const MAX_MATCHES: usize = 1_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: PathBuf,
    /// Zero-based source line.
    pub line: usize,
    pub preview: String,
}

#[derive(Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub skipped: usize,
    pub limited: bool,
}

/// Search exactly the tree listed in Files, with the same safe-read and preview
/// limits. Cancellation is checked between files and lines, including huge lines.
#[must_use]
pub fn search_files(
    root: &Path,
    files: &[CurrentFile],
    revision: Option<&str>,
    query: &str,
    newest: &AtomicU64,
    request: u64,
) -> Option<SearchResults> {
    let mut result = SearchResults::default();
    if query.is_empty() {
        return Some(result);
    }
    let mut bytes = 0;
    for file in files {
        if newest.load(Ordering::Acquire) != request {
            return None;
        }
        if file.text != TextEligibility::Text || file.size > INLINE_TEXT_LIMIT {
            result.skipped += 1;
            continue;
        }
        if bytes + file.size > MAX_BYTES {
            result.limited = true;
            break;
        }
        let text = revision.map_or_else(
            || {
                safe_read(root, &file.relative, INLINE_TEXT_LIMIT)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()))
            },
            |revision| revision_source(root, revision, file),
        );
        let Ok(text) = text else {
            result.skipped += 1;
            continue;
        };
        if text.contains('\0') {
            result.skipped += 1;
            continue;
        }
        bytes += text.len() as u64;
        if bytes > MAX_BYTES {
            result.limited = true;
            break;
        }
        for (line, content) in text.lines().enumerate() {
            if newest.load(Ordering::Acquire) != request {
                return None;
            }
            if let Some(offset) = content.find(query) {
                if result.hits.len() == MAX_MATCHES {
                    result.limited = true;
                    return Some(result);
                }
                // Keep the match visible even on a long line, and bound memory.
                let start = content[..offset]
                    .char_indices()
                    .rev()
                    .nth(40)
                    .map_or(0, |(i, _)| i);
                let preview = content[start..]
                    .chars()
                    .take(200)
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                result.hits.push(SearchHit {
                    path: file.relative.clone(),
                    line,
                    preview,
                });
            }
        }
    }
    Some(result)
}
