//! File names as plutil handles them: URL's lastPathComponent, and its String extension's
//! `stem` and `replaceExtension` (which looks for the last '.' in the whole path, as plutil does).

/// URL(fileURLWithPath:).lastPathComponent.
pub fn last_component(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return if p.is_empty() { String::new() } else { "/".into() };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// The index of the '.' that starts an extension: the last one, unless it is the first
/// character or the path is "." or "..".
fn extension_index(s: &str) -> Option<usize> {
    if s == "." || s == ".." {
        return None;
    }
    match s.rfind('.') {
        Some(0) | None => None,
        Some(i) => Some(i),
    }
}

/// The last component without its extension.
pub fn stem(p: &str) -> String {
    let last = last_component(p);
    match extension_index(&last) {
        Some(i) => last[..i].to_string(),
        None => last,
    }
}

/// `replaceExtension(with:)`: what follows the last '.' becomes `ext`; nothing changes without one.
pub fn replace_extension(p: &str, ext: &str) -> String {
    match extension_index(p) {
        Some(i) => format!("{}{ext}", &p[..=i]),
        None => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plutil_file_names() {
        assert_eq!(last_component("a/b.plist"), "b.plist");
        assert_eq!(last_component("a/b/"), "b");
        assert_eq!(last_component("-"), "-");
        assert_eq!(stem("dir/Info.plist"), "Info");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("a.tar.gz"), "a.tar");
        assert_eq!(replace_extension("x/Info.plist", "swift"), "x/Info.swift");
        assert_eq!(replace_extension("noext", "m"), "noext");
        assert_eq!(replace_extension("dir.d/file", "m"), "dir.m");
    }
}
