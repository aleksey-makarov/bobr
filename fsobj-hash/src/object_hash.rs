crate::define_hex_hash_type! {
    /// Stable hash of a normalized filesystem object.
    ///
    /// `ObjectHash` identifies the normalized object tree produced by this
    /// crate's filesystem object hashing rules. The hash covers object shape
    /// and metadata that are part of the normalized model: regular file
    /// content and executable bit, symlink target bytes, directory entry names,
    /// entry kinds, and child hashes.
    ///
    /// The textual representation is exactly 64 lowercase hexadecimal
    /// characters.
    pub struct ObjectHash;
}
