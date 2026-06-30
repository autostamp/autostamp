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
            WitType::Named(name) => name.clone(),
        }
    }
}
