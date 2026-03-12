mod error;
mod hash;
mod node;
mod normalize;
mod object_hash;
mod path_source;
mod tar_source;

pub use error::{EntryKind, Error, InvalidPathReason, TarEntryKind};
pub use object_hash::ObjectHash;

use std::fs::File;
use std::io::Read;
use std::path::Path;

pub fn hash_path(path: impl AsRef<Path>) -> Result<ObjectHash, Error> {
    let node = path_source::load_path(path.as_ref())?;
    Ok(hash::hash_node(&node))
}

pub fn hash_tar_reader<R: Read>(reader: R) -> Result<ObjectHash, Error> {
    let node = tar_source::load_tar_reader(reader)?;
    Ok(hash::hash_node(&node))
}

pub fn hash_tar_file(path: impl AsRef<Path>) -> Result<ObjectHash, Error> {
    let file = File::open(path.as_ref()).map_err(Error::Io)?;
    hash_tar_reader(file)
}
