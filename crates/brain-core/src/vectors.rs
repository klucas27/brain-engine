//! LanceDB vector store for chunk embeddings.
//!
//! Each indexed chunk that has been embedded gets a row in the `chunks` table
//! inside the project's `.brain/vectors/` directory.  The `chunk_id` column
//! mirrors the SQLite primary key so the two stores stay in sync with a simple
//! integer comparison.
//!
//! LanceDB is async; this module wraps every call in a `current_thread` Tokio
//! runtime so the public API remains synchronous and compatible with the
//! Rayon-based indexer.  The runtime is stored inside [`VectorStore`] to avoid
//! repeated creation overhead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use crate::error::{BrainError, Result};

const CHUNKS_TABLE: &str = "chunks";

/// A fully embedded chunk ready to be written to the vector store.
#[derive(Debug, Clone)]
pub struct ChunkVector {
    /// Primary key matching the SQLite `chunks.id` column.
    pub chunk_id: i64,
    /// Embedding of length equal to the model's output dimension.
    pub vector: Vec<f32>,
    /// Project-relative file path (for debugging / future retrieval).
    pub file_path: String,
    /// Chunk text (for semantic cache and future re-ranking).
    pub content: String,
}

/// Thin synchronous wrapper around a LanceDB database in `.brain/vectors/`.
pub struct VectorStore {
    /// Path to the LanceDB directory.
    path: PathBuf,
    /// Single-threaded Tokio runtime used to drive async LanceDB operations.
    rt: tokio::runtime::Runtime,
}

impl VectorStore {
    /// Open (or create) the LanceDB database at `path`.
    ///
    /// Creates the directory if it does not exist.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path).map_err(|e| BrainError::io(path, e))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| BrainError::Vector(format!("tokio runtime init: {e}")))?;
        Ok(Self {
            path: path.to_path_buf(),
            rt,
        })
    }

    /// Insert a batch of chunk vectors, creating the table if needed.
    ///
    /// `dim` is the embedding dimension; it must be consistent with the
    /// schema of any existing table (enforced by the model-pin check above
    /// this layer).
    pub fn upsert(&self, chunks: &[ChunkVector], dim: usize) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let path = self.path_str()?.to_owned();
        let schema = make_schema(dim);
        let batch = build_batch(chunks, schema.clone(), dim)?;

        self.rt.block_on(async {
            let db = lancedb::connect(&path)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            let tbl = get_or_create_table(&db, dim).await?;
            let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
            tbl.add(Box::new(reader))
                .execute()
                .await
                .map(|_| ())
                .map_err(|e| BrainError::Vector(e.to_string()))
        })
    }

    /// Delete every row whose `chunk_id` is not present in `valid_ids`.
    ///
    /// Call this after re-indexing to prune embeddings for deleted/replaced
    /// chunks.  A no-op when the table does not exist yet.
    pub fn delete_orphans(&self, valid_ids: &[i64]) -> Result<()> {
        let path = self.path_str()?.to_owned();

        self.rt.block_on(async {
            let db = lancedb::connect(&path)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            let tables = db
                .table_names()
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            if !tables.contains(&CHUNKS_TABLE.to_string()) {
                return Ok(());
            }
            let tbl = db
                .open_table(CHUNKS_TABLE)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;

            if valid_ids.is_empty() {
                // No chunks in SQLite at all — clear the whole table.
                tbl.delete("chunk_id IS NOT NULL")
                    .await
                    .map(|_| ())
                    .map_err(|e| BrainError::Vector(e.to_string()))
            } else {
                // Only i64 values — no injection risk.
                let ids_str = valid_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                tbl.delete(&format!("chunk_id NOT IN ({ids_str})"))
                    .await
                    .map(|_| ())
                    .map_err(|e| BrainError::Vector(e.to_string()))
            }
        })
    }

    /// Total number of vectors currently in the store.  Returns `0` when the
    /// table has never been created.
    pub fn count(&self) -> Result<usize> {
        let path = self.path_str()?.to_owned();

        self.rt.block_on(async {
            let db = lancedb::connect(&path)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            let tables = db
                .table_names()
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            if !tables.contains(&CHUNKS_TABLE.to_string()) {
                return Ok(0);
            }
            let tbl = db
                .open_table(CHUNKS_TABLE)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            tbl.count_rows(None)
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))
        })
    }

    /// Drop the `chunks` table entirely.
    ///
    /// Used when the embedding model changes and a full reindex is forced.
    pub fn clear(&self) -> Result<()> {
        let path = self.path_str()?.to_owned();

        self.rt.block_on(async {
            let db = lancedb::connect(&path)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            let tables = db
                .table_names()
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            if tables.contains(&CHUNKS_TABLE.to_string()) {
                db.drop_table(CHUNKS_TABLE)
                    .await
                    .map_err(|e| BrainError::Vector(e.to_string()))?;
            }
            Ok(())
        })
    }

    /// ANN vector search: return the `top_k` closest chunks to `query`.
    ///
    /// Returns an empty list when the table does not yet exist (no embeddings
    /// stored) or when there are fewer than `top_k` rows.
    /// The `score` field is the cosine similarity (0–1), derived from the
    /// raw cosine distance returned by LanceDB (`score = 1 – distance`).
    pub fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<SearchHit>> {
        if top_k == 0 || query.is_empty() {
            return Ok(Vec::new());
        }
        let path = self.path_str()?.to_owned();
        let query_vec: Vec<f32> = query.to_vec();

        self.rt.block_on(async {
            let db = lancedb::connect(&path)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;

            let tables = db
                .table_names()
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            if !tables.contains(&CHUNKS_TABLE.to_string()) {
                return Ok(Vec::new());
            }

            let tbl = db
                .open_table(CHUNKS_TABLE)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;

            let row_count = tbl
                .count_rows(None)
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;
            if row_count == 0 {
                return Ok(Vec::new());
            }

            // Clamp to avoid requesting more rows than exist.
            let limit = top_k.min(row_count);

            let stream = tbl
                .query()
                .nearest_to(query_vec.as_slice())
                .map_err(|e| BrainError::Vector(e.to_string()))?
                .distance_type(lancedb::DistanceType::Cosine)
                .limit(limit)
                .execute()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;

            let batches: Vec<RecordBatch> = stream
                .try_collect()
                .await
                .map_err(|e| BrainError::Vector(e.to_string()))?;

            let mut hits: Vec<SearchHit> = Vec::new();
            for batch in &batches {
                extract_hits(batch, &mut hits)?;
            }
            Ok(hits)
        })
    }

    fn path_str(&self) -> Result<&str> {
        self.path
            .to_str()
            .ok_or_else(|| BrainError::Vector("vector store path contains non-UTF-8 bytes".into()))
    }
}

// ---------------------------------------------------------------------------
// Search result type
// ---------------------------------------------------------------------------

/// A single result from an ANN vector search.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Primary key of the chunk in the SQLite `chunks` table.
    pub chunk_id: i64,
    /// Project-relative file path.
    pub file_path: String,
    /// Raw chunk text.
    pub content: String,
    /// Cosine similarity score in [0, 1]; higher is more relevant.
    pub score: f32,
}

/// Extract [`SearchHit`]s from a single Arrow record batch.
fn extract_hits(batch: &RecordBatch, hits: &mut Vec<SearchHit>) -> Result<()> {
    let chunk_ids = batch
        .column_by_name("chunk_id")
        .ok_or_else(|| BrainError::Vector("search result missing 'chunk_id' column".into()))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| BrainError::Vector("'chunk_id' column is not Int64".into()))?;

    let file_paths = batch
        .column_by_name("file_path")
        .ok_or_else(|| BrainError::Vector("search result missing 'file_path' column".into()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| BrainError::Vector("'file_path' column is not Utf8".into()))?;

    let contents = batch
        .column_by_name("content")
        .ok_or_else(|| BrainError::Vector("search result missing 'content' column".into()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| BrainError::Vector("'content' column is not Utf8".into()))?;

    let distances = batch
        .column_by_name("_distance")
        .ok_or_else(|| BrainError::Vector("search result missing '_distance' column".into()))?
        .as_any()
        .downcast_ref::<Float32Array>()
        .ok_or_else(|| BrainError::Vector("'_distance' column is not Float32".into()))?;

    for i in 0..batch.num_rows() {
        // Cosine distance ∈ [0, 2] for unit vectors; similarity = 1 – distance.
        // We clamp to [0, 1] since floating-point rounding may push past bounds.
        let score = (1.0f32 - distances.value(i)).clamp(0.0, 1.0);
        hits.push(SearchHit {
            chunk_id: chunk_ids.value(i),
            file_path: file_paths.value(i).to_string(),
            content: contents.value(i).to_string(),
            score,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Open the existing `chunks` table or create an empty one with the right schema.
async fn get_or_create_table(db: &lancedb::Connection, dim: usize) -> Result<lancedb::Table> {
    let tables = db
        .table_names()
        .execute()
        .await
        .map_err(|e| BrainError::Vector(e.to_string()))?;

    if tables.contains(&CHUNKS_TABLE.to_string()) {
        db.open_table(CHUNKS_TABLE)
            .execute()
            .await
            .map_err(|e| BrainError::Vector(e.to_string()))
    } else {
        create_empty_table(db, dim).await
    }
}

/// Bootstrap an empty table so the schema is fixed before the first insert.
async fn create_empty_table(db: &lancedb::Connection, dim: usize) -> Result<lancedb::Table> {
    let schema = make_schema(dim);

    // Arrow requires at least one column; build a zero-row batch.
    let empty_ids: ArrayRef = Arc::new(Int64Array::from(Vec::<i64>::new()));
    let empty_floats = Arc::new(Float32Array::from(Vec::<f32>::new()));
    let empty_vectors = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dim as i32,
        empty_floats,
        None,
    )
    .expect("zero-row fixed-size list always succeeds");
    let empty_str: ArrayRef = Arc::new(StringArray::from(Vec::<String>::new()));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            empty_ids,
            Arc::new(empty_vectors) as ArrayRef,
            empty_str.clone(),
            empty_str,
        ],
    )
    .expect("zero-row record batch always succeeds");

    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    db.create_table(CHUNKS_TABLE, Box::new(reader))
        .execute()
        .await
        .map_err(|e| BrainError::Vector(e.to_string()))
}

/// Build the LanceDB schema for a given embedding dimension.
fn make_schema(dim: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("chunk_id", DataType::Int64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
    ]))
}

/// Convert a slice of [`ChunkVector`] into an Arrow [`RecordBatch`].
fn build_batch(chunks: &[ChunkVector], schema: Arc<Schema>, dim: usize) -> Result<RecordBatch> {
    let chunk_ids: Vec<i64> = chunks.iter().map(|c| c.chunk_id).collect();
    let all_floats: Vec<f32> = chunks
        .iter()
        .flat_map(|c| c.vector.iter().copied())
        .collect();
    let file_paths: Vec<&str> = chunks.iter().map(|c| c.file_path.as_str()).collect();
    let contents: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();

    let float_arr = Arc::new(Float32Array::from(all_floats));
    let vector_arr = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dim as i32,
        float_arr,
        None,
    )
    .map_err(|e| BrainError::Vector(format!("building vector column: {e}")))?;

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(chunk_ids)) as ArrayRef,
            Arc::new(vector_arr) as ArrayRef,
            Arc::new(StringArray::from(file_paths)) as ArrayRef,
            Arc::new(StringArray::from(contents)) as ArrayRef,
        ],
    )
    .map_err(|e| BrainError::Vector(format!("building record batch: {e}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dummy_vectors(n: usize, dim: usize) -> Vec<ChunkVector> {
        (0..n)
            .map(|i| ChunkVector {
                chunk_id: i as i64,
                vector: vec![i as f32 / n as f32; dim],
                file_path: format!("src/file{i}.rs"),
                content: format!("chunk content {i}"),
            })
            .collect()
    }

    #[test]
    fn open_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let vecs_dir = tmp.path().join("vectors");
        assert!(!vecs_dir.exists());
        VectorStore::open(&vecs_dir).unwrap();
        assert!(vecs_dir.is_dir());
    }

    #[test]
    fn count_on_empty_store_is_zero() {
        let tmp = TempDir::new().unwrap();
        let vs = VectorStore::open(tmp.path()).unwrap();
        assert_eq!(vs.count().unwrap(), 0);
    }

    #[test]
    fn upsert_and_count() {
        let tmp = TempDir::new().unwrap();
        let vs = VectorStore::open(tmp.path()).unwrap();
        let dim = 4;
        let chunks = dummy_vectors(3, dim);
        vs.upsert(&chunks, dim).unwrap();
        assert_eq!(vs.count().unwrap(), 3);
    }

    #[test]
    fn delete_orphans_removes_stale_rows() {
        let tmp = TempDir::new().unwrap();
        let vs = VectorStore::open(tmp.path()).unwrap();
        let dim = 4;
        vs.upsert(&dummy_vectors(5, dim), dim).unwrap();
        // Keep only chunk_ids 0 and 2.
        vs.delete_orphans(&[0, 2]).unwrap();
        assert_eq!(vs.count().unwrap(), 2);
    }

    #[test]
    fn clear_empties_the_store() {
        let tmp = TempDir::new().unwrap();
        let vs = VectorStore::open(tmp.path()).unwrap();
        let dim = 4;
        vs.upsert(&dummy_vectors(4, dim), dim).unwrap();
        vs.clear().unwrap();
        assert_eq!(vs.count().unwrap(), 0);
    }

    #[test]
    fn delete_orphans_with_empty_valid_set_clears_all() {
        let tmp = TempDir::new().unwrap();
        let vs = VectorStore::open(tmp.path()).unwrap();
        let dim = 4;
        vs.upsert(&dummy_vectors(3, dim), dim).unwrap();
        vs.delete_orphans(&[]).unwrap();
        assert_eq!(vs.count().unwrap(), 0);
    }
}
