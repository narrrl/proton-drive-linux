//! Trace the key chain that opens My Files, for `pdfs diagnose`.
//!
//! The daemon only says "missing key" when the My Files share does not open
//! (#40). This walks the chain the other way and names the link that breaks:
//! the share's passphrase is encrypted to an address key, whose token is
//! encrypted to an account key, which opens with the stored passphrase. Every
//! finding holds key IDs and flags only, never key material, so the output is
//! safe to post in an issue.

use proton_sdk::account::{AddressKeyReport, KeyReport, UserKeyReport};
use proton_sdk::ids::AddressId;

use crate::auth;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub label: String,
    pub detail: String,
}

/// The address and recipients of the My Files share passphrase, or why they
/// could not be read.
pub type ShareRecipients = std::result::Result<(AddressId, Vec<Option<String>>), String>;

/// Resume the stored session and ask Proton for the account's keys and the
/// My Files share. Fails when there is no session, the keys are locked, or
/// Proton cannot be reached; reading the share is allowed to fail on its own.
///
/// The session's tokens are the daemon's too. A refresh here spends the
/// refresh token the daemon holds, which it recovers from by resuming the
/// stored session (see [`auth::stored_session_is_newer`]).
pub async fn run() -> Result<(KeyReport, ShareRecipients)> {
    let (client, _session) = auth::resume_client().await?;
    let report = client.account().key_report().await?;
    let share = client
        .my_files_share_recipients()
        .await
        .map_err(|e| e.to_string());
    Ok((report, share))
}

/// One finding per key, then one for the My Files share.
pub fn findings(report: &KeyReport, share: &ShareRecipients) -> Vec<Finding> {
    let mut findings = Vec::new();
    for key in &report.user_keys {
        findings.push(user_key_finding(key));
    }
    for address in &report.addresses {
        for key in &address.keys {
            findings.push(address_key_finding(report, &address.email, key));
        }
    }
    findings.push(share_finding(report, share));
    findings
}

fn user_key_finding(key: &UserKeyReport) -> Finding {
    let label = format!("account key {}", short(key.id.as_str()));
    let (level, detail) = match (key.active, key.unlocks) {
        (true, true) => (Level::Ok, "opens".to_string()),
        (true, false) => (
            Level::Fail,
            format!(
                "does not open: {}",
                key.error.as_deref().unwrap_or("unknown")
            ),
        ),
        (false, true) => (Level::Ok, "inactive, opens with your password".to_string()),
        (false, false) => (Level::Ok, "inactive, does not open".to_string()),
    };
    Finding {
        level,
        label,
        detail: with_primary(key.primary, detail),
    }
}

fn address_key_finding(report: &KeyReport, email: &str, key: &AddressKeyReport) -> Finding {
    let label = format!("{email} key {}", short(key.id.as_str()));
    let (level, detail) = if !key.active {
        (Level::Ok, "inactive, not used".to_string())
    } else if key.unlocks {
        (Level::Ok, "opens".to_string())
    } else {
        // A non-primary key matters only for what was encrypted to it; the
        // share finding says whether My Files is.
        let level = if key.primary {
            Level::Fail
        } else {
            Level::Warn
        };
        (level, format!("does not open: {}", why_closed(report, key)))
    };
    Finding {
        level,
        label,
        detail: with_primary(key.primary, detail),
    }
}

/// Why an active address key does not open, named by the account key its
/// token was meant for when the token has one.
fn why_closed(report: &KeyReport, key: &AddressKeyReport) -> String {
    let error = key.error.as_deref().unwrap_or("unknown");
    let Some(recipients) = &key.token_recipients else {
        return error.to_string();
    };
    let named: Vec<&str> = recipients.iter().flatten().map(String::as_str).collect();
    if let Some(user_key) = named.iter().find_map(|r| report.user_key_named(r)) {
        let id = short(user_key.id.as_str());
        return match (user_key.active, user_key.unlocks) {
            (false, true) => {
                format!(
                    "its token is for inactive account key {id}, which opens with your password"
                )
            }
            (false, false) => {
                format!("its token is for inactive account key {id}, which does not open either")
            }
            (true, false) => format!("its token is for account key {id}, which does not open"),
            // The token opened; the key itself refused its passphrase.
            (true, true) => error.to_string(),
        };
    }
    match named.first() {
        Some(recipient) => {
            format!("its token is for key {recipient}, which is none of your account keys")
        }
        None if recipients.is_empty() => error.to_string(),
        None => "its token does not name the key it is for".to_string(),
    }
}

fn share_finding(report: &KeyReport, share: &ShareRecipients) -> Finding {
    let label = "My Files share".to_string();
    let (level, detail) = match share {
        Err(e) => (Level::Warn, format!("could not be read: {e}")),
        Ok((address_id, recipients)) => {
            let named: Vec<&str> = recipients.iter().flatten().map(String::as_str).collect();
            match named.iter().find_map(|r| report.address_key_named(r)) {
                Some((address, key)) => {
                    let name = format!("{} key {}", address.email, short(key.id.as_str()));
                    if key.unlocks {
                        (Level::Ok, format!("encrypted to {name}, which opens"))
                    } else {
                        (
                            Level::Fail,
                            format!("encrypted to {name}, which does not open"),
                        )
                    }
                }
                None => (
                    Level::Fail,
                    match named.first() {
                        Some(recipient) => format!(
                            "encrypted to key {recipient}, which is none of the keys of address {address_id}"
                        ),
                        None => "its passphrase does not name the key it is for".to_string(),
                    },
                ),
            }
        }
    };
    Finding {
        level,
        label,
        detail,
    }
}

fn with_primary(primary: bool, detail: String) -> String {
    if primary {
        format!("primary, {detail}")
    } else {
        detail
    }
}

/// The start of a Proton key ID: enough to tell keys apart and to find them
/// in the daemon log, which prints the whole ID.
fn short(id: &str) -> String {
    match id.char_indices().nth(8) {
        Some((end, _)) => format!("{}…", &id[..end]),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_sdk::account::AddressReport;
    use proton_sdk::ids::{AddressKeyId, UserKeyId};

    fn user_key(id: &str, active: bool, unlocks: bool, pgp: &str) -> UserKeyReport {
        UserKeyReport {
            id: UserKeyId::new(id),
            primary: active,
            active,
            unlocks,
            error: (!unlocks).then(|| "unlock: wrong passphrase".to_string()),
            key_ids: vec![pgp.to_string()],
        }
    }

    fn address_key(id: &str, primary: bool, token_for: &str, pgp: &str) -> AddressKeyReport {
        AddressKeyReport {
            id: AddressKeyId::new(id),
            primary,
            active: true,
            token_recipients: Some(vec![Some(token_for.to_string())]),
            unlocks: token_for == "u1",
            error: (token_for != "u1").then(|| "token: failed to decrypt: missing key".to_string()),
            key_ids: vec![pgp.to_string()],
        }
    }

    /// The account in #40, as far as its log shows: the primary address key
    /// opens, an older one does not, and My Files is encrypted to the older one.
    fn report(stranded_token_for: &str) -> KeyReport {
        KeyReport {
            user_keys: vec![
                user_key("UserKeyOne-123", true, true, "u1"),
                user_key("UserKeyTwo-456", false, true, "u2"),
            ],
            addresses: vec![AddressReport {
                id: AddressId::new("address-1"),
                email: "user@proton.me".to_string(),
                keys: vec![
                    address_key("AddrKeyNew-789", true, "u1", "a1"),
                    address_key("AddrKeyOld-000", false, stranded_token_for, "a2"),
                ],
            }],
        }
    }

    #[test]
    fn a_stranded_address_key_is_traced_to_the_inactive_account_key() {
        let report = report("u2");
        let share = Ok((AddressId::new("address-1"), vec![Some("a2".to_string())]));
        let findings = findings(&report, &share);

        assert_eq!(findings[0].detail, "primary, opens");
        assert_eq!(findings[1].detail, "inactive, opens with your password");
        assert_eq!(findings[2].level, Level::Ok);
        assert_eq!(findings[3].level, Level::Warn);
        assert_eq!(findings[3].label, "user@proton.me key AddrKeyO…");
        assert_eq!(
            findings[3].detail,
            "does not open: its token is for inactive account key UserKeyT…, which opens with your password"
        );
        assert_eq!(findings[4].level, Level::Fail);
        assert_eq!(
            findings[4].detail,
            "encrypted to user@proton.me key AddrKeyO…, which does not open"
        );
    }

    #[test]
    fn a_token_for_a_key_the_account_does_not_list_says_so() {
        let report = report("ffff");
        let detail = &findings(&report, &Err("offline".to_string()))[3].detail;
        assert_eq!(
            detail,
            "does not open: its token is for key ffff, which is none of your account keys"
        );
    }

    #[test]
    fn a_share_that_opens_and_one_that_cannot_be_read() {
        let report = report("u2");
        let open = Ok((AddressId::new("address-1"), vec![Some("a1".to_string())]));
        let last = findings(&report, &open).pop().unwrap();
        assert_eq!(last.level, Level::Ok);
        assert_eq!(
            last.detail,
            "encrypted to user@proton.me key AddrKeyN…, which opens"
        );

        let last = findings(&report, &Err("offline".to_string()))
            .pop()
            .unwrap();
        assert_eq!(last.level, Level::Warn);
        assert_eq!(last.detail, "could not be read: offline");
    }
}
