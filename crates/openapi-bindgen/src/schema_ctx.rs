//! The [`SchemaCtx`] schema resolver: resolves `$ref`s against an OpenAPI document.

use anyhow::{Context, Result};
use openapiv3::{OpenAPI, ReferenceOr, Schema, SchemaKind, Type};

/// Resolves schema references against the components section of an OpenAPI document.
pub(crate) struct SchemaCtx<'a> {
    spec: &'a OpenAPI,
}

impl<'a> SchemaCtx<'a> {
    /// Create a resolver borrowing `spec`.
    pub(crate) fn new(spec: &'a OpenAPI) -> Self {
        Self { spec }
    }

    pub(crate) fn resolve_schema(&self, r: &'a ReferenceOr<Schema>) -> Result<&'a Schema> {
        match r {
            ReferenceOr::Item(s) => Ok(s),
            ReferenceOr::Reference { reference } => {
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .with_context(|| format!("unsupported ref: {reference}"))?;
                let schemas = self
                    .spec
                    .components
                    .as_ref()
                    .context("spec missing components section")?
                    .schemas
                    .get(name)
                    .with_context(|| format!("unknown schema $ref: {name}"))?;
                self.resolve_schema(schemas)
            }
        }
    }

    pub(crate) fn resolve_schema_boxed(
        &self,
        r: &'a ReferenceOr<Box<Schema>>,
    ) -> Result<&'a Schema> {
        match r {
            ReferenceOr::Item(s) => Ok(s.as_ref()),
            ReferenceOr::Reference { reference } => {
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .with_context(|| format!("unsupported ref: {reference}"))?;
                let schemas = self
                    .spec
                    .components
                    .as_ref()
                    .context("spec missing components section")?
                    .schemas
                    .get(name)
                    .with_context(|| format!("unknown schema $ref: {name}"))?;
                self.resolve_schema(schemas)
            }
        }
    }

    /// Whether `r` resolves to an `object` schema.
    pub(crate) fn is_object_schema(&self, r: &'a ReferenceOr<Schema>) -> bool {
        let Ok(s) = self.resolve_schema(r) else {
            return false;
        };
        matches!(s.schema_kind, SchemaKind::Type(Type::Object(_)))
    }
}
