<h1 align="center">openapi-bindgen</h1>
<div align="center">
  <strong>
    Convert OpenAPI schema definitions to WebAssembly Components
  </strong>
</div>

<br />

<div align="center">
  <!-- Crates version -->
  <a href="https://crates.io/crates/openapi-bindgen">
    <img src="https://img.shields.io/crates/v/openapi-bindgen.svg?style=flat-square"
    alt="Crates.io version" />
  </a>
  <!-- Downloads -->
  <a href="https://crates.io/crates/openapi-bindgen">
    <img src="https://img.shields.io/crates/d/openapi-bindgen.svg?style=flat-square"
      alt="Download" />
  </a>
  <!-- docs.rs docs -->
  <a href="https://docs.rs/openapi-bindgen">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="docs.rs docs" />
  </a>
</div>

<div align="center">
  <h3>
    <a href="https://docs.rs/openapi-bindgen">
      API Docs
    </a>
    <span> | </span>
    <a href="https://github.com/yoshuawuyts/openapi-bindgen/releases">
      Releases
    </a>
    <span> | </span>
    <a href="https://github.com/yoshuawuyts/openapi-bindgen/blob/master/.github/CONTRIBUTING.md">
      Contributing
    </a>
  </h3>
</div>

## Installation
```sh
$ cargo add openapi-bindgen
```

## WebAssembly component
Besides the Rust library, this crate builds as a [WebAssembly component][component] that
exports the `openapi-bindgen:generator/generator` interface defined in
[`crates/openapi-bindgen/wit`](crates/openapi-bindgen/wit/world.wit):

```sh
$ just component
# or: cargo build -p openapi-bindgen --target wasm32-wasip2 --release
```

[component]: https://component-model.bytecodealliance.org/

## Safety
This crate denies `unsafe_code` throughout. The single exception is the
wit-bindgen-generated component bindings, which require the component-model FFI glue and
are confined to one module behind `#[allow(unsafe_code)]`.

## Contributing
Want to join us? Check out our ["Contributing" guide][contributing] and take a
look at some of these issues:

- [Issues labeled "good first issue"][good-first-issue]
- [Issues labeled "help wanted"][help-wanted]

[contributing]: https://github.com/yoshuawuyts/openapi-bindgen/blob/master/.github/CONTRIBUTING.md
[good-first-issue]: https://github.com/yoshuawuyts/openapi-bindgen/labels/good%20first%20issue
[help-wanted]: https://github.com/yoshuawuyts/openapi-bindgen/labels/help%20wanted

## License

<sup>
Licensed under the <a href="LICENSE-APACHE">Apache License, Version 2.0 with the
LLVM exception</a>.
</sup>

<br/>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>
