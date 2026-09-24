//! Synchronous, background-worker-only local embeddings. No text is sent over the network.
mod download;
mod inference;

pub use download::{download_model, model_available, model_dir};
pub use inference::LocalEmbedder;

pub const MODEL_NAME: &str = "multilingual-e5-small (int8)";
pub const MODEL_ID: &str = "Xenova/multilingual-e5-small@761b726dd34fb83930e26aab4e9ac3899aa1fa78:int8";
pub const DIMENSION: usize = 384;
/// Only the ONNX weights and self-contained tokenizer JSON are fetched.
pub const DOWNLOAD_BYTES: u64 = 118_054_593 + 17_082_730;
