use crate::execution::ExecutionError;
use mbuild_builder::BuilderRegistry;

pub(crate) fn create_builder_registry() -> Result<BuilderRegistry, ExecutionError> {
    let mut registry = BuilderRegistry::new();
    mbuild_builder::register_in_tree_builders(&mut registry)
        .map_err(ExecutionError::InvalidRequest)?;
    bobr_sandbox::register_builders(&mut registry).map_err(ExecutionError::InvalidRequest)?;
    Ok(registry)
}
