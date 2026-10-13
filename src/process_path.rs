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
