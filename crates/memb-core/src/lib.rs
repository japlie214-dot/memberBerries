//! Project-scoped data and input policy. No network or host transcript access.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope(String);

impl Scope {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if value.is_empty()
            || value.len() > 64
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err("project must be 1–64 lowercase ASCII letters, digits, hyphens or underscores and start with a letter or digit");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Topic,
    Knowledge,
    Claim,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub title: String,
    pub body: String,
    pub kind: Kind,
    pub parents: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub kind: Kind,
    pub parents: Vec<u64>,
}

static CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)-----BEGIN +(?:[A-Z0-9]+ +)*PRIVATE +KEY-----|\b(?:AKIA|ASIA)[A-Z0-9]{16}\b|\b(?:gh[pousr]_|github_pat_|sk-(?:proj-|ant-)?|xox[baprs]-)[A-Z0-9_\-]{12,}|\b(?:bearer|api[_ -]?key|password|passwd|secret|access[_ -]?token)\s*[:= ]\s*\S{8,}").expect("constant credential detector")
});

impl Draft {
    pub fn into_note(mut self, id: u64) -> Result<Note, &'static str> {
        self.title = self.title.trim().to_owned();
        self.body = self.body.trim().to_owned();
        if self.title.is_empty()
            || self.title.len() > 160
            || self.title.chars().any(char::is_control)
        {
            return Err("title must contain 1–160 bytes of single-line text");
        }
        if self.body.is_empty()
            || self.body.len() > 4096
            || self
                .body
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err("body must contain 1–4096 bytes of text");
        }
        if CREDENTIAL.is_match(&self.title) || CREDENTIAL.is_match(&self.body) {
            return Err("possible credential detected; remove it before saving");
        }
        self.parents.sort_unstable();
        if self.parents.len() > 8 || self.parents.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("at most eight distinct parent IDs are allowed");
        }
        if (self.kind == Kind::Topic) != self.parents.is_empty() {
            return Err("topics have no parents; knowledge and claims require parents");
        }
        Ok(Note {
            id,
            title: self.title,
            body: self.body,
            kind: self.kind,
            parents: self.parents,
        })
    }
}

pub fn words(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_cannot_be_a_path() {
        assert!(Scope::parse("project-2").is_ok());
        for bad in ["", "..", "foo/bar", "Some", "-foo", "foo "] {
            assert!(Scope::parse(bad).is_err());
        }
    }

    #[test]
    fn credential_is_refused_before_note_construction() {
        let draft = Draft {
            title: "Example".into(),
            body: "api_key=synthetic-placeholder-123".into(),
            kind: Kind::Topic,
            parents: vec![],
        };
        assert!(draft.into_note(1).is_err());
    }
}
