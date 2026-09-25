//! Write a standalone client from an offline catalog.

use std::{env, error::Error, fs, path::Path};

use autostamp_mcp::{GenerateOptions, PackageName, generate, parse_catalog, runtime::Limits};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [catalog_path, package, endpoint, directory, rest @ ..] = args.as_slice() else {
        return Err("usage: generate <catalog.json> <namespace:package[@version]> <endpoint> <output-directory> [bearer-secret-key] [timeout-ms max-response-bytes max-event-bytes]".into());
    };
    let catalog = parse_catalog(&fs::read_to_string(catalog_path)?)?;
    let package = PackageName::parse(package)?;
    let options = options(endpoint, rest)?;
    let generated = generate(&catalog, &package, &options)?;
    let directory = Path::new(directory);
    write(directory, "wit/world.wit", &generated.wit)?;
    write(directory, "Cargo.toml", &generated.cargo_toml)?;
    write(directory, "wasm.toml", &generated.wasm_toml)?;
    write(directory, "README.md", &generated.readme)?;
    for source in generated.rust {
        write(&directory.join("src"), &source.path, &source.contents)?;
    }
    for dependency in generated.wit_dependencies {
        write(
            &directory.join("wit"),
            &dependency.path,
            &dependency.contents,
        )?;
    }
    Ok(())
}

fn options(endpoint: &str, rest: &[String]) -> Result<GenerateOptions, Box<dyn Error>> {
    let (secret, limits) = match rest {
        [] => (None, Limits::default()),
        [secret] => (Some(secret), Limits::default()),
        [timeout, response, event] => (None, limits(timeout, response, event)?),
        [secret, timeout, response, event] => {
            (Some(secret), limits(timeout, response, event)?)
        }
        _ => return Err("expected an optional secret key followed by all three limits: timeout-ms max-response-bytes max-event-bytes".into()),
    };
    Ok(GenerateOptions {
        endpoint: endpoint.into(),
        bearer_secret: secret.filter(|key| key.as_str() != "-").cloned(),
        limits,
    })
}

fn limits(timeout: &str, response: &str, event: &str) -> Result<Limits, Box<dyn Error>> {
    Ok(Limits {
        request_timeout_ms: timeout.parse()?,
        max_response_bytes: response.parse()?,
        max_event_bytes: event.parse()?,
    })
}

fn write(root: &Path, relative: &str, contents: &str) -> Result<(), std::io::Error> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_fixture_limits_with_or_without_credentials() {
        for (args, secret) in [
            (vec!["250", "2048", "1024"], None),
            (vec!["-", "250", "2048", "1024"], None),
            (
                vec!["fixture-key", "250", "2048", "1024"],
                Some("fixture-key"),
            ),
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            let options = options("http://127.0.0.1:8080/mcp", &args).unwrap();
            assert_eq!(options.bearer_secret.as_deref(), secret);
            assert_eq!(options.limits.request_timeout_ms, 250);
            assert_eq!(options.limits.max_response_bytes, 2048);
            assert_eq!(options.limits.max_event_bytes, 1024);
        }
    }

    #[test]
    fn defaults_and_malformed_limits() {
        assert_eq!(
            options("https://example.test/mcp", &[]).unwrap().limits,
            Limits::default()
        );
        let secret = options("https://example.test/mcp", &["key".into()]).unwrap();
        assert_eq!(secret.bearer_secret.as_deref(), Some("key"));
        for args in [
            vec!["250", "2048"],
            vec!["not-a-timeout", "2048", "1024"],
            vec!["key", "250", "2048", "1024", "extra"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            assert!(options("https://example.test/mcp", &args).is_err());
        }
    }
}
