use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    tensor::TensorElementType,
    value::{DynTensor, Tensor, ValueType},
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokenizers::{utils::truncation::TruncationParams, Tokenizer};

pub const MODEL_ID: &str =
    "onnx-community/embeddinggemma-2-ONNX@daa72c51243991dfcaf9f9137d2c573d8f7790c0-q4";
pub const DOC_PREFIX: &str = "title: none | text: ";
pub const QUERY_PREFIX: &str = "task: search result | query: ";
const MAX_TOKENS: usize = 8192;
const QUERY_MAX_TOKENS: usize = 128;

fn tokens(values: &[u32], query: bool) -> Vec<i64> {
    let mut values: Vec<i64> = values.iter().map(|&v| i64::from(v)).collect();
    if query && values.len() > QUERY_MAX_TOKENS {
        let head = QUERY_MAX_TOKENS * 30 / 100;
        let tail = QUERY_MAX_TOKENS - head;
        values.drain(head..values.len() - tail);
    }
    values
}

const FILES: [&str; 3] = [
    "tokenizer.json",
    "onnx/model_q4.onnx",
    "onnx/model_q4.onnx_data",
];
// Increment when tokenization, pooling, normalization or output semantics change.
const CONTRACT: &str =
    "gemma-text-v1:document-title-newline-description:sentence_embedding:l2:query-head30-tail70";

type FileStamp = Vec<(u64, SystemTime)>;
fn file_stamp(directory: &Path) -> Result<FileStamp, String> {
    FILES
        .iter()
        .map(|file| {
            let meta = std::fs::metadata(directory.join(file))
                .map_err(|e| format!("model file {file}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("model file {file} is not a file"));
            }
            Ok((meta.len(), meta.modified().map_err(|e| e.to_string())?))
        })
        .collect()
}
fn model_identity(directory: &Path) -> Result<String, String> {
    let mut hash = Sha256::new();
    hash.update(CONTRACT.as_bytes());
    hash.update(DOC_PREFIX.as_bytes());
    hash.update(QUERY_PREFIX.as_bytes());
    hash.update((MAX_TOKENS as u64).to_le_bytes());
    hash.update((QUERY_MAX_TOKENS as u64).to_le_bytes());
    for file in FILES {
        hash.update(file.as_bytes());
        let mut reader = std::fs::File::open(directory.join(file)).map_err(|e| e.to_string())?;
        hash.update(
            reader
                .metadata()
                .map_err(|e| e.to_string())?
                .len()
                .to_le_bytes(),
        );
        let mut buffer = [0u8; 65536];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
    }
    Ok(format!("gemma-sha256-{:x}", hash.finalize()))
}
struct Model {
    tokenizer: Tokenizer,
    session: Session,
    identity: String,
    stamp: FileStamp,
}
/// Lazily loads a caller-specified local model; inference never uses the network.
pub struct GemmaEmbedder {
    directory: PathBuf,
    model: crate::idle::IdleSlot<Model>,
}
impl GemmaEmbedder {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            model: crate::idle::IdleSlot::new(),
        }
    }

    /// Drops the loaded model if it has been unused for `idle` (its memory is returned;
    /// the next embedding loads it again). True if it was dropped.
    pub fn unload_if_idle(&self, idle: std::time::Duration) -> bool {
        self.model.unload_if_idle(idle, std::time::Instant::now())
    }

    /// Drops the loaded model now. True if one was loaded.
    pub fn unload(&self) -> bool {
        self.model.unload()
    }

    pub fn is_loaded(&self) -> bool {
        self.model.is_loaded()
    }
    fn load(&self) -> Result<Model, String> {
        let stamp = file_stamp(&self.directory)?;
        let identity = model_identity(&self.directory)?;
        let mut tokenizer = Tokenizer::from_file(self.directory.join("tokenizer.json"))
            .map_err(|e| e.to_string())?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| e.to_string())?;
        let session = Session::builder()
            .map_err(|e| e.to_string())?
            .with_intra_threads(2)
            .map_err(|e| e.to_string())?
            .with_inter_threads(1)
            .map_err(|e| e.to_string())?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| e.to_string())?
            .commit_from_file(self.directory.join("onnx/model_q4.onnx"))
            .map_err(|e| format!("cannot load ONNX model: {e}"))?;
        if file_stamp(&self.directory)? != stamp {
            return Err("model files changed while loading".into());
        }
        Ok(Model {
            tokenizer,
            session,
            identity,
            stamp,
        })
    }
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>, String> {
        self.embed_tagged(text, true).map(|(_, v)| v)
    }
    pub fn embed_document(&self, text: &str) -> Result<Vec<f32>, String> {
        self.embed_tagged(text, false).map(|(_, v)| v)
    }
    pub fn embed_tagged(&self, text: &str, query: bool) -> Result<(String, Vec<f32>), String> {
        self.model.with(|| self.load(), |model| self.infer(model, text, query))
    }
    fn infer(&self, model: &mut Model, text: &str, query: bool) -> Result<(String, Vec<f32>), String> {
        if file_stamp(&self.directory)? != model.stamp {
            return Err("model files changed; restart to reload".into());
        }
        let Model {
            tokenizer,
            session,
            identity,
            ..
        } = model;
        let truncation = tokenizer.get_truncation().cloned();
        if query {
            tokenizer.with_truncation(None).map_err(|e| e.to_string())?;
        }
        let encoding = tokenizer.encode(
            format!("{}{text}", if query { QUERY_PREFIX } else { DOC_PREFIX }),
            true,
        );
        if query {
            tokenizer
                .with_truncation(truncation)
                .map_err(|e| e.to_string())?;
        }
        let encoding = encoding.map_err(|e| e.to_string())?;
        let ids = tokens(encoding.get_ids(), query);
        let len = ids.len();
        let mut inputs: Vec<(String, DynTensor)> = Vec::new();
        for input in session.inputs() {
            let ValueType::Tensor { ty, shape, .. } = input.dtype() else {
                return Err(format!("unsupported ONNX input {}", input.name()));
            };
            let tensor = if input.name().ends_with("_features") && *ty == TensorElementType::Float32
            {
                let dims: Vec<i64> = shape.iter().map(|&n| if n < 0 { 0 } else { n }).collect();
                Tensor::<f32>::new(session.allocator(), dims)
                    .map_err(|e| e.to_string())?
                    .upcast()
            } else {
                let values = match input.name() {
                    "input_ids" => ids.clone(),
                    "attention_mask" => tokens(encoding.get_attention_mask(), query),
                    "position_ids" => (0..len as i64).collect(),
                    "token_type_ids" => tokens(encoding.get_type_ids(), query),
                    "global_attention_mask" => vec![0; len],
                    name => return Err(format!("unsupported ONNX input {name}")),
                };
                match ty {
                    TensorElementType::Int64 => Tensor::from_array(([1usize, len], values))
                        .map_err(|e| e.to_string())?
                        .upcast(),
                    TensorElementType::Int32 => Tensor::from_array((
                        [1usize, len],
                        values.iter().map(|&v| v as i32).collect::<Vec<_>>(),
                    ))
                    .map_err(|e| e.to_string())?
                    .upcast(),
                    _ => return Err(format!("unsupported ONNX dtype for {}", input.name())),
                }
            };
            inputs.push((input.name().to_owned(), tensor));
        }
        let outputs = session
            .run(inputs)
            .map_err(|e| format!("ONNX inference failed: {e}"))?;
        let (shape, data) = outputs
            .get("sentence_embedding")
            .ok_or("missing sentence_embedding")?
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if shape.len() != 2 || shape[0] != 1 || data.is_empty() {
            return Err("invalid embedding shape".into());
        }
        let norm = data.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 || !norm.is_finite() || data.iter().any(|v| !v.is_finite()) {
            return Err("invalid embedding vector".into());
        }
        Ok((identity.clone(), data.iter().map(|v| v / norm).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_truncation_preserves_head_and_tail() {
        let values: Vec<u32> = (0..300).collect();
        let capped = tokens(&values, true);
        assert_eq!(capped.len(), 128);
        assert_eq!(capped[37], 37);
        assert_eq!(capped[38], 210);
        assert_eq!(tokens(&values, false).len(), 300);
    }
    #[test]
    fn identity_changes_for_each_model_file() {
        let dir = std::env::temp_dir().join(format!("memb-gemma-identity-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("onnx")).unwrap();
        for file in FILES {
            std::fs::write(dir.join(file), b"synthetic").unwrap();
        }
        let initial = model_identity(&dir).unwrap();
        assert_eq!(initial, model_identity(&dir).unwrap());
        for file in FILES {
            std::fs::write(dir.join(file), b"changed synthetic").unwrap();
            assert_ne!(initial, model_identity(&dir).unwrap());
            std::fs::write(dir.join(file), b"synthetic").unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_model_returns_error() {
        assert!(GemmaEmbedder::new(PathBuf::from("missing-synthetic-model"))
            .embed_query("synthetic")
            .is_err());
    }
    #[test]
    #[ignore = "requires MEMB_TEST_GEMMA_DIR with pinned local model"]
    fn real_model_inference() {
        let embedder = GemmaEmbedder::new(PathBuf::from(
            std::env::var_os("MEMB_TEST_GEMMA_DIR").unwrap(),
        ));
        let q = embedder.embed_query("bright red apple").unwrap();
        let d = embedder
            .embed_document("scarlet apples are fresh fruit")
            .unwrap();
        assert_eq!(q.len(), d.len());
        assert!(q.len() > 100);
        assert!((q.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-4);
        assert!(q.iter().zip(&d).map(|(a, b)| a * b).sum::<f32>() > 0.25);
    }
}
