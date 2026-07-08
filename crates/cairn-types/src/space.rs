//! Free/total space for the volume backing a path (statvfs-style).

/// Filesystem space totals for the volume backing a directory, in bytes.
///
/// Reported by `Vfs::space` (in `cairn-vfs`) for backends that advertise [`Caps::SPACE`](crate::Caps::SPACE)
/// — local and SSH/SFTP. Advisory only: the authoritative out-of-space signal is still a backend's
/// error on the actual write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Total capacity of the volume.
    pub total: u64,
    /// Space available to the current user — respects root-reserved blocks (POSIX `f_bavail`) and
    /// per-user quotas. This is what should be shown as "free": raw free space would overstate what
    /// the user can actually write (e.g. an ext4 volume with 5% reserved for root).
    pub available: u64,
}
