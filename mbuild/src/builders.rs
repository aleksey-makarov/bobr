use mbuild_core::{Builder, BuilderError};
use mbuild_github::GithubBuilder;
use serde_json::Value;

pub struct BinaryBuilder;

impl Builder for BinaryBuilder {
    fn get_type(&self) -> &'static str {
        "binary"
    }

    fn run_build(&self, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        Err(BuilderError::NotImplemented(format!(
            "binary build dispatch is not implemented yet for artifact '{}'",
            artifact
        )))
    }

    fn summarize_recipe(&self, recipe: &Value) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let obj = recipe.as_object().ok_or_else(|| {
            BuilderError::InvalidRecipe("binary recipe must be an object".to_string())
        })?;
        let script = obj
            .get("script")
            .and_then(Value::as_str)
            .ok_or_else(|| BuilderError::InvalidRecipe("binary recipe missing 'script'".to_string()))?;
        Ok(vec![("script_bytes", script.len().to_string())])
    }
}

static GITHUB_BUILDER: GithubBuilder = GithubBuilder;
static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 2] {
    [&GITHUB_BUILDER, &BINARY_BUILDER]
}

pub fn get_builder(recipe_type: &str) -> Option<&'static dyn Builder> {
    registered_builders()
        .iter()
        .find(|builder| builder.get_type() == recipe_type)
        .copied()
}

pub fn supported_verbs_for_type(recipe_type: &str) -> Option<Vec<&'static str>> {
    get_builder(recipe_type).map(|builder| {
        let mut verbs = Vec::with_capacity(1 + builder.custom_verbs().len());
        verbs.push("build");
        verbs.extend(builder.custom_verbs().iter().map(|v| v.name));
        verbs
    })
}
