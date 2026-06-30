//! The [`SchemaCtx`] schema resolver: resolves `$ref`s against an OpenAPI document.

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
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
            ReferenceOr::Reference { reference } => self.resolve_reference(reference),
        }
    }

    pub(crate) fn resolve_schema_boxed(
        &self,
        r: &'a ReferenceOr<Box<Schema>>,
    ) -> Result<&'a Schema> {
        match r {
            ReferenceOr::Item(s) => Ok(s.as_ref()),
            ReferenceOr::Reference { reference } => self.resolve_reference(reference),
        }
    }

    /// Resolve a `#/components/schemas/...` reference string to the schema it points at.
    ///
    /// Beyond the common flat `#/components/schemas/{Name}` form, this also follows a
    /// JSON-Pointer tail into sub-schemas — `#/components/schemas/{Name}/properties/{field}`
    /// and `.../items` — which real-world specs (e.g. OpenAI, Linode) use to share a single
    /// nested schema. The named component is resolved first, then each `properties/{key}` or
    /// `items` segment is walked in turn.
    fn resolve_reference(&self, reference: &str) -> Result<&'a Schema> {
        let path = reference
            .strip_prefix("#/components/schemas/")
            .with_context(|| format!("unsupported ref: {reference}"))?;

        let mut segments = path.split('/');
        let name = segments
            .next()
            .filter(|n| !n.is_empty())
            .with_context(|| format!("malformed schema $ref: {reference}"))?;

        let schemas = &self
            .spec
            .components
            .as_ref()
            .context("spec missing components section")?
            .schemas;
        let mut current = self.resolve_schema(
            schemas
                .get(name)
                .with_context(|| format!("unknown schema $ref: {name}"))?,
        )?;

        while let Some(segment) = segments.next() {
            current = match segment {
                "properties" => {
                    let key = segments.next().with_context(|| {
                        format!("malformed schema $ref (dangling `properties`): {reference}")
                    })?;
                    let prop = object_properties(current)
                        .and_then(|props| props.get(key))
                        .with_context(|| format!("unknown schema $ref: {reference}"))?;
                    self.resolve_schema_boxed(prop)?
                }
                "items" => {
                    let items = array_items(current)
                        .with_context(|| format!("unknown schema $ref: {reference}"))?;
                    self.resolve_schema_boxed(items)?
                }
                other => bail!("unsupported schema $ref pointer segment `{other}`: {reference}"),
            };
        }

        Ok(current)
    }

    /// Whether `r` resolves to an `object` schema.
    pub(crate) fn is_object_schema(&self, r: &'a ReferenceOr<Schema>) -> bool {
        let Ok(s) = self.resolve_schema(r) else {
            return false;
        };
        matches!(s.schema_kind, SchemaKind::Type(Type::Object(_)))
    }
}

/// The `properties` map of an object (or untyped `any`) schema, if it has one.
fn object_properties(schema: &Schema) -> Option<&IndexMap<String, ReferenceOr<Box<Schema>>>> {
    match &schema.schema_kind {
        SchemaKind::Type(Type::Object(obj)) => Some(&obj.properties),
        SchemaKind::Any(any) => Some(&any.properties),
        _ => None,
    }
}

/// The `items` schema of an array (or untyped `any`) schema, if it has one.
fn array_items(schema: &Schema) -> Option<&ReferenceOr<Box<Schema>>> {
    match &schema.schema_kind {
        SchemaKind::Type(Type::Array(arr)) => arr.items.as_ref(),
        SchemaKind::Any(any) => any.items.as_ref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::SchemaCtx;
    use openapiv3::{OpenAPI, ReferenceOr, Schema, SchemaKind, Type};

    fn doc() -> OpenAPI {
        serde_json::from_str(
            r#"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {},
              "components": { "schemas": {
                "Parent": { "type": "object", "properties": {
                  "child": { "type": "string" }
                }},
                "ListHolder": { "type": "object", "properties": {
                  "list": { "type": "array", "items": { "type": "integer" } }
                }}
              }}
            }"#,
        )
        .unwrap()
    }

    fn reference(s: &str) -> ReferenceOr<Schema> {
        ReferenceOr::Reference {
            reference: s.to_string(),
        }
    }

    #[test]
    fn resolves_flat_component_ref() {
        let spec = doc();
        let r = reference("#/components/schemas/Parent");
        let ctx = SchemaCtx::new(&spec);
        assert!(ctx.is_object_schema(&r));
    }

    #[test]
    fn resolves_deep_property_ref() {
        let spec = doc();
        let r = reference("#/components/schemas/Parent/properties/child");
        let ctx = SchemaCtx::new(&spec);
        let resolved = ctx.resolve_schema(&r).unwrap();
        assert!(matches!(
            resolved.schema_kind,
            SchemaKind::Type(Type::String(_))
        ));
    }

    #[test]
    fn resolves_array_items_ref() {
        let spec = doc();
        let r = reference("#/components/schemas/ListHolder/properties/list/items");
        let ctx = SchemaCtx::new(&spec);
        let resolved = ctx.resolve_schema(&r).unwrap();
        assert!(matches!(
            resolved.schema_kind,
            SchemaKind::Type(Type::Integer(_))
        ));
    }

    #[test]
    fn unknown_deep_ref_errors() {
        let spec = doc();
        let r = reference("#/components/schemas/Parent/properties/missing");
        let ctx = SchemaCtx::new(&spec);
        assert!(ctx.resolve_schema(&r).is_err());
    }
}
