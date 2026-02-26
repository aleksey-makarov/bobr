pub trait Builder {
    fn get_type(&self) -> &'static str;
    fn verbs(&self) -> &'static [&'static str];
}
