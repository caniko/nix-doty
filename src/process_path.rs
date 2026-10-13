//! Linux /proc/maps escapes only newline bytes, leaving literal backslashes
//! and spaces unchanged. Compare in that spelling rather than decoding it:
//! a literal backslash-012 and a newline are intentionally indistinguishable.
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

pub(crate) fn maps_spelling(path: &Path) -> PathBuf {
    let mut spelling = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte == b'\n' {
            spelling.extend_from_slice(b"\\012");
        } else {
            spelling.push(byte);
        }
    }
    PathBuf::from(OsString::from_vec(spelling))
}

/// The kernel's deleted-object annotation is ambiguous with a literal filename
/// suffix. Preserve both interpretations instead of discarding the raw name.
pub(crate) fn reference_spellings(path: &str) -> impl Iterator<Item = &Path> {
    std::iter::once(Path::new(path)).chain(path.strip_suffix(" (deleted)").map(Path::new))
}

/// Structured argument values have no universal pathname delimiter grammar.
/// Treat a pathname occurrence in each NUL-separated field conservatively,
/// including percent-encoded URI values, rather than guessing suffix syntax.
pub(crate) fn argv_references(command: &[u8], path: &Path) -> bool {
    let needle = path.as_os_str().as_bytes();
    if needle.is_empty() {
        return true;
    }
    let contains = |field: &[u8]| field.windows(needle.len()).any(|bytes| bytes == needle);
    command.split(|byte| *byte == 0).any(|field| {
        if contains(field) {
            return true;
        }
        if !field.contains(&b'%') {
            return false;
        }
        let hex = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
        let mut decoded = Vec::with_capacity(field.len());
        let mut index = 0;
        while index < field.len() {
            if field[index] == b'%'
                && let Some(pair) = field.get(index + 1..index + 3)
                && let (Some(high), Some(low)) = (hex(pair[0]), hex(pair[1]))
            {
                decoded.push(high * 16 + low);
                index += 3;
            } else {
                decoded.push(field[index]);
                index += 1;
            }
        }
        contains(&decoded)
    })
}
