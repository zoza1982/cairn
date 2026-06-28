//! Identifiers and the backend [`Scheme`] enumeration.

use smol_str::SmolStr;

/// An opaque handle to a configured (and possibly connected) backend instance.
///
/// The UI never constructs one directly; the connection registry hands them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn:{}", self.0)
    }
}

/// The backend family a connection addresses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scheme {
    /// The local filesystem.
    Local,
    /// SSH/SFTP.
    Ssh,
    /// Amazon S3 (and S3-compatible endpoints).
    S3,
    /// Google Cloud Storage.
    Gcs,
    /// Azure Blob Storage.
    Azure,
    /// Docker / OCI containers and images.
    Docker,
    /// Kubernetes.
    Kubernetes,
    /// A scheme provided by a third-party plugin, identified by name.
    Plugin(SmolStr),
}

impl Scheme {
    /// The canonical lowercase URI scheme string (e.g. `"s3"`, `"k8s"`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
            Self::Docker => "docker",
            Self::Kubernetes => "k8s",
            Self::Plugin(name) => name.as_str(),
        }
    }
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_strings() {
        assert_eq!(Scheme::S3.as_str(), "s3");
        assert_eq!(Scheme::Kubernetes.as_str(), "k8s");
        assert_eq!(Scheme::Plugin(SmolStr::new("ftp")).as_str(), "ftp");
    }

    #[test]
    fn connection_id_display() {
        assert_eq!(ConnectionId(7).to_string(), "conn:7");
    }
}
