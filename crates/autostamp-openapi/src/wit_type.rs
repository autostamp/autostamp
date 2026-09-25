//! The [`WitType`] intermediate representation of a WIT type.

/// A WIT type in the intermediate model, before it is rendered to WIT source.
#[derive(Debug, Clone)]
pub(crate) enum WitType {
    String,
    Bool,
    S32,
    S64,
    F64,
    Option(Box<WitType>),
    List(Box<WitType>),
    /// A string-keyed open map (free-form `object`). WIT has no native map type, so it is
    /// rendered as `list<{entry}>` where `entry` is a named `record { key: string, value: V }`
    /// emitted alongside it. The wire form remains a JSON object; the list-of-entries is only the
    /// in-language shape. `value` is retained for the hand-rolled JSON (de)serialization of `V`.
    Map {
        entry: String,
        value: Box<WitType>,
    },
    Named(String),
}

impl WitType {
    /// Render this type as WIT source, e.g. `option<list<string>>`.
    pub(crate) fn render(&self) -> String {
        match self {
            WitType::String => "string".into(),
            WitType::Bool => "bool".into(),
            WitType::S32 => "s32".into(),
            WitType::S64 => "s64".into(),
            WitType::F64 => "f64".into(),
            WitType::Option(inner) => format!("option<{}>", inner.render()),
            WitType::List(inner) => format!("list<{}>", inner.render()),
            WitType::Map { entry, .. } => format!("list<{entry}>"),
            WitType::Named(name) => name.clone(),
        }
    }
}
