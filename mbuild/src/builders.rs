use mbuild_binary::BinaryBuilder;
use mbuild_core::Builder;
use mbuild_github::GithubBuilder;

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
