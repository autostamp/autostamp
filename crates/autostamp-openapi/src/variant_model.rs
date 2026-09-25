//! The [`VariantModel`] intermediate representation of a WIT variant.
//!
//! Variants model an operation's enumerated *error* responses: one case per declared error
//! status (or status range), plus a catch-all `other` case for undeclared statuses and
//! transport-level failures. Each case carries the raw response body (or error message) as a
//! `string` payload.

use crate::wit_type::WitType;

/// A WIT variant: a named tagged union with one or more cases.
#[derive(Debug, Clone)]
pub(crate) struct VariantModel {
    pub(crate) name_kebab: String,
    pub(crate) cases: Vec<VariantCase>,
}

/// Which HTTP status(es) a variant case maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseStatus {
    /// A specific status code, e.g. `404`.
    Code(u16),
    /// A whole status class named by its leading digit, e.g. `Range(4)` for `4XX`.
    Range(u8),
    /// The catch-all case: undeclared statuses and transport-level failures.
    Other,
}

/// A single case of a [`VariantModel`].
#[derive(Debug, Clone)]
pub(crate) struct VariantCase {
    pub(crate) name_kebab: String,
    /// The payload type carried by the case, or `None` for a payload-less case.
    pub(crate) payload: Option<WitType>,
    /// The HTTP status(es) this case is constructed from at runtime.
    pub(crate) status: CaseStatus,
}
