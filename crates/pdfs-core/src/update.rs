//! The opt-in check for a newer release ([`crate::config::AppConfig::update_check`]).
//!
//! Only the decisions live here: what GitHub's answer says, which version is
//! newer, when to ask again, and whether a package manager already owns
//! updates. The desktop app does the request itself.

use std::path::Path;
use std::time::Duration;

/// GitHub's latest published release. Drafts and pre-releases never show up
/// here, so a tagged release candidate does not reach anyone.
pub const RELEASES_LATEST_API: &str =
    "https://api.github.com/repos/narrrl/proton-drive-linux/releases/latest";

/// How often the app asks, across restarts.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Installed next to the binaries' `share/` by packages whose package manager
/// brings new releases (COPR, the AUR, Nix). The release `.deb`, `.rpm` and
/// `.tar.gz` do not ship it, since nothing updates them.
pub const MANAGED_MARKER: &str = "share/proton-drive-linux/managed-updates";

/// The version of the release in a `/releases/latest` answer, without the
/// tag's `v`. `None` for an answer that does not parse or a tag that is not
/// `X.Y.Z`, so a changed API can never claim an update.
pub fn latest_version(body: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse(version).map(|_| version.to_owned())
}

/// Whether `version` is a later release than `current`. Anything that is not
/// `X.Y.Z` on either side is not.
pub fn is_newer(version: &str, current: &str) -> bool {
    matches!((parse(version), parse(current)), (Some(v), Some(c)) if v > c)
}

/// The release page of `version` on GitHub.
pub fn release_url(version: &str) -> String {
    format!("https://github.com/narrrl/proton-drive-linux/releases/tag/v{version}")
}

/// `X.Y.Z` as numbers, so `3.10.0` sorts after `3.9.0`.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let triple = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(triple)
}

/// How long until the next check is due, given the last one in Unix seconds.
/// Zero when it is due now. A clock set back past the last check counts as
/// due, so a wrong clock cannot stop checks.
pub fn next_check_in(last_checked: Option<i64>, now: i64) -> Duration {
    let interval = CHECK_INTERVAL.as_secs() as i64;
    match last_checked {
        Some(last) if (0..interval).contains(&(now - last)) => {
            Duration::from_secs((interval - (now - last)) as u64)
        }
        _ => Duration::ZERO,
    }
}

/// Whether the binary at `exe` was installed by a package manager that updates
/// it, which [`MANAGED_MARKER`] next to its `bin/` says.
pub fn updates_managed(exe: &Path) -> bool {
    exe.parent()
        .and_then(Path::parent)
        .is_some_and(|prefix| prefix.join(MANAGED_MARKER).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest(tag: &str) -> Vec<u8> {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/narrrl/proton-drive-linux/releases/tag/{tag}","draft":false}}"#
        )
        .into_bytes()
    }

    #[test]
    fn the_latest_release_is_read_without_its_v() {
        assert_eq!(latest_version(&latest("v3.6.0")).as_deref(), Some("3.6.0"));
        assert_eq!(latest_version(&latest("3.6.0")).as_deref(), Some("3.6.0"));
    }

    #[test]
    fn an_answer_that_does_not_parse_has_no_version() {
        assert_eq!(latest_version(b"not json"), None);
        assert_eq!(latest_version(b"{}"), None);
        assert_eq!(latest_version(br#"{"tag_name":7}"#), None);
        assert_eq!(latest_version(&latest("nightly")), None);
        assert_eq!(latest_version(&latest("v4.0.0-rc1")), None);
        assert_eq!(latest_version(&latest("v4.0")), None);
        assert_eq!(latest_version(&latest("v4.0.0.1")), None);
    }

    #[test]
    fn only_a_later_release_is_newer() {
        assert!(is_newer("3.6.0", "3.5.0"));
        assert!(!is_newer("3.5.0", "3.5.0"));
        assert!(!is_newer("3.4.1", "3.5.0"));
        assert!(!is_newer("2.99.99", "3.0.0"));
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("3.10.0", "3.9.0"));
        assert!(is_newer("3.5.10", "3.5.9"));
        assert!(is_newer("4.0.0", "3.99.99"));
    }

    #[test]
    fn a_version_that_is_not_x_y_z_is_never_newer() {
        assert!(!is_newer("4.0.0", "3.5.0-dev"));
        assert!(!is_newer("garbage", "3.5.0"));
    }

    #[test]
    fn the_release_page_uses_the_tag() {
        assert_eq!(
            release_url("3.6.0"),
            "https://github.com/narrrl/proton-drive-linux/releases/tag/v3.6.0"
        );
    }

    #[test]
    fn a_check_is_due_once_a_day() {
        let day = CHECK_INTERVAL.as_secs() as i64;
        assert_eq!(next_check_in(None, 1_000), Duration::ZERO);
        assert_eq!(next_check_in(Some(1_000), 1_000), CHECK_INTERVAL);
        assert_eq!(
            next_check_in(Some(1_000), 1_000 + day - 1),
            Duration::from_secs(1)
        );
        assert_eq!(next_check_in(Some(1_000), 1_000 + day), Duration::ZERO);
        assert_eq!(
            next_check_in(Some(1_000), 999),
            Duration::ZERO,
            "a clock set back must not stop checks"
        );
    }

    /// A unique temp directory removed on drop; avoids a dev-dependency, as in
    /// [`crate::syncignore`]'s tests.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-update-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_marker_next_to_bin_means_a_package_manager_updates() {
        let prefix = TempDir::new();
        let exe = prefix.0.join("bin/pdfs-app");
        assert!(!updates_managed(&exe));
        let marker = prefix.0.join(MANAGED_MARKER);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "").unwrap();
        assert!(updates_managed(&exe));
    }
}
