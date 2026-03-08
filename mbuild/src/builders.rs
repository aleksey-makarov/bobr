use mbuild_binary::BinaryBuilder;
use mbuild_core::Builder;
use mbuild_fetch::FetchBuilder;
use mbuild_github::GithubBuilder;
use mbuild_image::{ContainerImageBuilder, ImageBuilder};
use mbuild_text::TextBuilder;

static GITHUB_BUILDER: GithubBuilder = GithubBuilder;
static BINARY_BUILDER: BinaryBuilder = BinaryBuilder;
static TEXT_BUILDER: TextBuilder = TextBuilder;
static FETCH_BUILDER: FetchBuilder = FetchBuilder;
static IMAGE_BUILDER: ImageBuilder = ImageBuilder;
static CONTAINER_IMAGE_BUILDER: ContainerImageBuilder = ContainerImageBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 6] {
    [
        &GITHUB_BUILDER,
        &BINARY_BUILDER,
        &TEXT_BUILDER,
        &FETCH_BUILDER,
        &IMAGE_BUILDER,
        &CONTAINER_IMAGE_BUILDER,
    ]
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
