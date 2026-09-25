//! The [`Field`] of a record or operation parameter list.

use crate::location::Location;
use crate::wit_type::WitType;

/// A single field of a record or operation parameter list.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    /// The WIT identifier (kebab-case) — the field name in the generated `.wit` record and,
    /// after wit-bindgen, the Rust struct field. Unique within its record.
    pub(crate) name_kebab: String,
    /// The internal key used to plumb this field's value from the guest's params JSON to the
    /// runtime dispatcher. An implementation detail, kept unique per record; **not** what goes
    /// on the wire.
    pub(crate) name_snake: String,
    /// The verbatim name the value is sent under in the HTTP request: the query/header parameter
    /// name or the JSON body property, exactly as the OpenAPI document spells it (`fieldSelector`,
    /// `PhoneNumber`, `DateCreated<`). Preserving this is what makes the generated request
    /// truthful — snake-casing it would send a name the API doesn't recognize.
    pub(crate) wire_name: String,
    pub(crate) description: Option<String>,
    pub(crate) ty: WitType,
    pub(crate) location: Location,
}
