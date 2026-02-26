use mbuild_core::{Builder, BuilderError};
use serde_json::Value;

const BUILD_VERBS: &[&str] = &["build"];

pub struct GithubBuilder;
pub struct BinaryBuilder;

impl Builder for GithubBuilder {
    fn get_type(&self) -> &'static str {
        "github"
    }

    fn verbs(&self) -> &'static [&'static str] {
        BUILD_VERBS
    }

    fn run_build(&self, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        Err(BuilderError::NotImplemented(format!(
            "github build dispatch is not implemented yet for artifact '{}'",
            artifact
        )))
    }
}

impl Builder for BinaryBuilder {
    fn get_type(&self) -> &'static str {
        "binary"
    }

    fn verbs(&self) -> &'static [&'static str] {
        BUILD_VERBS
    }

    fn run_build(&self, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        Err(BuilderError::NotImplemented(format!(
            "binary build dispatch is not implemented yet for artifact '{}'",
            artifact
        )))
    }
}

static GITHUB_BUILDER: GithubBuilder = GithubBuilder;
static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 2] {
    [&GITHUB_BUILDER, &BINARY_BUILDER]
}

pub fn supported_verbs_for_type(recipe_type: &str) -> Option<Vec<&'static str>> {
    registered_builders()
        .iter()
        .find(|builder| builder.get_type() == recipe_type)
        .map(|builder| builder.verbs().to_vec())
}
