use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const MAX_TITLE_BYTES: usize = 160;
pub const MAX_DESC_BYTES: usize = 4096;
pub const MAX_PARENTS: usize = 8;
pub const MAX_PROPERTIES_BYTES: usize = 4096;
pub const MAX_QUERY_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Topic,
    Knowledge,
    Claim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteStatus {
    #[default]
    Active,
    Superseded,
    Retracted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub title: String,
    #[serde(alias = "body")]
    pub desc: String,
    pub kind: Kind,
    #[serde(default)]
    pub parents: Vec<u64>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotePatch {
    pub title: Option<String>,
    #[serde(alias = "body")]
    pub desc: Option<String>,
    pub parents: Option<Vec<u64>>,
    /// Merges keys into existing properties; null values remove the key.
    pub properties: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub title: String,
    pub desc: String,
    pub kind: Kind,
    #[serde(default)]
    pub status: NoteStatus,
    #[serde(default)]
    pub parents: Vec<u64>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<u64>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ValidationError {}

fn secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(sk-[a-z0-9]{20,}|ghp_[a-z0-9]{30,}|akia[0-9a-z]{16}|-----begin [a-z ]*private key-----|bearer\s+[a-z0-9._\-]{24,})")
            .expect("valid secret regex")
    })
}

pub fn reject_secrets(text: &str) -> Result<(), ValidationError> {
    if secret_regex().is_match(text) {
        return Err(ValidationError(
            "rejected: input appears to contain a credential or private key".into(),
        ));
    }
    Ok(())
}

pub fn validate_title(title: &str) -> Result<String, ValidationError> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(ValidationError("title must not be empty".into()));
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(ValidationError("title must be single-line".into()));
    }
    if trimmed.len() > MAX_TITLE_BYTES {
        return Err(ValidationError(format!("title exceeds {MAX_TITLE_BYTES} UTF-8 bytes")));
    }
    reject_secrets(trimmed)?;
    Ok(trimmed.to_string())
}

pub fn validate_desc(desc: &str) -> Result<String, ValidationError> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return Err(ValidationError("desc must not be empty".into()));
    }
    if trimmed.len() > MAX_DESC_BYTES {
        return Err(ValidationError(format!("desc exceeds {MAX_DESC_BYTES} UTF-8 bytes")));
    }
    reject_secrets(trimmed)?;
    Ok(trimmed.to_string())
}

pub fn validate_properties(props: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    let serialized = serde_json::to_string(props)
        .map_err(|e| ValidationError(format!("invalid properties JSON: {e}")))?;
    if serialized.len() > MAX_PROPERTIES_BYTES {
        return Err(ValidationError(format!(
            "properties JSON exceeds {MAX_PROPERTIES_BYTES} bytes"
        )));
    }
    reject_secrets(&serialized)?;
    Ok(())
}

pub fn validate_parents_shape(kind: Kind, parents: &[u64]) -> Result<Vec<u64>, ValidationError> {
    let mut dedup = Vec::with_capacity(parents.len());
    for &p in parents {
        if !dedup.contains(&p) {
            dedup.push(p);
        }
    }
    match kind {
        Kind::Topic => {
            if !dedup.is_empty() {
                return Err(ValidationError("topic notes must not have parents".into()));
            }
        }
        Kind::Knowledge | Kind::Claim => {
            if dedup.is_empty() || dedup.len() > MAX_PARENTS {
                return Err(ValidationError(format!(
                    "{:?} notes require 1..={MAX_PARENTS} parents",
                    kind
                )));
            }
        }
    }
    Ok(dedup)
}

impl Draft {
    pub fn validate(self) -> Result<Self, ValidationError> {
        let title = validate_title(&self.title)?;
        let desc = validate_desc(&self.desc)?;
        let parents = validate_parents_shape(self.kind, &self.parents)?;
        validate_properties(&self.properties)?;
        Ok(Self {
            title,
            desc,
            kind: self.kind,
            parents,
            properties: self.properties,
        })
    }
}

pub fn validate_query(query: &str) -> Result<String, ValidationError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(ValidationError("query must not be empty".into()));
    }
    if trimmed.len() > MAX_QUERY_BYTES {
        return Err(ValidationError(format!("query exceeds {MAX_QUERY_BYTES} bytes")));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_draft_with_desc_and_properties() {
        let mut props = BTreeMap::new();
        props.insert("confidence".into(), serde_json::json!(0.92));
        let d = Draft {
            title: "Unified Graph".into(),
            desc: "Single redb store without project scoping.".into(),
            kind: Kind::Topic,
            parents: vec![],
            properties: props,
        };
        assert!(d.validate().is_ok());
    }

    #[test]
    fn rejects_secret_in_properties() {
        let mut props = BTreeMap::new();
        props.insert("token".into(), serde_json::json!("sk-1234567890abcdefghijkl"));
        let d = Draft {
            title: "Bad Note".into(),
            desc: "Clean desc".into(),
            kind: Kind::Topic,
            parents: vec![],
            properties: props,
        };
        assert!(d.validate().is_err());
    }
}
