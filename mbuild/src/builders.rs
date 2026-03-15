use mbuild_core::Builder;
use mbuild_fetch::FetchBuilder;
use mbuild_text::TextBuilder;

static FETCH_BUILDER: FetchBuilder = FetchBuilder;
static TEXT_BUILDER: TextBuilder = TextBuilder;

pub fn registered_builders() -> [&'static dyn Builder; 2] {
    [&TEXT_BUILDER, &FETCH_BUILDER]
}

pub fn get_builder(tag: &str) -> Option<&'static dyn Builder> {
    registered_builders()
        .iter()
        .find(|builder| builder.spec().tag.eq_ignore_ascii_case(tag))
        .copied()
}

pub fn supported_builder_tags() -> Vec<&'static str> {
    registered_builders()
        .iter()
        .map(|builder| builder.spec().tag)
        .collect()
}
