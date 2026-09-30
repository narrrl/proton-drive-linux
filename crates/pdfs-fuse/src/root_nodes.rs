//! Materialization of independent roots, rather than siblings of one folder.

use std::future::Future;

/// Keep each root in its own SDK request. In proton-drive-rs 0.7.1 both
/// full and light enumeration cache parent keys by `Option<LinkId>` within
/// a volume batch: unrelated parentless roots collide at `None`.
///
/// Sequential requests bound concurrency to one and preserve input order.
/// Keep the SDK's omission/logging of missing or undecryptable nodes and
/// propagate request failures, rather than presenting an outage as an empty
/// listing. Ordinary folder children should still use SDK batching directly.
pub(crate) async fn enumerate_roots<'a, U, N, E, F, Fut>(
    uids: &'a [U],
    mut enumerate: F,
) -> Result<Vec<N>, E>
where
    F: FnMut(&'a [U]) -> Fut,
    Fut: Future<Output = Result<Vec<N>, E>>,
{
    let mut nodes = Vec::with_capacity(uids.len());
    for root in uids.chunks(1) {
        nodes.extend(enumerate(root).await?);
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn empty_roots_make_no_request() {
        let mut calls = 0;
        let actual = enumerate_roots::<u8, u8, (), _, _>(&[], |_| {
            calls += 1;
            std::future::ready(Ok(Vec::new()))
        })
        .await
        .unwrap();
        assert!(actual.is_empty());
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn omitted_root_does_not_hide_later_roots() {
        let actual = enumerate_roots(&[1, 2, 3], |batch| {
            std::future::ready(Ok::<_, ()>(
                batch.iter().copied().filter(|id| *id != 2).collect(),
            ))
        })
        .await
        .unwrap();
        assert_eq!(actual, [1, 3]);
    }

    #[tokio::test]
    async fn request_failure_is_not_silently_skipped() {
        let mut calls = Vec::new();
        let actual = enumerate_roots(&[1, 2, 3], |batch| {
            calls.extend_from_slice(batch);
            std::future::ready(if batch.contains(&2) {
                Err("request failed")
            } else {
                Ok(batch.to_vec())
            })
        })
        .await;
        assert_eq!(actual, Err("request failed"));
        assert_eq!(calls, [1, 2]);
    }

    #[tokio::test]
    async fn singleton_root_is_unchanged() {
        let roots = [Root {
            volume: 1,
            id: 1,
            share_key: 10,
        }];
        assert_eq!(
            enumerate_roots(&roots, buggy_sdk_batch).await.unwrap(),
            roots
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Root {
        volume: u8,
        id: u8,
        share_key: u8,
    }

    // Model the SDK 0.7.1 per-volume parent-key cache. Every root has
    // parent_id=None, but each is encrypted with its own share key.
    async fn buggy_sdk_batch(roots: &[Root]) -> Result<Vec<Root>, &'static str> {
        let mut keys = HashMap::new();
        Ok(roots
            .iter()
            .filter(|root| {
                let key = keys
                    .entry((root.volume, None::<u8>))
                    .or_insert(root.share_key);
                *key == root.share_key
            })
            .cloned()
            .collect())
    }

    #[tokio::test]
    async fn independent_roots_keep_distinct_share_keys_and_input_order() {
        let roots = vec![
            Root {
                volume: 1,
                id: 3,
                share_key: 30,
            },
            Root {
                volume: 2,
                id: 1,
                share_key: 10,
            },
            Root {
                volume: 1,
                id: 2,
                share_key: 20,
            },
        ];
        assert_eq!(buggy_sdk_batch(&roots).await.unwrap().len(), 2);
        let actual = enumerate_roots(&roots, buggy_sdk_batch).await.unwrap();
        assert_eq!(actual, roots);
    }
}
