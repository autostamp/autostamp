//! The [`OperationModel`] intermediate representation of an API operation.

use crate::field::Field;
use crate::security::AuthApply;
use crate::wit_type::WitType;

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
    /// The WIT type of the success (2xx) response body — the `ok` arm of the returned
    /// `result`. Defaults to [`WitType::String`] (the raw response body) when the operation
    /// declares no typed success schema.
    pub(crate) ok_ty: WitType,
    /// The WIT type of the `err` arm of the returned `result`: a [`WitType::Named`] variant
    /// enumerating the operation's declared error responses, or [`WitType::String`] (a raw
    /// error message) when the operation declares no specific error responses.
    pub(crate) err_ty: WitType,
    /// The effective security schemes to apply to this operation, resolved from the
    /// document- and operation-level `security` requirements.
    pub(crate) auth: Vec<AuthApply>,
}
