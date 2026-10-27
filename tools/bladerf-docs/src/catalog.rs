use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use url::Url;

const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

/// A schema- and consistency-checked catalog; construction is private to this module.
#[derive(Debug)]
pub(crate) struct Catalog {
    document: Value,
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("expected string field {key}"))
}

pub(crate) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("expected array field {key}"))
}

pub(crate) fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .with_context(|| format!("missing field {key}"))
}

fn read_json(path: &Path) -> Result<Value> {
    let input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::new();
    input
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "{} exceeds the 2 MiB document limit",
        path.display()
    );
    crate::json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn local_schema_references(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "$ref" || key == "$dynamicRef" {
                    ensure!(
                        child.as_str().is_some_and(|s| s.starts_with('#')),
                        "schema references must be internal fragments; external retrieval is disabled"
                    );
                }
                local_schema_references(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                local_schema_references(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn source_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("parse source URL {value}"))?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "source URLs must be absolute HTTPS URLs without credentials"
    );
    Ok(url)
}

fn unique_records<'a>(records: &'a [Value], kind: &str) -> Result<BTreeMap<&'a str, &'a Value>> {
    let mut result = BTreeMap::new();
    for record in records {
        let id = text(record, "id")?;
        ensure!(
            result.insert(id, record).is_none(),
            "duplicate {kind} id {id}"
        );
    }
    Ok(result)
}

fn check_source(source: &Value) -> Result<()> {
    let id = text(source, "id")?;
    let url = source_url(text(source, "url")?)?;
    ensure!(
        url.fragment().is_none(),
        "source {id} URL must not contain a locator fragment"
    );
    if text(source, "kind")? == "source_code" {
        let commit = text(source, "commit")?;
        ensure!(
            url.path().split('/').any(|part| part == commit),
            "source {id} URL must pin its declared commit"
        );
    }
    Ok(())
}

fn check_evidence(claim: &Value, sources: &BTreeMap<&str, &Value>) -> Result<()> {
    let id = text(claim, "id")?;
    let mut has_code = false;
    for evidence in array(claim, "evidence")? {
        let source_id = text(evidence, "source_id")?;
        let source = sources
            .get(source_id)
            .with_context(|| format!("claim {id} refers to missing source {source_id}"))?;
        has_code |= text(source, "kind")? == "source_code";
        let locator = field(evidence, "locator")?;
        match text(locator, "kind")? {
            "pdf_page" => {
                ensure!(
                    text(source, "kind")? == "pdf",
                    "claim {id} uses a PDF locator on non-PDF source {source_id}"
                );
                let page = field(locator, "page")?
                    .as_u64()
                    .context("expected PDF page integer")?;
                let count = field(source, "page_count")?
                    .as_u64()
                    .context("expected PDF page count")?;
                ensure!(
                    page <= count,
                    "claim {id} PDF page {page} exceeds source {source_id} page count {count}"
                );
            }
            "symbol" => {
                ensure!(
                    text(source, "kind")? == "source_code",
                    "claim {id} uses a symbol locator on non-code source {source_id}"
                );
            }
            "section" => {}
            other => bail!("claim {id} has unsupported locator kind {other}"),
        }
    }
    if text(claim, "evidence_kind")? == "pinned_implementation" {
        ensure!(
            has_code,
            "implementation claim {id} needs a pinned source-code reference"
        );
    }
    Ok(())
}

fn check_semantics(document: &Value) -> Result<()> {
    let sources = unique_records(array(document, "sources")?, "source")?;
    let claims = unique_records(array(document, "claims")?, "claim")?;
    let issues = unique_records(array(document, "issues")?, "issue")?;
    for source in sources.values() {
        check_source(source)?;
    }
    let mut issue_claims = BTreeSet::new();
    for (id, issue) in issues {
        let active_disagreement = text(issue, "status")? == "open"
            && text(issue, "kind")? == "source_disagreement"
            && array(issue, "source_ids")?.len() >= 2;
        for claim_id in array(issue, "claim_ids")? {
            let claim_id = claim_id
                .as_str()
                .context("expected issue claim id string")?;
            ensure!(
                claims.contains_key(claim_id),
                "issue {id} refers to missing claim {claim_id}"
            );
            if active_disagreement {
                issue_claims.insert(claim_id);
            }
        }
        for source_id in array(issue, "source_ids")? {
            let source_id = source_id
                .as_str()
                .context("expected issue source id string")?;
            ensure!(
                sources.contains_key(source_id),
                "issue {id} refers to missing source {source_id}"
            );
        }
    }
    for (id, claim) in claims {
        check_evidence(claim, &sources)?;
        let value = field(claim, "value")?;
        if text(value, "kind")? == "integer" {
            ensure!(
                field(value, "value")?.as_i64().is_some(),
                "claim {id} needs a representable signed 64-bit integer, not a floating-point JSON number"
            );
        }
        if text(value, "kind")? == "integer_range" {
            let min = field(value, "minimum")?
                .as_i64()
                .context("expected minimum integer")?;
            let max = field(value, "maximum")?
                .as_i64()
                .context("expected maximum integer")?;
            ensure!(min <= max, "claim {id} range minimum exceeds maximum");
        }
        if text(field(claim, "review")?, "status")? == "conflicted" {
            ensure!(
                issue_claims.contains(id),
                "conflicted claim {id} needs an open source-disagreement issue with at least two sources"
            );
        }
    }
    Ok(())
}

impl Catalog {
    pub(crate) fn load(schema_path: &Path, catalog_path: &Path) -> Result<Self> {
        Self::validate(&read_json(schema_path)?, read_json(catalog_path)?)
    }

    fn validate(schema: &Value, document: Value) -> Result<Self> {
        local_schema_references(schema)?;
        let validator = jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(schema)
            .context("compile catalog schema")?;
        let errors: Vec<_> = validator
            .iter_errors(&document)
            .take(20)
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect();
        ensure!(
            errors.is_empty(),
            "catalog schema validation failed:\n{}",
            errors.join("\n")
        );
        check_semantics(&document)?;
        Ok(Self { document })
    }

    pub(crate) fn claims(&self) -> Result<&[Value]> {
        array(&self.document, "claims")
    }

    pub(crate) fn sources(&self) -> Result<&[Value]> {
        array(&self.document, "sources")
    }

    pub(crate) fn issues(&self) -> Result<&[Value]> {
        array(&self.document, "issues")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Value, Value) {
        let schema =
            serde_json::from_str(include_str!("../../../docs/knowledge/catalog.schema.json"))
                .expect("committed schema");
        let document = serde_json::from_str(include_str!("../../../docs/knowledge/catalog.json"))
            .expect("committed catalog");
        (schema, document)
    }

    #[test]
    fn rejects_unknown_sources_and_duplicate_claim_ids() {
        let (schema, mut document) = fixture();
        document["claims"][0]["evidence"][0]["source_id"] = Value::from("missing-source");
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("missing source")
        );
        let (schema, mut document) = fixture();
        let duplicate = document["claims"][0].clone();
        document["claims"].as_array_mut().unwrap().push(duplicate);
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("duplicate claim")
        );
    }

    #[test]
    fn rejects_out_of_document_pages_and_reversed_ranges() {
        let (schema, mut document) = fixture();
        let source = document["sources"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|s| s["kind"] == "pdf")
            .unwrap();
        let source_id = source["id"].clone();
        source["page_count"] = Value::from(1);
        let claim = document["claims"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| {
                c["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["source_id"] == source_id && e["locator"]["kind"] == "pdf_page")
            })
            .unwrap();
        claim["evidence"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|e| e["source_id"] == source_id)
            .unwrap()["locator"]["page"] = Value::from(2);
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("exceeds source")
        );
        let (schema, mut document) = fixture();
        let claim = document["claims"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| c["value"]["kind"] == "integer_range")
            .unwrap();
        claim["value"]["minimum"] = Value::from(7);
        claim["value"]["maximum"] = Value::from(3);
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("minimum exceeds")
        );
    }

    #[test]
    fn rejects_unqualified_conflicts_and_external_schema_references() {
        let (schema, mut document) = fixture();
        document["issues"] = Value::Array(Vec::new());
        document["claims"][0]["review"]["status"] = Value::from("conflicted");
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("needs an open")
        );
        for schema in [
            serde_json::json!({"$ref": "https://example.invalid/schema"}),
            serde_json::json!({"$defs": {"nested": {"$ref": "file:///unavailable.schema.json"}}}),
            serde_json::json!({"allOf": [{"$dynamicRef": "https://example.invalid/schema"}]}),
        ] {
            assert!(local_schema_references(&schema).is_err());
        }
    }

    #[test]
    fn rejects_integral_float_in_integer_fact() {
        let (schema, mut document) = fixture();
        let claim = document["claims"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| c["value"]["kind"] == "integer")
            .unwrap();
        claim["value"]["value"] = serde_json::json!(1.0);
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("representable signed")
        );
    }

    #[test]
    fn rejects_mutable_source_urls_and_closed_conflict_issues() {
        let (schema, mut document) = fixture();
        let source = document["sources"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|s| s["kind"] == "source_code")
            .unwrap();
        source["url"] = Value::from("https://github.com/Nuand/bladeRF/blob/master/file.c");
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("must pin")
        );
        let (schema, mut document) = fixture();
        let claim_id = document["claims"][0]["id"].clone();
        document["claims"][0]["review"]["status"] = Value::from("conflicted");
        document["issues"][0]["claim_ids"] = Value::Array(vec![claim_id]);
        document["issues"][0]["status"] = Value::from("resolved");
        assert!(
            Catalog::validate(&schema, document)
                .unwrap_err()
                .to_string()
                .contains("needs an open")
        );
    }
}
