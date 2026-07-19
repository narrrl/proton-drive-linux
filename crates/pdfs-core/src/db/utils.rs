//! Small query helpers shared across the table modules.

use rusqlite::{Connection, params};

use crate::Result;

/// Below this length the trigram tokenizer indexes nothing (it needs 3-char
/// grams), so short queries fall back to a `LIKE` scan over `nodes.name`.
pub(super) const TRIGRAM_MIN: usize = 3;

pub(super) fn pair(row: &rusqlite::Row) -> rusqlite::Result<(String, String)> {
    Ok((row.get(0)?, row.get(1)?))
}

/// Drain a `query_map` of [`pair`] rows into a `Vec`, propagating row errors.
pub(super) fn collect_pairs(
    rows: impl Iterator<Item = rusqlite::Result<(String, String)>>,
) -> Result<Vec<(String, String)>> {
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

/// Resolve a node's mountpoint-relative path by walking `parent_uid` to the
/// root via a recursive CTE. The root (the node with no parent) is excluded, so
/// a top-level file `report.pdf` yields `"report.pdf"`, not `"My Files/report.pdf"`.
pub(super) fn path_of(conn: &Connection, uid: &str) -> Result<String> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE anc(uid, parent_uid, name, depth) AS (
           SELECT uid, parent_uid, name, 0 FROM nodes WHERE uid = ?1
           UNION ALL
           SELECT n.uid, n.parent_uid, n.name, anc.depth + 1
           FROM nodes n JOIN anc ON n.uid = anc.parent_uid
         )
         SELECT name, parent_uid FROM anc ORDER BY depth DESC",
    )?;
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
