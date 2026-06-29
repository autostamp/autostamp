//! The [`Location`] of an operation parameter in an HTTP request.

/// Where a field's value belongs in the outgoing HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Location {
    Path,
    Query,
    Body,
}
