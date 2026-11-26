//! The control requests about Drive's trash and the local content cache.

use super::*;

/// The requests [`answer`] serves, as a pattern, so the control socket's match
/// hands exactly these over and the compiler still checks that it covers every
/// request.
macro_rules! request {
    () => {
        ::pdfs_core::control::Request::ListTrash
            | ::pdfs_core::control::Request::ListTrashedFolder { .. }
            | ::pdfs_core::control::Request::Restore { .. }
            | ::pdfs_core::control::Request::DeleteForever { .. }
            | ::pdfs_core::control::Request::EmptyTrash
            | ::pdfs_core::control::Request::PurgeCache
            | ::pdfs_core::control::Request::CacheInspect { .. }
            | ::pdfs_core::control::Request::CacheVacuum
            | ::pdfs_core::control::Request::SetCacheBudget { .. }
    };
}
pub(super) use request;

/// Answer a request that [`request!`] matches.
pub(super) fn answer(ask: &Ask<'_>, request: CtlRequest) -> CtlResponse {
    let Ask { core, .. } = *ask;
    match request {
        CtlRequest::ListTrash => match core.trash().list_trash() {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::ListTrashedFolder { uid } => match core.trash().list_trashed_folder(&uid) {
            Ok(entries) => CtlResponse::Entries { entries },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::Restore { uids } => match core.trash().restore(&uids) {
            Ok(n) => {
                core.log_activity(ActivityKind::Restore, format!("{n} item(s)"), "", true);
                CtlResponse::Ok {
                    message: format!("restored {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::DeleteForever { uids } => match core.trash().delete_forever(&uids) {
            Ok(n) => {
                core.log_activity(
                    ActivityKind::DeleteForever,
                    format!("{n} item(s)"),
                    "",
                    true,
                );
                CtlResponse::Ok {
                    message: format!("permanently deleted {n} item(s)"),
                }
            }
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::EmptyTrash => match core.trash().empty_trash() {
            Ok(n) => {
                let items = if n == 1 {
                    "1 item".to_string()
                } else {
                    format!("{n} items")
                };
                core.log_activity(ActivityKind::EmptyTrash, items.clone(), "", true);
                CtlResponse::Ok {
                    message: format!("emptied trash ({items})"),
                }
            }
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::PurgeCache => {
            let freed = core.cache.clear_unpinned();
            CtlResponse::Ok {
                message: format!(
                    "purged {:.1} MiB of unpinned cache",
                    freed as f64 / 1_048_576.0
                ),
            }
        }
        CtlRequest::CacheInspect { deep } => match core.db.stats() {
            Ok(stats) => {
                // The deep check reads every page — tens of seconds on a real
                // database, all of it holding the connection every FUSE handler
                // needs — so it runs on a thread and this request returns
                // whatever the last completed check found. `integrity_checked`
                // tells the caller which answer it is holding, since "no
                // problems found" and "never looked" are both an empty list;
                // `integrity_running` says a fresh answer is on its way.
                let (integrity_problems, integrity_checked, integrity_running) = if deep {
                    core.start_or_read_integrity_check()
                } else {
                    (Vec::new(), false, false)
                };
                CtlResponse::CacheReport {
                    integrity_running,
                    schema_version: stats.schema_version,
                    db_bytes: stats.total_bytes(),
                    db_reclaimable_bytes: stats.reclaimable_bytes(),
                    tables: stats.tables,
                    cache_used: core.cache.usage(),
                    cache_budget: core.cache.budget(),
                    integrity_problems,
                    integrity_checked,
                }
            }
            Err(e) => CtlResponse::error(CoreError::internal(format!("read db stats: {e:?}"))),
        },
        CtlRequest::CacheVacuum => match core.start_vacuum() {
            Ok(()) => CtlResponse::Ok {
                message: "compacting the cache database in the background".into(),
            },
            Err(e) => CtlResponse::error(e),
        },
        CtlRequest::SetCacheBudget { bytes } => {
            core.cache.set_budget(bytes);
            // Persist so the next mount keeps the new cap. Best-effort: a config
            // write failure is reported but the live cap is already applied.
            match AppDirs::new()
                .map(|dirs| dirs.update_config(|cfg| cfg.cache_budget = Some(bytes)))
            {
                Ok(Ok(())) => {
                    core.touch_profile();
                    CtlResponse::Ok {
                        message: format!("cache budget set to {bytes} bytes"),
                    }
                }
                Ok(Err(e)) => CtlResponse::error(CoreError::internal(format!(
                    "budget applied but config write failed: {e}"
                ))),
                Err(e) => CtlResponse::error(CoreError::internal(format!(
                    "budget applied but config unavailable: {e}"
                ))),
            }
        }
        // `control` only hands over what `request!` matches.
        _ => CtlResponse::error(CoreError::internal("not a trash request".to_string())),
    }
}
