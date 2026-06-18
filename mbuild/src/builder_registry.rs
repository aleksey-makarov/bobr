use crate::runtime::RuntimeError;
use mbuild_builder::BuilderRegistry;

pub(crate) fn create_builder_registry() -> Result<BuilderRegistry, RuntimeError> {
    let mut registry = BuilderRegistry::new();
    mbuild_builder::register_in_tree_builders(&mut registry)
        .map_err(RuntimeError::InvalidRequest)?;
    mbuild_sandbox::register_builders(&mut registry).map_err(RuntimeError::InvalidRequest)?;
    bobr_sandbox::register_builders(&mut registry).map_err(RuntimeError::InvalidRequest)?;
    Ok(registry)
}
