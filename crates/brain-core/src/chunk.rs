//! Line-window chunking.
//!
//! Source files are split into overlapping windows of lines. Overlap preserves
//! context that would otherwise be cut mid-function across a chunk boundary,
//! which improves retrieval quality in later phases. Line-based windowing is
//! language-agnostic and deterministic; AST-aware chunking is a possible future
//! refinement but is intentionally out of scope here.

use crate::config::ChunkConfig;
use crate::hash::sha256_hex;

/// A single chunk produced from a file. Line numbers are 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0-based position of this chunk within its file.
    pub ordinal: usize,
    /// First source line covered (1-based, inclusive).
    pub start_line: usize,
    /// Last source line covered (1-based, inclusive).
    pub end_line: usize,
    /// The chunk text (lines joined with `\n`).
    pub content: String,
    /// SHA-256 of `content`, for change detection and embedding reuse.
    pub content_hash: String,
    /// Rough token count (`bytes / 4`), used for context budgeting later.
    pub token_estimate: usize,
}

/// Split `text` into overlapping line windows according to `cfg`.
///
/// Guarantees:
/// * empty / whitespace-only input yields no chunks;
/// * the step is always at least 1 line, so it can never loop forever even if
///   `overlap_lines >= max_lines` (the overlap is clamped);
/// * every source line appears in at least one chunk.
pub fn chunk_text(text: &str, cfg: &ChunkConfig) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() || text.trim().is_empty() {
        return Vec::new();
    }

    let max_lines = cfg.max_lines.max(1);
    // Clamp overlap so the window always advances by at least one line.
    let overlap = cfg.overlap_lines.min(max_lines.saturating_sub(1));
    let step = max_lines - overlap;

    let mut chunks = Vec::new();
    let mut start = 0usize; // 0-based index into `lines`
    let mut ordinal = 0usize;

    while start < lines.len() {
        let end = (start + max_lines).min(lines.len()); // exclusive
        let content = lines[start..end].join("\n");
        let token_estimate = content.len().div_ceil(4).max(1);

        chunks.push(Chunk {
            ordinal,
            start_line: start + 1,
            end_line: end, // 1-based inclusive == 0-based exclusive end
            content_hash: sha256_hex(content.as_bytes()),
            content,
            token_estimate,
        });

        ordinal += 1;
        // Stop once this window reached the end, regardless of overlap.
        if end == lines.len() {
            break;
        }
        start += step;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max: usize, overlap: usize) -> ChunkConfig {
        ChunkConfig {
            max_lines: max,
            overlap_lines: overlap,
        }
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(chunk_text("", &cfg(10, 2)).is_empty());
        assert!(chunk_text("   \n  \n", &cfg(10, 2)).is_empty());
    }

    #[test]
    fn single_window_covers_short_file() {
        let chunks = chunk_text("a\nb\nc", &cfg(10, 2));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert_eq!(chunks[0].content, "a\nb\nc");
    }

    #[test]
    fn windows_overlap_and_cover_all_lines() {
        let text = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_text(&text, &cfg(4, 1)); // step = 3
                                                    // windows: [1..4],[4..7],[7..10],[10..10]
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 4);
        assert_eq!(chunks[1].start_line, 4); // overlap line 4
        assert_eq!(chunks.last().unwrap().end_line, 10);
        // ordinals are contiguous
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ordinal, i);
        }
    }

    #[test]
    fn overlap_ge_max_does_not_hang() {
        // overlap >= max would make step 0; ensure it is clamped.
        let text = "a\nb\nc\nd\ne";
        let chunks = chunk_text(text, &cfg(2, 5));
        assert!(chunks.len() >= 3);
        assert_eq!(chunks.last().unwrap().end_line, 5);
    }
}
