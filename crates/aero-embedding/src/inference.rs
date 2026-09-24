use crate::{
    DIMENSION,
    download::{open_local, verify_local},
};
use ort::{
    ep,
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use std::{io::Read, path::Path, sync::OnceLock};
use tokenizers::{Encoding, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

const MAX_BATCH: usize = 4;
const MAX_TOKENS: usize = 256;
// Token truncation alone does not bound tokenizer work on a multi-megabyte note.
// The worker should chunk large notes; rejecting is preferable to hiding data loss.
const MAX_TEXT_BYTES: usize = 32_768;
static RUNTIME: OnceLock<Result<(), String>> = OnceLock::new();

/// One CPU session, owned by one background worker. Drop releases its model and
/// tokenizer; the process-global ORT environment is not promised to be unloaded.
/// Never invoke load/embed on the UI thread. This type has no network operations.
pub struct LocalEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}
impl LocalEmbedder {
    /// Only opens the versioned local installation. Missing/corrupt files are
    /// errors, never an implicit download. SHA-256 is checked once per load.
    pub fn load(root: &Path) -> Result<Self, String> {
        let dir = verify_local(root)?;
        RUNTIME
            .get_or_init(|| {
                // Fail closed if another component already configured an unknown
                // environment: we cannot otherwise guarantee telemetry is disabled.
                if ort::init().with_name("aero-local-embedding").with_telemetry(false).commit() {
                    Ok(())
                } else {
                    Err("ORT environment was already configured outside aero-embedding".into())
                }
            })
            .clone()?;
        let session = Session::builder()
            .map_err(|e| e.to_string())?
            .with_no_environment_execution_providers()
            .map_err(|e| e.to_string())?
            .with_execution_providers([ep::CPU::default().with_arena_allocator(false).build()])
            .map_err(|e| e.to_string())?
            .with_independent_thread_pool()
            .map_err(|e| e.to_string())?
            .with_intra_threads(1)
            .map_err(|e| e.to_string())?
            .with_inter_threads(1)
            .map_err(|e| e.to_string())?
            .with_parallel_execution(false)
            .map_err(|e| e.to_string())?
            .with_intra_op_spinning(false)
            .map_err(|e| e.to_string())?
            .with_inter_op_spinning(false)
            .map_err(|e| e.to_string())?
            .with_memory_pattern(false)
            .map_err(|e| e.to_string())?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| e.to_string())?
            .with_log_level(ort::logging::LogLevel::Error)
            .map_err(|e| e.to_string())?
            .commit_from_file(dir.join("model_int8.onnx"))
            .map_err(|e| format!("load local ONNX: {e}"))?;
        // Load the session first: ONNX's temporary graph allocations should not
        // overlap the large multilingual vocabulary's live allocations. This
        // order reduces measured peak RSS by about 150 MB for the pinned model.
        tokenizers::utils::parallelism::set_parallelism(false);
        let mut bytes = Vec::new();
        open_local(&dir.join("tokenizer.json"))?
            .take(17_082_731)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let mut tokenizer = Tokenizer::from_bytes(&bytes).map_err(|_| "invalid local tokenizer JSON")?;
        drop(bytes);
        configure_tokenizer(&mut tokenizer)?;
        Ok(Self { session, tokenizer })
    }

    /// At most four texts, each at most 32 KiB before tokenization, truncated to
    /// 256 tokens INCLUDING special tokens. Returns finite, unit-length 384-vectors.
    /// Empty input returns empty output; oversized batches/texts return an error.
    pub fn embed(&mut self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>, String> {
        let encodings = tokenize(&self.tokenizer, texts, query)?;
        if encodings.is_empty() {
            return Ok(Vec::new());
        }
        let batch = encodings.len();
        let tokens = encodings[0].len();
        let ids: Vec<i64> =
            encodings.iter().flat_map(|e| e.get_ids().iter().map(|&n| i64::from(n))).collect();
        let mask: Vec<i64> =
            encodings.iter().flat_map(|e| e.get_attention_mask().iter().map(|&n| i64::from(n))).collect();
        let types: Vec<i64> =
            encodings.iter().flat_map(|e| e.get_type_ids().iter().map(|&n| i64::from(n))).collect();
        let ids = Tensor::from_array(([batch, tokens], ids)).map_err(|e| e.to_string())?;
        let attention = Tensor::from_array(([batch, tokens], mask.clone())).map_err(|e| e.to_string())?;
        let types = Tensor::from_array(([batch, tokens], types)).map_err(|e| e.to_string())?;
        let output = self
            .session
            .run(ort::inputs! {
                "input_ids" => ids, "attention_mask" => attention, "token_type_ids" => types,
            })
            .map_err(|e| format!("local inference failed: {e}"))?;
        let hidden = output.get("last_hidden_state").ok_or("model has no last_hidden_state")?;
        let (shape, values) = hidden.try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        if shape.as_ref() != [batch as i64, tokens as i64, DIMENSION as i64] {
            return Err("unexpected embedding tensor dimensions".into());
        }
        pool(values, &mask, batch, tokens)
    }
}
fn prefixed(text: &str, query: bool) -> String {
    format!("{}{}", if query { "query: " } else { "passage: " }, text)
}
fn configure_tokenizer(tokenizer: &mut Tokenizer) -> Result<(), String> {
    // Safe atomic library override, not unsafe process environment mutation.
    tokenizers::utils::parallelism::set_parallelism(false);
    let pad_id = tokenizer.token_to_id("<pad>").ok_or("tokenizer lacks <pad>")?;
    tokenizer
        .with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, stride: 0, ..Default::default() }))
        .map_err(|_| "cannot configure token truncation")?;
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        pad_id,
        pad_token: "<pad>".into(),
        ..Default::default()
    }));
    Ok(())
}
fn tokenize(tokenizer: &Tokenizer, texts: &[String], query: bool) -> Result<Vec<Encoding>, String> {
    if texts.len() > MAX_BATCH {
        return Err("embedding batch exceeds 4 texts".into());
    }
    if texts.iter().any(|s| s.len() > MAX_TEXT_BYTES) {
        return Err("embedding text exceeds 32768 bytes; chunk it before embedding".into());
    }
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    tokenizers::utils::parallelism::set_parallelism(false);
    let encoded = tokenizer
        .encode_batch(texts.iter().map(|t| prefixed(t, query)).collect(), true)
        .map_err(|_| "local tokenization failed")?;
    let tokens = encoded[0].len();
    if tokens == 0 || tokens > MAX_TOKENS || encoded.iter().any(|e| e.len() != tokens) {
        return Err("invalid bounded tokenization shape".into());
    }
    Ok(encoded)
}
fn pool(hidden: &[f32], mask: &[i64], batch: usize, tokens: usize) -> Result<Vec<Vec<f32>>, String> {
    if batch == 0
        || batch > MAX_BATCH
        || tokens == 0
        || tokens > MAX_TOKENS
        || hidden.len() != batch * tokens * DIMENSION
        || mask.len() != batch * tokens
        || mask.iter().any(|&m| m != 0 && m != 1)
    {
        return Err("invalid pooling shape or attention mask".into());
    }
    let mut result = Vec::with_capacity(batch);
    for b in 0..batch {
        // f64 accumulation prevents overflow even for very large finite f32 values.
        let mut mean = [0.0_f64; DIMENSION];
        let mut count = 0;
        for t in 0..tokens {
            if mask[b * tokens + t] == 0 {
                continue;
            }
            count += 1;
            let row = &hidden[(b * tokens + t) * DIMENSION..][..DIMENSION];
            for (sum, &value) in mean.iter_mut().zip(row) {
                if !value.is_finite() {
                    return Err("non-finite model output".into());
                }
                *sum += f64::from(value);
            }
        }
        if count == 0 {
            return Err("empty attention mask".into());
        }
        for value in &mut mean {
            *value /= f64::from(count);
        }
        let norm = mean.iter().map(|x| x * x).sum::<f64>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return Err("invalid embedding norm".into());
        }
        let vector: Vec<f32> = mean.iter().map(|x| (x / norm) as f32).collect();
        if vector.len() != DIMENSION || vector.iter().any(|x| !x.is_finite()) {
            return Err("invalid embedding vector".into());
        }
        result.push(vector);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DIMENSION;
    use tokenizers::{models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace};

    #[test]
    fn prefix_is_explicit_and_does_not_treat_user_text_as_instructions() {
        assert_eq!(prefixed("你好", true), "query: 你好");
        assert_eq!(prefixed("query: 笔记", false), "passage: query: 笔记");
    }
    #[test]
    fn missing_model_load_is_read_only() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("not-created");
        assert!(LocalEmbedder::load(&missing).is_err());
        assert!(!missing.exists());
    }
    fn tokenizer() -> Tokenizer {
        let model = WordLevel::builder()
            .vocab([("[UNK]".into(), 0), ("<pad>".into(), 1), ("hello".into(), 2)].into_iter().collect())
            .unk_token("[UNK]".into())
            .build()
            .unwrap();
        let mut t = Tokenizer::new(model);
        t.with_pre_tokenizer(Some(Whitespace));
        configure_tokenizer(&mut t).unwrap();
        t
    }
    #[test]
    fn tokens_are_truncated_padded_and_never_parallel() {
        let t = tokenizer();
        let encoded = tokenize(&t, &["hello ".repeat(600), "hello".into()], true).unwrap();
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0].len(), 256);
        assert_eq!(encoded[1].len(), 256);
        assert_eq!(*encoded[1].get_attention_mask().last().unwrap(), 0);
        assert_eq!(*encoded[1].get_ids().last().unwrap(), 1);
        assert!(!tokenizers::utils::parallelism::get_parallelism());
        assert!(!tokenizers::utils::parallelism::has_parallelism_been_used());
    }
    #[test]
    fn rejects_oversized_batch_and_text_before_tokenization() {
        let t = tokenizer();
        assert!(tokenize(&t, &vec!["x".into(); 5], true).is_err());
        assert!(tokenize(&t, &["x".repeat(32_769)], true).is_err());
        assert!(tokenize(&t, &[], false).unwrap().is_empty());
    }
    #[test]
    fn masked_mean_and_l2_ignore_padding() {
        let mut hidden = vec![0.0; 3 * DIMENSION];
        hidden[0] = 6.0;
        hidden[DIMENSION + 1] = 8.0;
        hidden[2 * DIMENSION] = f32::NAN; // masked values must not leak
        let v = pool(&hidden, &[1, 1, 0], 1, 3).unwrap();
        assert_eq!(v[0].len(), DIMENSION);
        assert!((v[0][0] - 0.6).abs() < 1e-6);
        assert!((v[0][1] - 0.8).abs() < 1e-6);
    }
    #[test]
    fn pooling_rejects_invalid_shape_mask_zero_and_nonfinite() {
        assert!(pool(&[1.0], &[1], 1, 1).is_err());
        assert!(pool(&vec![1.0; DIMENSION], &[0], 1, 1).is_err());
        assert!(pool(&vec![0.0; DIMENSION], &[1], 1, 1).is_err());
        assert!(pool(&vec![f32::INFINITY; DIMENSION], &[1], 1, 1).is_err());
        assert!(pool(&vec![1.0; DIMENSION], &[2], 1, 1).is_err());
    }
    #[test]
    fn normalization_avoids_f32_overflow_and_keeps_batch_rows_separate() {
        let mut h = vec![f32::MAX; DIMENSION * 2];
        h[DIMENSION..].fill(-f32::MAX);
        let v = pool(&h, &[1, 1], 2, 1).unwrap();
        for row in &v {
            assert!((row.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
        }
        assert!(v[0][0] > 0.0 && v[1][0] < 0.0);
    }
}
