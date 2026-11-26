//! Effective access carried by mounted Drive nodes.

use proton_drive_rs::MemberRole;

/// Effective write access for a node in a mounted tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Owner,
    Editor,
    Viewer,
    Unknown,
}

impl Access {
    pub fn writable(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Resolve a share root's effective access from its exact membership role.
///
/// Owned trees fail open. A share whose role is absent or unrecognised fails
/// closed. Descendants normally inherit directly at intern time; `Inherited`
/// exists for the explicit wire role and therefore returns `parent`.
pub fn access_for(role: Option<MemberRole>, parent: Access, under_share: bool) -> Access {
    match role {
        Some(MemberRole::Viewer) => Access::Viewer,
        Some(MemberRole::Editor | MemberRole::Admin) => Access::Editor,
        Some(MemberRole::Inherited) => parent,
        None if under_share => Access::Viewer,
        None => Access::Owner,
    }
}

/// POSIX permission bits advertised for a mounted node.
pub fn perm_bits(is_dir: bool, access: Access) -> u16 {
    match (is_dir, access.writable()) {
        (true, true) => 0o755,
        (false, true) => 0o644,
        (true, false) => 0o555,
        (false, false) => 0o444,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_for_covers_the_fail_open_and_fail_closed_table() {
        for parent in [
            Access::Owner,
            Access::Editor,
            Access::Viewer,
            Access::Unknown,
        ] {
            assert_eq!(access_for(None, parent, false), Access::Owner);
            assert_eq!(access_for(None, parent, true), Access::Viewer);
            assert_eq!(
                access_for(Some(MemberRole::Viewer), parent, true),
                Access::Viewer
            );
            assert_eq!(
                access_for(Some(MemberRole::Editor), parent, true),
                Access::Editor
            );
            assert_eq!(
                access_for(Some(MemberRole::Admin), parent, true),
                Access::Editor
            );
            assert_eq!(
                access_for(Some(MemberRole::Inherited), parent, true),
                parent
            );
        }
    }

    #[test]
    fn permission_bits_follow_effective_access() {
        for writable in [Access::Owner, Access::Editor] {
            assert_eq!(perm_bits(true, writable), 0o755);
            assert_eq!(perm_bits(false, writable), 0o644);
        }
        for read_only in [Access::Viewer, Access::Unknown] {
            assert_eq!(perm_bits(true, read_only), 0o555);
            assert_eq!(perm_bits(false, read_only), 0o444);
        }
    }
}
