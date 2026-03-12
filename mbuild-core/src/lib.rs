use serde_json::Value;

pub mod cas;
pub mod fsutil;

#[derive(Debug)]
pub enum BuilderError {
    UnsupportedVerb(String),
    InvalidRecipe(String),
    ExecutionFailed(String),
    NotImplemented(String),
}

#[derive(Clone, Copy, Debug)]
pub struct VerbSpec {
    pub name: &'static str,
    pub description: &'static str,
}

pub trait Builder {
    fn get_type(&self) -> &'static str;
    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError>;
    fn summarize_recipe(
        &self,
        _recipe: &Value,
    ) -> Result<Vec<(&'static str, String)>, BuilderError> {
        Ok(vec![])
    }

    fn custom_verbs(&self) -> &'static [VerbSpec] {
        &[]
    }

    fn run_custom(&self, verb: &str, artifact: &str, _recipe: &Value) -> Result<(), BuilderError> {
        Err(BuilderError::UnsupportedVerb(format!(
            "verb '{}' is not supported by builder '{}'; artifact '{}'",
            verb,
            self.get_type(),
            artifact
        )))
    }
}
