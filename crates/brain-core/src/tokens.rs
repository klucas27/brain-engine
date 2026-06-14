//! Lightweight token estimation used for context budgeting.
//!
//! Exact BPE tokenisation would require a ~2 MB tokeniser binary and a nontrivial
//! runtime dependency.  For budget decisions and "tokens saved" estimates the
//! `bytes / 4` heuristic (same as the one used during indexing) is precise enough.

use rusqlite::Connection;

use crate::error::Result;

/// Estimate the number of tokens in `text`.
///
/// Rule: 1 token ≈ 4 bytes of UTF-8 text.  Guarantees at least 1.
pub fn estimate(text: &str) -> usize {
    text.len().div_ceil(4).max(1)
}

/// Sum the `token_estimate` of all indexed chunks, giving a rough token count
/// for the *entire* project if it were sent verbatim to the model.
///
/// Used as the denominator for the tokens-saved metric.
pub fn project_total(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(SUM(token_estimate), 0) FROM chunks",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Count the total number of chunks in the index.
pub fn chunk_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_yields_one() {
        // Even an empty doc is counted as 1 token to avoid divide-by-zero later.
        assert_eq!(estimate(""), 1);
    }

    #[test]
    fn four_ascii_bytes_is_one_token() {
        assert_eq!(estimate("abcd"), 1);
    }

    #[test]
    fn eight_bytes_is_two_tokens() {
        assert_eq!(estimate("abcdefgh"), 2);
    }

    #[test]
    fn round_trip_consistency_with_indexer() {
        // The chunk.rs indexer uses the same formula.  Keep them in sync.
        let text = "fn main() { println!(\"hello\"); }\n";
        assert_eq!(estimate(text), text.len().div_ceil(4).max(1));
    }
}
