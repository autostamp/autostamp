//! The [`EnumModel`] intermediate representation of a WIT enum.

/// A WIT enum: a set of named cases.
#[derive(Debug, Clone)]
pub(crate) struct EnumModel {
    pub(crate) name_kebab: String,
    /// (kebab-case for WIT, original-case for the API wire)
    pub(crate) cases: Vec<(String, String)>,
}
