//! [`VfsPath`]: a normalized, backend-agnostic, traversal-safe location within one backend.

use smol_str::SmolStr;

/// Errors produced when parsing or manipulating a [`VfsPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PathError {
    /// The path contained a `..` segment. Parent traversal is rejected at parse time so a path can
    /// never escape its backend root (important across the plugin/AI trust boundary).
    #[error("path traversal ('..') is not allowed")]
    Traversal,
    /// The path contained a control character (including NUL) or other disallowed byte.
    #[error("path contains a control or disallowed character")]
    InvalidChar,
}

/// A location within a single backend.
///
/// Paths are **normalized but not resolved**: they are stored as `/`-separated segments regardless
/// of the host platform, and `..` is rejected at construction time. The `trailing_slash` flag is
/// preserved because on object stores `foo` (an object) and `foo/` (a prefix) are distinct entities.
///
/// An empty segment list is the backend root (`/`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct VfsPath {
    segments: Vec<SmolStr>,
    trailing_slash: bool,
}

impl VfsPath {
    /// The backend root path (`/`).
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// Parse a `/`-separated string into a `VfsPath`.
    ///
    /// Leading/trailing slashes are interpreted; empty and `.` segments are dropped. A `..` segment
    /// or any control character is an error.
    ///
    /// # Errors
    /// Returns [`PathError::Traversal`] if any segment is `..`, or [`PathError::InvalidChar`] if any
    /// character is a control character.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.chars().any(char::is_control) {
            return Err(PathError::InvalidChar);
        }
        let trailing_slash = s.ends_with('/') && !s.is_empty();
        let mut segments = Vec::new();
        for raw in s.split('/') {
            match raw {
                "" | "." => {}
                ".." => return Err(PathError::Traversal),
                seg => segments.push(SmolStr::new(seg)),
            }
        }
        Ok(Self {
            segments,
            trailing_slash,
        })
    }

    /// Returns `true` if this is the backend root (no segments).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// The path's segments, leaf-last. Empty for the root.
    #[must_use]
    pub fn segments(&self) -> &[SmolStr] {
        &self.segments
    }

    /// Whether the path carries a significant trailing slash (object-store prefix marker).
    #[must_use]
    pub fn has_trailing_slash(&self) -> bool {
        self.trailing_slash
    }

    /// The leaf (final segment) name, or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.segments.last().map(SmolStr::as_str)
    }

    /// The parent path, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.segments.is_empty() {
            return None;
        }
        let mut segments = self.segments.clone();
        segments.pop();
        Some(Self {
            segments,
            trailing_slash: false,
        })
    }

    /// Append a single child segment, returning the new path.
    ///
    /// # Errors
    /// Returns [`PathError::Traversal`] if `name` is `..`, or [`PathError::InvalidChar`] if it
    /// contains a `/` or control character.
    pub fn join(&self, name: &str) -> Result<Self, PathError> {
        if name == ".." {
            return Err(PathError::Traversal);
        }
        if name.contains('/') || name.chars().any(char::is_control) {
            return Err(PathError::InvalidChar);
        }
        let mut segments = self.segments.clone();
        if !matches!(name, "" | ".") {
            segments.push(SmolStr::new(name));
        }
        Ok(Self {
            segments,
            trailing_slash: false,
        })
    }

    /// Render the canonical `/`-prefixed string form, preserving a trailing slash if present.
    #[must_use]
    pub fn as_str(&self) -> String {
        if self.segments.is_empty() {
            return "/".to_owned();
        }
        let mut out = String::with_capacity(self.segments.iter().map(|s| s.len() + 1).sum());
        for seg in &self.segments {
            out.push('/');
            out.push_str(seg);
        }
        if self.trailing_slash {
            out.push('/');
        }
        out
    }
}

impl std::fmt::Display for VfsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_empty() {
        let r = VfsPath::root();
        assert!(r.is_root());
        assert_eq!(r.as_str(), "/");
        assert_eq!(r.file_name(), None);
        assert_eq!(r.parent(), None);
    }

    #[test]
    fn parse_normalizes_segments() {
        let p = VfsPath::parse("/a//b/./c").unwrap();
        assert_eq!(p.segments().len(), 3);
        assert_eq!(p.as_str(), "/a/b/c");
        assert_eq!(p.file_name(), Some("c"));
    }

    #[test]
    fn parse_rejects_traversal() {
        assert_eq!(VfsPath::parse("/a/../b"), Err(PathError::Traversal));
        assert_eq!(VfsPath::parse(".."), Err(PathError::Traversal));
    }

    #[test]
    fn parse_rejects_control_chars() {
        assert_eq!(VfsPath::parse("/a\0b"), Err(PathError::InvalidChar));
        assert_eq!(VfsPath::parse("/a\nb"), Err(PathError::InvalidChar));
    }

    #[test]
    fn trailing_slash_is_preserved() {
        let prefix = VfsPath::parse("/logs/2026/").unwrap();
        assert!(prefix.has_trailing_slash());
        assert_eq!(prefix.as_str(), "/logs/2026/");
        let object = VfsPath::parse("/logs/2026").unwrap();
        assert!(!object.has_trailing_slash());
        // Object vs prefix are distinct.
        assert_ne!(prefix, object);
    }

    #[test]
    fn join_and_parent_round_trip() {
        let base = VfsPath::parse("/a/b").unwrap();
        let child = base.join("c").unwrap();
        assert_eq!(child.as_str(), "/a/b/c");
        assert_eq!(child.parent(), Some(base));
    }

    #[test]
    fn join_rejects_traversal_and_separators() {
        let base = VfsPath::root();
        assert_eq!(base.join(".."), Err(PathError::Traversal));
        assert_eq!(base.join("a/b"), Err(PathError::InvalidChar));
    }
}
