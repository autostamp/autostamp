//! The [`RecordModel`] intermediate representation of a WIT record.

use crate::field::Field;

/// A WIT record: a named product type with one or more fields.
#[derive(Debug, Clone)]
pub(crate) struct RecordModel {
    pub(crate) name_kebab: String,
    pub(crate) description: Option<String>,
    pub(crate) fields: Vec<Field>,
}
