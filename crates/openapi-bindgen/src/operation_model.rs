//! The [`OperationModel`] intermediate representation of an API operation.

use crate::field::Field;

/// A single API operation: an HTTP method + path plus its parameter fields.
#[derive(Debug, Clone)]
pub(crate) struct OperationModel {
    pub(crate) op_kebab: String,
    pub(crate) op_snake: String,
    pub(crate) method: String,
    pub(crate) path_template: String,
    pub(crate) summary: Option<String>,
    pub(crate) params_record: String, // kebab-case
    pub(crate) fields: Vec<Field>,
}
