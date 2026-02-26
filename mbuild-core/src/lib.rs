use serde_json::Value;

#[derive(Debug)]
pub enum BuilderError {
    NotImplemented(String),
}

pub trait Builder {
    fn get_type(&self) -> &'static str;
    fn verbs(&self) -> &'static [&'static str];
    fn run_build(&self, artifact: &str, recipe: &Value) -> Result<(), BuilderError>;
}
