//! Small query helpers shared across the table modules.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

/// Below this length the trigram tokenizer indexes nothing (it needs 3-char
/// grams), so short queries fall back to a `LIKE` scan over `nodes.name`.
pub(super) const TRIGRAM_MIN: usize = 3;

/// One raw search row: the node's JSON, its uid, and its stored path — `None`
/// only for a row written before the `path` column existed, which
/// [`path_of`] repairs by walking.
pub(super) type HitRow = (String, String, Option<String>);

pub(super) fn hit_row(row: &rusqlite::Row) -> rusqlite::Result<HitRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

/// Drain a `query_map` of [`hit_row`] rows into a `Vec`, propagating row errors.
pub(super) fn collect_hits(
    rows: impl Iterator<Item = rusqlite::Result<HitRow>>,
) -> Result<Vec<HitRow>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Escape `LIKE` wildcards in a user query so `%` and `_` match literally
/// (paired with `ESCAPE '\'` in the statement).
pub(super) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// How deep the parent walk may go before it gives up. Real Drive trees are
/// nowhere near this; a parent *cycle* is unbounded, and this query is on the
/// path of every search result, including the `LIKE` lane that reads `nodes`
/// directly and so never passed through the index's cycle check.
pub(super) const MAX_PATH_DEPTH: usize = 256;

/// Append `name` to a parent's stored path. An empty parent path is the root,
/// whose name is the mount and so contributes nothing.
pub(super) fn join_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}

/// Resolve a node's mountpoint-relative path. The root (the node with no
/// parent) is excluded, so a top-level file `report.pdf` yields
/// `"report.pdf"`, not `"My Files/report.pdf"`.
///
/// Reads the stored `nodes.path`, which the write path maintains (see
/// `upsert_node_tx`). Falls back to [`walk_path_of`] for a row that predates the
/// column or was never written through — a search serving a keystroke used to
/// run that walk once per candidate, which is the reason the column exists.
pub(super) fn path_of(conn: &Connection, uid: &str) -> Result<String> {
    let stored: Option<Option<String>> = conn
        .query_row("SELECT path FROM nodes WHERE uid = ?1", params![uid], |r| {
            r.get(0)
        })
        .optional()?;
    match stored.flatten() {
        Some(path) => Ok(path),
        None => walk_path_of(conn, uid),
    }
}

/// Resolve a path by walking `parent_lid` to the root via a recursive CTE.
///
/// A cycle in the parents is corrupt data the API can still hand us, and
/// `UNION ALL` over one never terminates — the walk is therefore depth-capped
/// and returns the truncated path rather than hanging the caller.
pub(super) fn walk_path_of(conn: &Connection, uid: &str) -> Result<String> {
    let mut stmt = conn.prepare(&format!(
        "WITH RECURSIVE anc(parent_lid, parent_uid, name, depth) AS (
           SELECT parent_lid, parent_uid, name, 0 FROM nodes WHERE uid = ?1
           UNION ALL
           SELECT n.parent_lid, n.parent_uid, n.name, anc.depth + 1
           FROM nodes n JOIN anc ON n.lid = anc.parent_lid
           WHERE anc.depth < {MAX_PATH_DEPTH}
         )
         SELECT name, parent_uid FROM anc ORDER BY depth DESC"
    ))?;
    let rows = stmt.query_map(params![uid], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut parts = Vec::new();
    for r in rows {
        let (name, parent_uid) = r?;
        // Skip the root node (no parent); its name is the mount itself.
        if parent_uid.is_some() {
            parts.push(name);
        }
    }
    Ok(parts.join("/"))
}
