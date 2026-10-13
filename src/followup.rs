//! Render copyable CLI instructions without changing argument boundaries.
pub(crate) fn command(args: impl IntoIterator<Item = String>) -> String {
    std::iter::once("doty".to_owned())
        .chain(args)
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
            {
                arg
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
