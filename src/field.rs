//! The [`Field`] of a record or operation parameter list.

use crate::location::Location;
use crate::wit_type::WitType;

/// A single field of a record or operation parameter list.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub(crate) name_kebab: String,
    pub(crate) name_snake: String,
    pub(crate) description: Option<String>,
    pub(crate) ty: WitType,
    pub(crate) location: Location,
}
