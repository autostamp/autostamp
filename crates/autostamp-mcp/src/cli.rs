use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

use autostamp_mcp::{
    Catalog, GenerateOptions, Generated, PackageName, artifact_writer,
    discovery::{self, DiscoveryLimits},
    from_json_value, generate,
    native::NativeTransport,
    parse_catalog,
    runtime::{self, Limits},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Generate a typed WASI MCP client without invoking tools"
)]
#[command(group(clap::ArgGroup::new("input").required(true).args(["server", "catalog"])))]
struct Args {
    /// Discover every tools/list page from this Streamable HTTP URL.
    #[arg(long)]
    server: Option<String>,
    /// Read a complete tools/list result from a bounded UTF-8 JSON file.
    #[arg(long, requires = "endpoint")]
    catalog: Option<PathBuf>,
    /// Generated client's URL (defaults to --server; required with --catalog).
    #[arg(long)]
    endpoint: Option<String>,
    /// Target WIT package, including an explicit version: namespace:name@version.
    #[arg(long)]
    package: String,
    /// Output crate directory; edited or unowned artifacts are never overwritten.
    #[arg(long)]
    output: PathBuf,
    /// Environment variable containing the discovery bearer token (live only).
    #[arg(long, requires = "server", conflicts_with = "catalog")]
    token_env: Option<String>,
    /// Host wasmcloud:secrets key used by generated calls, NOT a token.
    #[arg(long)]
    bearer_secret: Option<String>,
    /// Total per-request deadline, including streaming and protocol replies.
    #[arg(long, default_value_t = Limits::default().request_timeout_ms)]
    timeout_ms: u64,
    /// Maximum bytes consumed per HTTP response.
    #[arg(long, default_value_t = Limits::default().max_response_bytes)]
    max_response_bytes: usize,
    /// Maximum bytes in one SSE event or physical line.
    #[arg(long, default_value_t = Limits::default().max_event_bytes)]
    max_event_bytes: usize,
    /// Maximum number of discovery pages.
    #[arg(long, default_value_t = DiscoveryLimits::default().max_pages)]
    max_pages: usize,
    /// Maximum catalog tool count (live or offline).
    #[arg(long, default_value_t = DiscoveryLimits::default().max_tools)]
    max_tools: usize,
    /// Maximum serialized discovery catalog or offline input file bytes.
    #[arg(long, default_value_t = DiscoveryLimits::default().max_catalog_bytes)]
    max_catalog_bytes: usize,
}

pub(super) fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("autostamp-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    if args.catalog.is_some() && args.token_env.is_some() {
        return Err("--token-env requires live --server input, not --catalog".into());
    }
    let package = PackageName::parse(&args.package).map_err(|error| error.to_string())?;
    if package.version().is_none() {
        return Err("--package requires an explicit version: namespace:name@version".into());
    }
    let limits = Limits {
        request_timeout_ms: args.timeout_ms,
        max_response_bytes: args.max_response_bytes,
        max_event_bytes: args.max_event_bytes,
    };
    limits.validate().map_err(|error| error.to_string())?;
    if u32::try_from(limits.max_response_bytes).is_err()
        || u32::try_from(limits.max_event_bytes).is_err()
    {
        return Err("response/event byte limits must fit the wasm32 address space".into());
    }
    let discovery_limits = DiscoveryLimits {
        max_pages: args.max_pages,
        max_tools: args.max_tools,
        max_catalog_bytes: args.max_catalog_bytes,
    };
    discovery_limits
        .validate()
        .map_err(|error| error.to_string())?;
    let endpoint = args
        .endpoint
        .or_else(|| args.server.clone())
        .ok_or("--endpoint is required with --catalog")?;
    runtime::validate_endpoint(&endpoint).map_err(|error| error.to_string())?;
    if let Some(server) = &args.server {
        runtime::validate_endpoint(server).map_err(|error| error.to_string())?;
    }
    if args
        .bearer_secret
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.chars().any(char::is_control))
    {
        return Err("--bearer-secret must be a nonempty key without control characters".into());
    }
    let bearer = args.token_env.as_deref().map(read_token).transpose()?;
    let result = (|| {
        reject_credential_echo(&endpoint, bearer.as_deref())?;
        reject_credential_echo(&args.package, bearer.as_deref())?;
        if let Some(key) = &args.bearer_secret {
            reject_credential_echo(key, bearer.as_deref())?;
        }
        let catalog = if let Some(path) = args.catalog {
            let catalog = read_catalog(&path, discovery_limits.max_catalog_bytes)?;
            if catalog.len() > discovery_limits.max_tools {
                return Err("offline catalog tool limit exceeded".into());
            }
            catalog
        } else {
            let transport = NativeTransport::new(limits.max_response_bytes.min(16 * 1024))
                .map_err(|error| error.to_string())?;
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("create native executor: {error}"))?;
            let value = executor
                .block_on(discovery::discover(
                    transport,
                    args.server.ok_or("--server or --catalog is required")?,
                    bearer.clone(),
                    limits,
                    discovery_limits,
                ))
                .map_err(|error| error.to_string())?;
            reject_catalog_credential(&value, bearer.as_deref())?;
            from_json_value(value).map_err(|error| error.to_string())?
        };
        let generated = generate(
            &catalog,
            &package,
            &GenerateOptions {
                endpoint,
                bearer_secret: args.bearer_secret,
                limits,
            },
        )
        .map_err(|error| error.to_string())?;
        let files = artifact_files(generated);
        artifact_writer::write_crate(&args.output, &files).map_err(|error| error.to_string())
    })();
    result.map_err(|message| redact(&message, bearer.as_deref()))
}

fn read_token(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err("--token-env requires a nonempty environment variable name".into());
    }
    let token = std::env::var(name)
        .map_err(|_| "--token-env variable is missing or is not valid Unicode".to_owned())?;
    if token.is_empty() || !token.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err("--token-env variable must contain a nonempty printable bearer token".into());
    }
    Ok(token)
}

fn read_catalog(path: &Path, maximum: usize) -> Result<Catalog, String> {
    if !std::fs::metadata(path)
        .map_err(|error| format!("{}: inspect catalog: {error}", path.display()))?
        .is_file()
    {
        return Err(format!(
            "{}: catalog must be a regular file",
            path.display()
        ));
    }
    let file =
        File::open(path).map_err(|error| format!("{}: open catalog: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| format!("{}: inspect catalog: {error}", path.display()))?
        .is_file()
    {
        return Err(format!(
            "{}: catalog must be a regular file",
            path.display()
        ));
    }
    let bound = u64::try_from(maximum)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or("--max-catalog-bytes is too large")?;
    let mut bytes = Vec::new();
    file.take(bound)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: read catalog: {error}", path.display()))?;
    if bytes.len() > maximum {
        return Err(format!(
            "{}: offline catalog byte limit exceeded",
            path.display()
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{}: catalog must be UTF-8", path.display()))?;
    parse_catalog(text).map_err(|error| format!("{}: {error}", path.display()))
}

fn artifact_files(generated: Generated) -> BTreeMap<PathBuf, String> {
    let mut files = BTreeMap::from([
        (PathBuf::from("Cargo.toml"), generated.cargo_toml),
        (PathBuf::from("wasm.toml"), generated.wasm_toml),
        (PathBuf::from("README.md"), generated.readme),
        (PathBuf::from("wit/world.wit"), generated.wit),
    ]);
    // The writer's portable artifact names use '/' even on Windows.
    for source in generated.rust {
        files.insert(
            PathBuf::from(format!("src/{}", source.path)),
            source.contents,
        );
    }
    for dependency in generated.wit_dependencies {
        files.insert(
            PathBuf::from(format!("wit/{}", dependency.path)),
            dependency.contents,
        );
    }
    files
}

fn reject_credential_echo(text: &str, token: Option<&str>) -> Result<(), String> {
    if token.is_some_and(|token| redact(text, Some(token)) != text) {
        return Err("discovered/generated content contains the discovery credential; refusing to persist it".into());
    }
    Ok(())
}

fn reject_catalog_credential(value: &serde_json::Value, token: Option<&str>) -> Result<(), String> {
    match value {
        serde_json::Value::String(text) => reject_credential_echo(text, token)?,
        serde_json::Value::Array(values) => {
            for value in values {
                reject_catalog_credential(value, token)?;
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                reject_credential_echo(key, token)?;
                reject_catalog_credential(value, token)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn redact(message: &str, token: Option<&str>) -> String {
    let Some(token) = token else {
        return message.to_owned();
    };
    let mut result = message.replace(token, "[redacted]");
    let escaped = serde_json::Value::String(token.into()).to_string();
    if let Some(escaped) = escaped.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        result = result.replace(escaped, "[redacted]");
    }
    result
}
