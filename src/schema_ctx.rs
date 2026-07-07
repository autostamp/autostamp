//! The [`SchemaCtx`] schema resolver: resolves `$ref`s against an OpenAPI document.

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use openapiv3::{OpenAPI, Parameter, ReferenceOr, RequestBody, Response, Schema, SchemaKind, Type};

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

    /// Resolve a `#/components/requestBodies/{Name}` reference to the request body it points at.
    ///
    /// Operations commonly share a single request body via `$ref` — e.g. Azure Cognitive
    /// Services points every Computer Vision operation's `requestBody` at
    /// `#/components/requestBodies/ImageUrl`. Without resolving it the operation would carry no
    /// body and, in the original generator, was dropped outright, leaving a function-less
    /// interface that wit-bindgen emits no `Guest` trait for (an uncompilable component).
    /// Follows up to a small fixed depth of indirection in case the named entry is itself a
    /// `$ref`.
    pub(crate) fn resolve_request_body(&self, reference: &str) -> Result<&'a RequestBody> {
        let bodies = &self
            .spec
            .components
            .as_ref()
            .context("spec missing components section")?
            .request_bodies;
        let mut reference = reference;
        for _ in 0..8 {
            let name = reference
                .strip_prefix("#/components/requestBodies/")
                .with_context(|| format!("unsupported request-body $ref: {reference}"))?;
            if name.is_empty() {
                bail!("malformed request-body $ref: {reference}");
            }
            match bodies
                .get(name)
                .with_context(|| format!("request-body $ref not found: {reference}"))?
            {
                ReferenceOr::Item(b) => return Ok(b),
                ReferenceOr::Reference { reference: next } => reference = next,
            }
        }
        bail!("request-body $ref nested too deeply: {reference}")
    }

    /// Resolve a `#/components/parameters/{Name}` reference to the parameter it points at.
    ///
    /// Shared parameters are pervasive: GitHub alone `$ref`s `#/components/parameters/*` over
    /// 2000 times (pagination `per-page`/`page` and the `owner`/`repo` *path* params). An
    /// unresolved ref previously hit `ReferenceOr::Reference { .. } => continue`, silently
    /// dropping the argument — and, for a path parameter, leaving a `{placeholder}` in the URL
    /// template with nothing to fill it. Follows chained refs to a small fixed depth in case the
    /// named entry is itself a `$ref`.
    pub(crate) fn resolve_parameter(&self, reference: &str) -> Result<&'a Parameter> {
        let params = &self
            .spec
            .components
            .as_ref()
            .context("spec missing components section")?
            .parameters;
        let mut reference = reference;
        for _ in 0..8 {
            let name = reference
                .strip_prefix("#/components/parameters/")
                .with_context(|| format!("unsupported parameter $ref: {reference}"))?;
            if name.is_empty() {
                bail!("malformed parameter $ref: {reference}");
            }
            match params
                .get(name)
                .with_context(|| format!("parameter $ref not found: {reference}"))?
            {
                ReferenceOr::Item(p) => return Ok(p),
                ReferenceOr::Reference { reference: next } => reference = next,
            }
        }
        bail!("parameter $ref nested too deeply: {reference}")
    }

    /// Resolve a `#/components/responses/{Name}` reference to the response it points at.
    ///
    /// Response `$ref`s are pervasive in real specs: GitHub points its `304`/`404`/`500`/`503`
    /// entries at shared `#/components/responses/*` definitions (`not_modified`, `not_found`,
    /// `internal_error`, `service_unavailable`) on nearly every operation. Without resolving
    /// them the operation's declared error surface — and any typed success body defined by
    /// reference — was invisible, leaving every function at `result<string, string>`. Follows
    /// chained refs to a small fixed depth in case the named entry is itself a `$ref`.
    pub(crate) fn resolve_response(&self, reference: &str) -> Result<&'a Response> {
        let responses = &self
            .spec
            .components
            .as_ref()
            .context("spec missing components section")?
            .responses;
        let mut reference = reference;
        for _ in 0..8 {
            let name = reference
                .strip_prefix("#/components/responses/")
                .with_context(|| format!("unsupported response $ref: {reference}"))?;
            if name.is_empty() {
                bail!("malformed response $ref: {reference}");
            }
            match responses
                .get(name)
                .with_context(|| format!("response $ref not found: {reference}"))?
            {
                ReferenceOr::Item(r) => return Ok(r),
                ReferenceOr::Reference { reference: next } => reference = next,
            }
        }
        bail!("response $ref nested too deeply: {reference}")
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

    // r[verify schema-ctx.request-body.resolve-ref]
    // Operations that share a request body via `$ref` (e.g. Azure Cognitive Services'
    // `#/components/requestBodies/ImageUrl`) must resolve to the named body rather than being
    // dropped, which previously left function-less, uncompilable interfaces.
    #[test]
    fn resolves_request_body_ref() {
        let spec: OpenAPI = serde_json::from_str(
            r##"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {},
              "components": {
                "requestBodies": {
                  "ImageUrl": {
                    "required": true,
                    "content": { "application/json": { "schema": { "type": "object" } } }
                  },
                  "Alias": { "$ref": "#/components/requestBodies/ImageUrl" }
                }
              }
            }"##,
        )
        .unwrap();
        let ctx = SchemaCtx::new(&spec);
        // Direct reference resolves to the named body.
        let body = ctx
            .resolve_request_body("#/components/requestBodies/ImageUrl")
            .unwrap();
        assert!(body.required);
        assert!(body.content.contains_key("application/json"));
        // A `$ref` that points at another `$ref` is followed to the concrete body.
        let chained = ctx
            .resolve_request_body("#/components/requestBodies/Alias")
            .unwrap();
        assert!(chained.required);
        // A dangling reference is an error (callers fall back to emitting a body-less op).
        assert!(
            ctx.resolve_request_body("#/components/requestBodies/Missing")
                .is_err()
        );
    }

    // r[verify schema-ctx.parameter.resolve-ref]
    // Shared parameters referenced via `#/components/parameters/*` (GitHub uses these for
    // `owner`/`repo`/`per-page` thousands of times) must resolve to the named parameter,
    // following one level of `$ref` indirection; a dangling ref is an error so the caller can
    // skip just that parameter with a diagnostic.
    #[test]
    fn resolves_parameter_ref() {
        let spec: OpenAPI = serde_json::from_str(
            r##"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {},
              "components": {
                "parameters": {
                  "PerPage": {
                    "name": "per_page",
                    "in": "query",
                    "required": false,
                    "schema": { "type": "integer" }
                  },
                  "Alias": { "$ref": "#/components/parameters/PerPage" }
                }
              }
            }"##,
        )
        .unwrap();
        let ctx = SchemaCtx::new(&spec);
        // Direct reference resolves to the named parameter.
        let param = ctx
            .resolve_parameter("#/components/parameters/PerPage")
            .unwrap();
        assert!(matches!(param, openapiv3::Parameter::Query { .. }));
        // A `$ref` that points at another `$ref` is followed to the concrete parameter.
        let chained = ctx
            .resolve_parameter("#/components/parameters/Alias")
            .unwrap();
        assert!(matches!(chained, openapiv3::Parameter::Query { .. }));
        // A dangling reference is an error (callers skip just that parameter, with a warning).
        assert!(
            ctx.resolve_parameter("#/components/parameters/Missing")
                .is_err()
        );
    }

    // r[verify schema-ctx.response.resolve-ref]
    // Responses shared via `#/components/responses/*` (issue #5) must resolve to the named
    // response, following `$ref` chains, so their body schema can be typed. A dangling ref is
    // an error, letting the caller fall back to the raw-body `string` instead of failing.
    #[test]
    fn resolves_response_ref() {
        let spec: OpenAPI = serde_json::from_str(
            r##"{
              "openapi": "3.0.0",
              "info": { "title": "t", "version": "1" },
              "paths": {},
              "components": {
                "responses": {
                  "Pet": {
                    "description": "a pet",
                    "content": { "application/json": { "schema": { "type": "object" } } }
                  },
                  "Alias": { "$ref": "#/components/responses/Pet" }
                }
              }
            }"##,
        )
        .unwrap();
        let ctx = SchemaCtx::new(&spec);
        // Direct reference resolves to the named response.
        let resp = ctx.resolve_response("#/components/responses/Pet").unwrap();
        assert!(resp.content.contains_key("application/json"));
        // A `$ref` that points at another `$ref` is followed to the concrete response.
        let chained = ctx
            .resolve_response("#/components/responses/Alias")
            .unwrap();
        assert!(chained.content.contains_key("application/json"));
        // A dangling reference is an error (callers fall back to the raw-body string).
        assert!(
            ctx.resolve_response("#/components/responses/Missing")
                .is_err()
        );
    }
}
