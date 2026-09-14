//! Durable workspace storage backed by zvec.
//!
//! Native WAL protects document writes. Pending records contain only affected
//! source metadata until a batch is flushed across all collections. Interrupted
//! batches are invalidated before readers open and rebuilt by normal indexing.

mod backend;
mod codec;
mod dictionary;
mod pending;
pub(crate) mod spi;
mod zvec;

pub(crate) use backend::ZvecStorageFactory;
