use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct OriginSpec {
    pub tag: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct OriginContext<'a> {
    pub temp_root: &'a Path,
}

pub trait ParsedOrigin: fmt::Debug + Send + Sync {
    fn spec(&self) -> &'static OriginSpec;

    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String>;

    fn clone_box(&self) -> Box<dyn ParsedOrigin>;
}

impl Clone for Box<dyn ParsedOrigin> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

pub trait OriginHandler: Send + Sync {
    fn spec(&self) -> &'static OriginSpec;

    fn parse(
        &self,
        object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct DummyOrigin;

    impl ParsedOrigin for DummyOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec { tag: "dummy" };
            &SPEC
        }

        fn materialize(&self, _cx: &OriginContext<'_>) -> Result<PathBuf, String> {
            Ok(PathBuf::from("/tmp/dummy"))
        }

        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn boxed_parsed_origin_is_cloneable() {
        let origin: Box<dyn ParsedOrigin> = Box::new(DummyOrigin);
        let cloned = origin.clone();
        assert_eq!(origin.spec().tag, cloned.spec().tag);
    }
}
