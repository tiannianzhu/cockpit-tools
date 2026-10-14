const SESSION_USAGE_ROLLUP: usage_rollup::Rollup = usage_rollup::Rollup {
    source: "session_usage_events",
    prefix: "session_usage_v2",
    width: 60,
    dimensions: &[
        "instance_id",
        "instance_name",
        "model",
        "file_path",
        "session_id",
        "service_tier",
        "context_band",
    ],
    metrics: &[
        usage_rollup::Metric {
            name: "context_input_tokens",
            expression: "MAX($input_tokens,0)",
            real: false,
            maximum: true,
        },
        usage_rollup::Metric {
            name: "input_tokens",
            expression: "MAX($input_tokens,0)",
            real: false,
            maximum: false,
        },
        usage_rollup::Metric {
            name: "cached_input_tokens",
            expression: "MAX($cached_input_tokens,0)",
            real: false,
            maximum: false,
        },
        usage_rollup::Metric {
            name: "output_tokens",
            expression: "MAX($output_tokens,0)",
            real: false,
            maximum: false,
        },
    ],
};

// Query helpers retain their parameterized filters; projection endpoints are
// derived only from timestamp parameters, never from instance text.
fn report_source_for_filter(
    conn: &Connection,
    filter: &str,
    params: &[rusqlite::types::Value],
) -> String {
    let mut values = params.iter();
    let start = if filter.contains("timestamp >= ?") {
        match values.next() {
            Some(rusqlite::types::Value::Integer(v)) => *v,
            _ => 0,
        }
    } else {
        0
    };
    let end = if filter.contains("timestamp <= ?") {
        match values.next() {
            Some(rusqlite::types::Value::Integer(v)) => *v,
            _ => i64::MAX / 2,
        }
    } else {
        i64::MAX / 2
    };
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='session_rewrite_events')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !exists {
        return format!("SELECT *,1 AS request_count,input_tokens AS context_input_tokens,CASE WHEN input_tokens>272000 THEN 'long' ELSE 'short' END AS context_band FROM session_usage_events");
    }
    session_report_source(conn, start, end)
}

#[cfg(test)]
fn drop_session_rollup(conn: &Connection) {
    let mut stmt=conn.prepare("SELECT type,name FROM sqlite_master WHERE name LIKE 'session_usage_v2_%' ORDER BY type DESC").unwrap();
    let entries = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(stmt);
    for (kind, name) in entries {
        if kind == "trigger" || kind == "table" {
            conn.execute_batch(&format!("DROP {kind} IF EXISTS {name}"))
                .unwrap();
        }
    }
}
