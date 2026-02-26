use mbuild_core::{Builder, BuilderError, VerbSpec};
use serde_json::Value;

const GITHUB_CUSTOM_VERBS: &[VerbSpec] = &[VerbSpec {
    name: "cache",
    description: "populate or update github mirrors without materializing outputs",
}];

pub struct GithubBuilder;
pub struct BinaryBuilder;

impl Builder for GithubBuilder {
    fn get_type(&self) -> &'static str {
        "github"
    }

    fn run_build(&self, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        Err(BuilderError::NotImplemented(format!(
            "github build dispatch is not implemented yet for artifact '{}'",
            artifact
        )))
    }

    fn summarize_recipe(&self, recipe: &Value) -> Result<Vec<(&'static str, String)>, BuilderError> {
        let obj = recipe.as_object().ok_or_else(|| {
            BuilderError::InvalidRecipe("github recipe must be an object".to_string())
        })?;
        let repo = obj
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| BuilderError::InvalidRecipe("github recipe missing 'repo'".to_string()))?;
        let commit = obj
            .get("commit")
            .and_then(Value::as_str)
            .ok_or_else(|| BuilderError::InvalidRecipe("github recipe missing 'commit'".to_string()))?;
        Ok(vec![("repo", repo.to_string()), ("commit", commit.to_string())])
    }

    fn custom_verbs(&self) -> &'static [VerbSpec] {
        GITHUB_CUSTOM_VERBS
    }

    fn run_custom(&self, verb: &str, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        match verb {
            "cache" => Err(BuilderError::NotImplemented(format!(
                "github cache dispatch is not implemented yet for artifact '{}'",
                artifact
            ))),
            _ => Err(BuilderError::UnsupportedVerb(format!(
                "verb '{}' is not supported by builder '{}'; artifact '{}'",
                verb,
                self.get_type(),
                artifact
            ))),
        }
    }
}

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
