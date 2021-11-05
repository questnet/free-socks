use anyhow::{anyhow, bail, Result};
use derive_more::{Display, From, Into};
use serde_json::Value;

/// A FreeSWITCH specific database query result.
#[derive(Clone, Default, Debug)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    // ADR:
    // Even though the default CSV format is a lot shorter because it transfer table column headers
    // once only, we use JSON here to get around escaping and encoding issues.
    pub(crate) fn from_json(json: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(json)?;
        let map = value
            .as_object()
            .ok_or_else(|| anyhow!("Expected Object"))?;
        let row_count = map
            .get("row_count")
            .ok_or_else(|| anyhow!("Expected `row_count` JSON member"))?;
        let no_rows = Value::Array(Vec::new());
        let rows = {
            if row_count == 0 {
                // JSON array does not contain `rows` if `row_count` is 0
                &no_rows
            } else {
                map.get("rows")
                    .ok_or_else(|| anyhow!("Expected `rows` JSON member"))?
            }
        };
        let rows = rows
            .as_array()
            .ok_or_else(|| anyhow!("Expected `rows` to be a JSON array"))?;
        let mut columns = Vec::new();
        let mut result_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let row = row
                .as_object()
                .ok_or_else(|| anyhow!("Expected row Object"))?;
            if columns.is_empty() {
                columns = row.keys().cloned().collect();
            }
            if row.len() != columns.len() {
                bail!(
                    "Inconsistent query table, seen {} columns, but one row had {} columns",
                    columns.len(),
                    row.len()
                )
            }
            let result_row: Result<Vec<_>> = row
                .values()
                .map(|v| {
                    v.as_str()
                        .map(|str| str.to_owned())
                        .ok_or_else(|| anyhow!("`row` value is not a string"))
                })
                .collect();
            result_rows.push(result_row?);
        }
        Ok(Self {
            columns,
            rows: result_rows,
        })
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, From, Display, Into)]
pub(crate) struct Count(usize);

impl Count {
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(json)?;
        let map = value
            .as_object()
            .ok_or_else(|| anyhow!("Expected Object"))?;
        let row_count = map
            .get("row_count")
            .ok_or_else(|| anyhow!("Expected `row_count` JSON member"))?;
        let row_count = row_count
            .as_u64()
            .ok_or_else(|| anyhow!("Expected `row_count` to be an unsigned integer"))?;
        let row_count = usize::try_from(row_count)?;
        Ok(row_count.into())
    }
}

#[cfg(test)]
mod tests {
    use super::Table;

    #[test]
    fn row_count_0_and_no_rows_returns_an_empty_table() {
        let table = Table::from_json(br#"{"row_count":0}"#).unwrap();
        assert!(table.rows.is_empty())
    }
}
