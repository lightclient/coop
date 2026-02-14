use anyhow::Result;
use rusqlite::{params_from_iter, types::Value};

use super::{RawIndex, SqliteMemory, helpers};

pub(super) fn search_by_file(
    memory: &SqliteMemory,
    path: &str,
    prefix_match: bool,
    limit: usize,
) -> Result<Vec<RawIndex>> {
    let now_ms = helpers::now_ms();
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

    let mut sql = String::from(
        "
        SELECT DISTINCT
            o.id,
            o.title,
            o.type,
            o.store,
            o.created_at,
            o.updated_at,
            o.token_count,
            o.mention_count,
            o.related_people,
            0.0 AS fts_score
        FROM observations o, json_each(o.related_files) AS f
        WHERE o.agent_id = ?
          AND (o.expires_at IS NULL OR o.expires_at > ?)
        ",
    );

    let mut params = vec![
        Value::from(memory.agent_id.clone()),
        Value::from(now_ms),
        Value::from(path.to_owned()),
    ];

    if prefix_match {
        sql.push_str(" AND f.value LIKE ? || '%'");
    } else {
        sql.push_str(" AND f.value = ?");
    }

    sql.push_str(" ORDER BY o.updated_at DESC LIMIT ?");
    params.push(Value::from(limit));

    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), helpers::raw_index_from_row)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }

    drop(stmt);
    drop(conn);
    Ok(out)
}
