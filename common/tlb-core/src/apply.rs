//! The business apply step, identical for project A and project B.
//!
//! Events are coalesced per order inside the batch (the most advanced state
//! wins) and merged into `orders` with a forward-only rule: a state only
//! replaces a lower one, and a `pending` event creates the order. The result
//! is idempotent and independent of arrival order.

/// Builds the MERGE statement that applies the events of `source_table`
/// selected by `filter` (an SQL predicate on alias `b`, or empty for all rows).
pub fn merge_sql(source_table: &str, filter: &str) -> String {
    let where_clause = if filter.is_empty() {
        String::new()
    } else {
        format!("WHERE {filter}")
    };
    format!(
        "MERGE INTO orders AS t
USING (
    SELECT x.order_id, x.status_code, x.source_ref, x.payload
      FROM (SELECT b.order_id,
                   b.status_code,
                   CAST('{source_table}:' || CAST(b.id AS VARCHAR(36)) AS VARCHAR(64)) AS source_ref,
                   b.payload,
                   ROW_NUMBER() OVER (PARTITION BY b.order_id
                                      ORDER BY b.status_code DESC, b.id DESC) AS rn
              FROM {source_table} AS b
              {where_clause}) AS x
     WHERE x.rn = 1
) AS s
ON (t.id = s.order_id)
WHEN MATCHED AND t.status_code < s.status_code THEN
    UPDATE SET status_code = s.status_code,
               source_ref  = s.source_ref,
               updated_at  = CURRENT_TIMESTAMP
WHEN NOT MATCHED THEN
    INSERT (id, status_code, source_ref, data, created_at, updated_at)
    VALUES (s.order_id, s.status_code, s.source_ref, s.payload, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
    )
}

/// Builds the audit statement used by the correctness tests (AUDIT=1).
pub fn audit_sql(source_table: &str, filter: &str) -> String {
    let where_clause = if filter.is_empty() {
        String::new()
    } else {
        format!(" WHERE {filter}")
    };
    format!(
        "INSERT INTO audit_consumed (order_id, status_code, consumed_at)
SELECT b.order_id, b.status_code, CURRENT_TIMESTAMP FROM {source_table} AS b{where_clause}"
    )
}

/// `$first, $first+1, ..., $first+count-1` for portable IN lists.
pub fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_sequential() {
        assert_eq!(placeholders(1, 3), "$1, $2, $3");
        assert_eq!(placeholders(2, 1), "$2");
    }

    #[test]
    fn merge_without_filter_has_no_where() {
        let sql = merge_sql("buffer_0", "");
        assert!(sql.contains("FROM buffer_0 AS b"));
        assert!(!sql.contains("WHERE b."));
    }
}
