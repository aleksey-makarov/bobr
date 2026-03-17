use crate::builders;
use mbuild_core::{
    BuildContext, BuildRequest, Builder, BuilderError, BuildKey, CasError, InputArity,
    PublishedBuild, ResolvedInputValue, ResolvedInputs, ResolvedObject, StoreLayout,
    compute_build_key, load_build_record, materialize_build, object_path, publish_refs,
};
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub enum RuntimeError {
    InvalidRequest(String),
    UnknownBuilder(String),
    RecipeLoad(String),
    Build(String),
    Store(String),
}

impl RuntimeError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid-request",
            Self::UnknownBuilder(_) => "unknown-builder",
            Self::RecipeLoad(_) => "recipe-load",
            Self::Build(_) => "build",
            Self::Store(_) => "store",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::UnknownBuilder(message)
            | Self::RecipeLoad(message)
            | Self::Build(message)
            | Self::Store(message) => message,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RuntimeError {}

pub fn run_request_in_workspace(
    workspace_root: &Path,
    request: &BuildRequest,
) -> Result<PublishedBuild, RuntimeError> {
    let layout = StoreLayout::discover(&workspace_root.join(".mbuild")).map_err(map_store_error)?;
    let published = interpret_build(workspace_root, &layout, &request.build)?;
    publish_refs(&layout, &request.meta.name, &published).map_err(map_store_error)?;
    Ok(published)
}

pub fn run_workspace_build(
    workspace_root: &Path,
    request_path: &Path,
) -> Result<PublishedBuild, RuntimeError> {
    let request = load_build_request(request_path)?;
    run_request_in_workspace(workspace_root, &request)
}

pub fn load_build_request(request_path: &Path) -> Result<BuildRequest, RuntimeError> {
    if !request_path.exists() {
        return Err(RuntimeError::RecipeLoad(format!(
            "request file '{}' does not exist",
            request_path.display()
        )));
    }

    let bytes = fs::read(request_path).map_err(|error| {
        RuntimeError::RecipeLoad(format!(
            "failed to read request file '{}': {error}",
            request_path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeError::InvalidRequest(format!(
            "failed to parse BuildRequest JSON from '{}': {error}",
            request_path.display()
        ))
    })
}

fn interpret_build(
    workspace_root: &Path,
    layout: &StoreLayout,
    build: &Value,
) -> Result<PublishedBuild, RuntimeError> {
    let (builder, payload) = resolve_builder_and_payload(build)?;
    let (config, inputs, input_hashes) = resolve_inputs(workspace_root, layout, builder, payload)?;
    let build_key =
        compute_build_key(builder.spec().tag, &config, &input_hashes).map_err(map_store_error)?;

    if let Some(record) = load_build_record(layout, build_key).map_err(map_store_error)? {
        let object_path = object_path(layout, record.object_hash);
        if !object_path.exists() {
            return Err(RuntimeError::Store(format!(
                "build '{}' points to missing object '{}'",
                build_key,
                object_path.display()
            )));
        }
        return Ok(PublishedBuild { record, object_path });
    }

    let mut context = build_context(workspace_root, layout, builder.spec().tag, build_key);
    let staged = builder
        .build_erased(config, inputs, &mut context)
        .map_err(map_builder_error)?;
    materialize_build(layout, build_key, staged).map_err(map_store_error)
}

fn resolve_builder_and_payload<'a>(
    build: &'a Value,
) -> Result<(&'static dyn Builder, &'a Map<String, Value>), RuntimeError> {
    let term = build.as_object().ok_or_else(|| {
        RuntimeError::InvalidRequest("build term must be a JSON object".to_string())
    })?;
    if term.len() != 1 {
        return Err(RuntimeError::InvalidRequest(
            "build term must contain exactly one builder tag".to_string(),
        ));
    }

    let (tag, payload) = term.iter().next().unwrap();
    let builder = builders::get_builder(tag).ok_or_else(|| {
        RuntimeError::UnknownBuilder(format!(
            "unknown builder tag '{}'; supported builders: {}",
            tag,
            builders::supported_builder_tags().join(", ")
        ))
    })?;
    let payload = payload.as_object().ok_or_else(|| {
        RuntimeError::InvalidRequest(format!("builder payload for '{}' must be a JSON object", tag))
    })?;
    Ok((builder, payload))
}

fn resolve_inputs(
    workspace_root: &Path,
    layout: &StoreLayout,
    builder: &'static dyn Builder,
    payload: &Map<String, Value>,
) -> Result<(Value, ResolvedInputs, Vec<BuildKey>), RuntimeError> {
    let mut config = payload.clone();
    let mut resolved = ResolvedInputs::empty();
    let mut ordered_keys = Vec::new();

    for slot in builder.spec().inputs {
        match slot.arity {
            InputArity::One => {
                let term = config.remove(slot.name).ok_or_else(|| {
                    RuntimeError::InvalidRequest(format!(
                        "required input slot '{}' is missing for builder '{}'",
                        slot.name,
                        builder.spec().tag
                    ))
                })?;
                let published = interpret_build(workspace_root, layout, &term)?;
                validate_allowed_kind(builder, slot.name, slot.allowed_kinds, &published.record.kind)?;
                ordered_keys.push(published.record.build_key);
                resolved.insert(slot.name, ResolvedInputValue::One(to_resolved_object(published)));
            }
            InputArity::Optional => {
                if let Some(term) = config.remove(slot.name) {
                    let published = interpret_build(workspace_root, layout, &term)?;
                    validate_allowed_kind(
                        builder,
                        slot.name,
                        slot.allowed_kinds,
                        &published.record.kind,
                    )?;
                    ordered_keys.push(published.record.build_key);
                    resolved.insert(
                        slot.name,
                        ResolvedInputValue::Optional(Some(to_resolved_object(published))),
                    );
                } else {
                    resolved.insert(slot.name, ResolvedInputValue::Optional(None));
                }
            }
            InputArity::Many => {
                let term = config.remove(slot.name).ok_or_else(|| {
                    RuntimeError::InvalidRequest(format!(
                        "repeated input slot '{}' is missing for builder '{}'",
                        slot.name,
                        builder.spec().tag
                    ))
                })?;
                let values = term.as_array().ok_or_else(|| {
                    RuntimeError::InvalidRequest(format!(
                        "input slot '{}' for builder '{}' must be an array",
                        slot.name,
                        builder.spec().tag
                    ))
                })?;
                let mut resolved_many = Vec::with_capacity(values.len());
                for value in values {
                    let published = interpret_build(workspace_root, layout, value)?;
                    validate_allowed_kind(
                        builder,
                        slot.name,
                        slot.allowed_kinds,
                        &published.record.kind,
                    )?;
                    ordered_keys.push(published.record.build_key);
                    resolved_many.push(to_resolved_object(published));
                }
                resolved.insert(slot.name, ResolvedInputValue::Many(resolved_many));
            }
        }
    }

    Ok((Value::Object(config), resolved, ordered_keys))
}

fn validate_allowed_kind(
    builder: &dyn Builder,
    slot_name: &str,
    allowed_kinds: &[&str],
    actual_kind: &str,
) -> Result<(), RuntimeError> {
    if allowed_kinds.is_empty() || allowed_kinds.iter().any(|kind| *kind == actual_kind) {
        return Ok(());
    }
    Err(RuntimeError::InvalidRequest(format!(
        "builder '{}' input slot '{}' rejects kind '{}'; allowed kinds: {}",
        builder.spec().tag,
        slot_name,
        actual_kind,
        allowed_kinds.join(", ")
    )))
}

fn to_resolved_object(published: PublishedBuild) -> ResolvedObject {
    ResolvedObject {
        object_hash: published.record.object_hash,
        build_key: published.record.build_key,
        kind: published.record.kind,
        attrs: published.record.attrs,
        object_path: published.object_path,
    }
}

fn build_context(
    workspace_root: &Path,
    layout: &StoreLayout,
    builder_tag: &str,
    build_key: BuildKey,
) -> BuildContext {
    let builder_root = layout
        .root
        .join("builder-state")
        .join(builder_tag.to_ascii_lowercase());
    BuildContext {
        workspace_root: workspace_root.to_path_buf(),
        builder_root: builder_root.clone(),
        temp_root: builder_root.join("tmp").join(build_key.to_hex()),
    }
}

fn map_builder_error(error: BuilderError) -> RuntimeError {
    RuntimeError::Build(error.to_string())
}

fn map_store_error(error: CasError) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_build_request_parses_modern_shape() {
        let temp = tempfile::tempdir().unwrap();
        let request_path = temp.path().join("request.json");
        fs::write(
            &request_path,
            "{\n  \"meta\": { \"name\": \"script\" },\n  \"build\": {\n    \"Text\": {\n      \"kind\": \"build-script\",\n      \"source\": \"#!/bin/sh\\necho hi\\n\"\n    }\n  }\n}",
        )
        .unwrap();

        let request = load_build_request(&request_path).unwrap();

        assert_eq!(request.meta.name, "script");
        assert_eq!(
            request.build,
            serde_json::json!({
                "Text": {
                    "kind": "build-script",
                    "source": "#!/bin/sh\necho hi\n",
                }
            })
        );
    }

    #[test]
    fn load_build_request_rejects_invalid_shape() {
        let temp = tempfile::tempdir().unwrap();
        let request_path = temp.path().join("request.json");
        fs::write(&request_path, r#"{ "meta": {} }"#).unwrap();

        let error = load_build_request(&request_path).unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidRequest(_)));
    }
}
