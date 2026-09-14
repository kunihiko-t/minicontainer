//! Anonymous OCI registry pulls pinned by manifest digest.
//!
//! Pulls fetch the manifest, config, and layer blobs over HTTPS, verify
//! every descriptor, and assemble canonical MiniBundle bytes through the
//! shared [`crate::import`] core. No credentials are ever sent: a registry
//! that answers 401 is reported, not authenticated against.

use crate::{
    MAX_JSON_LEN, MEDIA_TYPE_CONFIG, MEDIA_TYPE_LAYER, MEDIA_TYPE_MANIFEST, OciError,
    import::{Blob, assemble_bundle, decode_digest, require_media_type, shape},
    parse::{JsonValue, parse_json},
};
use minicontainer_bundle::format_digest;
use sha2::{Digest, Sha256};
use std::{io::Read, time::Duration};

/// Maximum accepted HTTP redirects per fetched blob.
pub const MAX_REDIRECTS: u32 = 5;
/// Default TCP connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default whole-request timeout, including the body download.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// One digest-pinned pull reference: `host[:port]/repository@sha256:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry host with an optional port, without scheme or userinfo.
    pub registry: String,
    /// Repository path inside the registry.
    pub repository: String,
    /// Pinned manifest digest in `sha256:<hex>` form.
    pub digest: String,
}

/// Timeout knobs for one pull. Tests shorten these; the CLI uses [`Default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOptions {
    /// TCP connect timeout per request.
    pub connect_timeout: Duration,
    /// Whole-request timeout per blob, including the body download.
    pub request_timeout: Duration,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Parses one strict pull reference. The registry host is required, tags
/// are rejected (digest pins only), and userinfo is rejected because pulls
/// are anonymous.
pub fn parse_reference(text: &str) -> Result<Reference, OciError> {
    let registry_error = |message: &str| OciError::Registry {
        message: message.to_owned(),
    };
    let mut parts = text.split('@');
    let (Some(head), Some(digest), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(registry_error(
            "reference must be host/repository@sha256:<hex>",
        ));
    };
    if decode_digest(digest).is_none() {
        return Err(registry_error(
            "reference digest must be sha256:<64 lowercase hex>",
        ));
    }
    let Some((host, repository)) = head.split_once('/') else {
        return Err(registry_error("reference must name a registry host"));
    };
    if !valid_host(host) || !valid_repository(repository) {
        return Err(registry_error("reference host or repository is malformed"));
    }
    Ok(Reference {
        registry: host.to_owned(),
        repository: repository.to_owned(),
        digest: digest.to_owned(),
    })
}

/// Pulls one pinned manifest and assembles canonical MiniBundle bytes.
pub fn pull_bundle(reference: &Reference, options: &PullOptions) -> Result<Vec<u8>, OciError> {
    pull_from(reference, options, "https")
}

/// Pulls over plain http for loopback fixture tests. Every other caller
/// uses [`pull_bundle`] over https.
#[cfg(test)]
pub(crate) fn pull_bundle_insecure(
    reference: &Reference,
    options: &PullOptions,
) -> Result<Vec<u8>, OciError> {
    pull_from(reference, options, "http")
}

/// Pulls one pinned manifest with the given URL scheme.
fn pull_from(
    reference: &Reference,
    options: &PullOptions,
    scheme: &str,
) -> Result<Vec<u8>, OciError> {
    let agent = pull_agent(options);
    let manifest_url = format!(
        "{scheme}://{registry}/v2/{repository}/manifests/{digest}",
        registry = reference.registry,
        repository = reference.repository,
        digest = reference.digest
    );
    let manifest = fetch_blob(&agent, &manifest_url, MAX_JSON_LEN, "manifest")?;
    let actual = format!("sha256:{}", format_digest(digest_of(&manifest)));
    if actual != reference.digest {
        return Err(OciError::DigestMismatch {
            path: std::path::PathBuf::from(&manifest_url),
            expected: reference.digest.clone(),
            actual,
        });
    }
    let manifest_value = parse_json(&manifest)?;
    require_media_type(&manifest_value, MEDIA_TYPE_MANIFEST, "manifest")?;

    let config_entry = manifest_value
        .get("config")
        .ok_or_else(|| shape("manifest is missing config"))?;
    require_media_type(config_entry, MEDIA_TYPE_CONFIG, "config")?;
    let config = fetch_descriptor(
        &agent,
        reference,
        config_entry,
        MAX_JSON_LEN,
        "config",
        scheme,
    )?;

    let layers = manifest_value
        .get("layers")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| shape("manifest layers must be an array"))?;
    if layers.len() != 1 {
        return Err(shape(&format!(
            "manifest must hold one layer, found {}",
            layers.len()
        )));
    }
    require_media_type(&layers[0], MEDIA_TYPE_LAYER, "layer")?;
    let layer = fetch_descriptor(
        &agent,
        reference,
        &layers[0],
        minicontainer_bundle::MAX_BUNDLE_LEN,
        "layer",
        scheme,
    )?;

    assemble_bundle(&config.bytes, &layer)
}

/// Builds the pull agent: no automatic redirects, no status errors, bounded
/// timeouts. Redirects run through the manual policy loop instead.
fn pull_agent(options: &PullOptions) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(options.connect_timeout))
        .timeout_global(Some(options.request_timeout))
        .build()
        .into()
}

/// Fetches one descriptor blob with size-then-digest verification.
fn fetch_descriptor(
    agent: &ureq::Agent,
    reference: &Reference,
    entry: &JsonValue,
    limit: u64,
    role: &str,
    scheme: &str,
) -> Result<Blob, OciError> {
    let expected_size = entry
        .get("size")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| shape(&format!("{role} size must be a plain integer")))?;
    if expected_size > limit {
        return Err(OciError::TooLarge {
            path: std::path::PathBuf::from(format!("{role} descriptor")),
            limit,
        });
    }
    let digest = entry
        .get("digest")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| shape(&format!("{role} is missing digest")))?;
    if decode_digest(digest).is_none() {
        return Err(shape(&format!("{role} digest is not sha256 hex")));
    }
    let url = format!(
        "{scheme}://{registry}/v2/{repository}/blobs/{digest}",
        registry = reference.registry,
        repository = reference.repository,
    );
    let bytes = fetch_blob(agent, &url, limit, role)?;
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(OciError::SizeMismatch {
            path: std::path::PathBuf::from(&url),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual = format!("sha256:{}", format_digest(digest_of(&bytes)));
    if actual != digest {
        return Err(OciError::DigestMismatch {
            path: std::path::PathBuf::from(&url),
            expected: digest.to_owned(),
            actual,
        });
    }
    Ok(Blob {
        bytes,
        digest: digest.to_owned(),
    })
}

/// Fetches one URL with the manual redirect policy: at most
/// [`MAX_REDIRECTS`] hops, every hop over HTTPS except http-to-localhost.
fn fetch_blob(agent: &ureq::Agent, url: &str, limit: u64, role: &str) -> Result<Vec<u8>, OciError> {
    require_secure_url(url)?;
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let mut response = agent
            .get(current.as_str())
            .call()
            .map_err(|error| registry_io(&error))?;
        let status = response.status().as_u16();
        if status == 200 {
            if let Some(declared) = response.body().content_length()
                && declared > limit
            {
                return Err(OciError::TooLarge {
                    path: std::path::PathBuf::from(&current),
                    limit,
                });
            }
            return read_bounded(response.body_mut().as_reader(), limit, &current, role);
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| registry_failure("redirect is missing Location"))?;
            current = resolve_redirect(&current, &location)?;
            require_secure_url(&current)?;
            continue;
        }
        return Err(match status {
            401 => registry_failure("registry requires authentication"),
            404 => registry_failure(&format!("{role} was not found")),
            _ => registry_failure(&format!("registry returned status {status}")),
        });
    }
    Err(registry_failure("redirect limit exceeded"))
}

/// Reads one response body with a take-one-more sentinel against servers
/// that lie about Content-Length.
fn read_bounded(reader: impl Read, limit: u64, url: &str, role: &str) -> Result<Vec<u8>, OciError> {
    let mut bounded = reader.take(limit + 1);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .map_err(|error| registry_failure(&format!("{role} download failed: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(OciError::TooLarge {
            path: std::path::PathBuf::from(url),
            limit,
        });
    }
    Ok(bytes)
}

/// Requires an https URL, or http to localhost for fixture tests. Real
/// pulls always start from https; only the loopback carve-out keeps the
/// offline fixture suite possible.
fn require_secure_url(url: &str) -> Result<(), OciError> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(registry_failure("URL must be absolute"));
    };
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err(registry_failure("URL must name a host"));
    }
    if host.contains('@') {
        return Err(registry_failure("URL must not carry userinfo"));
    }
    let bare = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    let loopback = matches!(bare, "localhost" | "127.0.0.1" | "::1");
    if scheme == "https" || (scheme == "http" && loopback) {
        Ok(())
    } else {
        Err(registry_failure("redirect target must be https"))
    }
}

/// Resolves one Location value against the request URL.
fn resolve_redirect(base: &str, location: &str) -> Result<String, OciError> {
    if location.contains("://") {
        if location.starts_with("https://") || location.starts_with("http://") {
            return Ok(location.to_owned());
        }
        return Err(registry_failure("redirect scheme must be http or https"));
    }
    let Some((scheme, rest)) = base.split_once("://") else {
        return Err(registry_failure("request URL is malformed"));
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(registry_failure("request URL is malformed"));
    }
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!("{scheme}://{authority}/{path}"));
    }
    if location.is_empty() {
        return Err(registry_failure("redirect Location is empty"));
    }
    let base_path = rest.split_at(rest.find('/').unwrap_or(rest.len())).1;
    let merged = match base_path.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{location}"),
        None => format!("/{location}"),
    };
    Ok(format!("{scheme}://{authority}{}", normalize_path(&merged)))
}

/// Removes dot segments from a merged redirect path.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    format!("/{}", segments.join("/"))
}

/// Validates a registry host with an optional port. Userinfo is rejected,
/// and IPv6 literals must use brackets.
fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return false;
        };
        let (ipv6, tail) = rest.split_at(close);
        let tail = &tail[1..];
        if tail.is_empty() {
            (ipv6, None)
        } else if let Some(port) = tail.strip_prefix(':') {
            (ipv6, Some(port))
        } else {
            return false;
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if !port.is_empty() => (name, Some(port)),
            _ => (host, None),
        }
    };
    if name.is_empty() || name.bytes().any(|byte| byte == b'@' || byte == b'/') {
        return false;
    }
    let ipv6 = host.starts_with('[');
    if !name.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-') || (ipv6 && byte == b':')
    }) {
        return false;
    }
    match port {
        None => !name.contains(':'),
        Some(digits) => {
            !digits.is_empty()
                && digits.len() <= 5
                && digits.bytes().all(|byte| byte.is_ascii_digit())
                && digits.parse::<u32>().is_ok_and(|port| port > 0)
        }
    }
}

/// Validates an OCI repository path: lowercase segments without empties.
fn valid_repository(repository: &str) -> bool {
    if repository.is_empty() || repository.len() > 255 {
        return false;
    }
    repository.split('/').all(|segment| {
        !segment.is_empty()
            && segment.len() <= 128
            && segment.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            && segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && segment
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    })
}

/// SHA-256 digest of one byte string.
fn digest_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Builds a registry failure without secrets: statuses and hosts only.
fn registry_failure(message: &str) -> OciError {
    OciError::Registry {
        message: message.to_owned(),
    }
}

/// Maps a transport error to a registry failure without a body dump.
fn registry_io(error: &ureq::Error) -> OciError {
    match error {
        ureq::Error::Timeout(_) => registry_failure("request timed out"),
        ureq::Error::HostNotFound => registry_failure("registry host was not found"),
        ureq::Error::Io(error) => registry_failure(&format!("transfer failed: {error}")),
        _ => registry_failure("request failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export_bundle;
    use minicontainer_bundle::ImageSpec;
    use std::{
        collections::HashMap,
        io::Write,
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    const LAYER_DIGEST: &str = "6c710671215c4929fa600956d236ae82df1f3a9f05f0b125c3fb0feed9096136";
    const CONFIG_DIGEST: &str = "3f19ea537fd3b4cdee56a34be0d9c7f90b1472d3e12c0e7aff3a505f7dfac98e";
    const MANIFEST_DIGEST: &str =
        "f095c9356a85036bc3e4a08e562e6826321aedd3569aaa15e076a9639170995b";

    /// One scripted route: status, extra headers, body, an optional
    /// pre-response delay, and an optional truncation point with a declared
    /// Content-Length larger than the sent prefix.
    #[derive(Clone)]
    struct Route {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        delay_ms: u64,
        declared_len: Option<usize>,
    }

    impl Route {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body,
                delay_ms: 0,
                declared_len: None,
            }
        }
    }

    /// A minimal loopback HTTP server for pull tests. Each connection runs
    /// on its own thread; dropping the server stops the accept loop.
    struct FixtureRegistry {
        base: String,
        shutdown: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FixtureRegistry {
        fn serve(routes: HashMap<String, Route>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
            listener.set_nonblocking(true).expect("nonblocking");
            let base = format!("http://{}", listener.local_addr().expect("addr"));
            let shutdown = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&shutdown);
            let routes = Arc::new(routes);
            let handle = std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let (stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    };
                    let routes = Arc::clone(&routes);
                    std::thread::spawn(move || serve_connection(stream, &routes));
                }
            });
            Self {
                base,
                shutdown,
                handle: Some(handle),
            }
        }

        fn host(&self) -> &str {
            self.base.strip_prefix("http://").expect("fixture base")
        }
    }

    impl Drop for FixtureRegistry {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Serves one HTTP/1.0 connection with `Connection: close` framing.
    fn serve_connection(mut stream: std::net::TcpStream, routes: &HashMap<String, Route>) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while head.len() < 16 * 1024 {
            match stream.read_exact(&mut byte) {
                Ok(()) => head.push(byte[0]),
                Err(_) => return,
            }
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head);
        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let route = routes.get(path).cloned().unwrap_or(Route {
            status: 404,
            headers: Vec::new(),
            body: b"no such fixture\n".to_vec(),
            delay_ms: 0,
            declared_len: None,
        });
        if route.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(route.delay_ms));
        }
        let sent = route
            .declared_len
            .map_or(route.body.len(), |declared| route.body.len().min(declared));
        let claimed = route.declared_len.unwrap_or(route.body.len());
        let reason = match route.status {
            200 => "OK",
            302 => "Found",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.0 {} {reason}\r\nContent-Length: {claimed}\r\n",
            route.status
        );
        for (name, value) in &route.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&route.body[..sent]);
    }

    /// The golden fixture: name `hello`, no arguments, ELF `0x00..0f`.
    fn golden_bundle() -> Vec<u8> {
        let elf: Vec<u8> = (0..16).collect();
        minicontainer_bundle::build(ImageSpec {
            name: "hello",
            args: &[],
            elf: &elf,
        })
        .expect("golden bundle builds")
    }

    /// Builds the three golden blobs via a real export.
    fn golden_blobs() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        static NEXT_PULL_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_PULL_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("minicontainer-pull-{}-{id}", std::process::id()));
        let dest = root.join("layout");
        export_bundle(&golden_bundle(), &dest).expect("export golden");
        let manifest =
            std::fs::read(dest.join(format!("blobs/sha256/{MANIFEST_DIGEST}"))).expect("manifest");
        let config =
            std::fs::read(dest.join(format!("blobs/sha256/{CONFIG_DIGEST}"))).expect("config");
        let layer =
            std::fs::read(dest.join(format!("blobs/sha256/{LAYER_DIGEST}"))).expect("layer");
        std::fs::remove_dir_all(&root).expect("cleanup");
        (manifest, config, layer)
    }

    fn fast_options() -> PullOptions {
        PullOptions {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        }
    }

    fn pull_reference(host: &str) -> Reference {
        parse_reference(&format!("{host}/test/hello@sha256:{MANIFEST_DIGEST}")).expect("reference")
    }

    #[test]
    fn parses_strict_references() {
        assert_eq!(
            parse_reference("registry.example.com:5000/a/b@sha256:ab"),
            Err(OciError::Registry {
                message: "reference digest must be sha256:<64 lowercase hex>".to_owned(),
            })
        );
        let good = format!("registry.example.com:5000/a/b@sha256:{}", "ab".repeat(32));
        assert_eq!(
            parse_reference(&good),
            Ok(Reference {
                registry: "registry.example.com:5000".to_owned(),
                repository: "a/b".to_owned(),
                digest: format!("sha256:{}", "ab".repeat(32)),
            })
        );
        let v6 = format!("[::1]:5000/a@sha256:{}", "ab".repeat(32));
        assert_eq!(
            parse_reference(&v6).unwrap().registry,
            "[::1]:5000".to_owned()
        );
        for bad in [
            "a/b@sha256:zz",
            "repo@sha256:",
            "host/repo:tag",
            "host/repo",
            "host//repo@sha256:",
            "user@host/repo@sha256:",
            "host/Repo@sha256:",
            "host/repo/@sha256:",
            "host:0/repo@sha256:",
            "@sha256:",
        ] {
            let text = if bad.ends_with(':') {
                format!("{bad}{}", "ab".repeat(32))
            } else {
                bad.to_owned()
            };
            assert!(parse_reference(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn pulls_the_golden_blobs_from_a_fixture_registry() {
        let (manifest, config, layer) = golden_blobs();
        let routes = HashMap::from([
            (
                format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
                Route::ok(manifest),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{CONFIG_DIGEST}"),
                Route::ok(config),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{LAYER_DIGEST}"),
                Route::ok(layer),
            ),
        ]);
        let registry = FixtureRegistry::serve(routes);

        let pulled =
            pull_bundle_insecure(&pull_reference(registry.host()), &fast_options()).expect("pull");
        assert_eq!(pulled, golden_bundle());
    }

    #[test]
    fn reports_missing_and_unauthorized_blobs() {
        let registry = FixtureRegistry::serve(HashMap::new());

        let error = pull_bundle_insecure(&pull_reference(registry.host()), &fast_options())
            .expect_err("missing manifest must fail");
        assert_eq!(
            error,
            OciError::Registry {
                message: "manifest was not found".to_owned(),
            }
        );

        let routes = HashMap::from([(
            format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
            Route {
                status: 401,
                headers: Vec::new(),
                body: Vec::new(),
                delay_ms: 0,
                declared_len: None,
            },
        )]);
        let registry = FixtureRegistry::serve(routes);
        let error = pull_bundle_insecure(&pull_reference(registry.host()), &fast_options())
            .expect_err("401 must fail without credentials");
        assert_eq!(
            error,
            OciError::Registry {
                message: "registry requires authentication".to_owned(),
            }
        );
    }

    #[test]
    fn follows_redirects_within_the_limit() {
        let (manifest, config, layer) = golden_blobs();
        let routes = HashMap::from([
            (
                format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
                Route {
                    status: 302,
                    headers: vec![("Location".to_owned(), "/real-manifest".to_owned())],
                    body: Vec::new(),
                    delay_ms: 0,
                    declared_len: None,
                },
            ),
            ("/real-manifest".to_owned(), Route::ok(manifest)),
            (
                format!("/v2/test/hello/blobs/sha256:{CONFIG_DIGEST}"),
                Route::ok(config),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{LAYER_DIGEST}"),
                Route::ok(layer),
            ),
        ]);
        let registry = FixtureRegistry::serve(routes);

        let pulled =
            pull_bundle_insecure(&pull_reference(registry.host()), &fast_options()).expect("pull");
        assert_eq!(pulled, golden_bundle());
    }

    #[test]
    fn rejects_redirect_loops_and_insecure_targets() {
        let routes = HashMap::from([(
            format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
            Route {
                status: 302,
                headers: vec![(
                    "Location".to_owned(),
                    format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
                )],
                body: Vec::new(),
                delay_ms: 0,
                declared_len: None,
            },
        )]);
        let registry = FixtureRegistry::serve(routes);
        let error = pull_bundle_insecure(&pull_reference(registry.host()), &fast_options())
            .expect_err("loop must fail");
        assert_eq!(
            error,
            OciError::Registry {
                message: "redirect limit exceeded".to_owned(),
            }
        );

        let routes = HashMap::from([(
            format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
            Route {
                status: 302,
                headers: vec![("Location".to_owned(), "http://example.com/blob".to_owned())],
                body: Vec::new(),
                delay_ms: 0,
                declared_len: None,
            },
        )]);
        let registry = FixtureRegistry::serve(routes);
        let error = pull_bundle_insecure(&pull_reference(registry.host()), &fast_options())
            .expect_err("downgrade must fail before fetching");
        assert_eq!(
            error,
            OciError::Registry {
                message: "redirect target must be https".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_oversized_truncated_and_tampered_blobs() {
        let (manifest, config, _) = golden_blobs();
        let oversized = Route {
            status: 200,
            headers: Vec::new(),
            body: vec![0u8; 16],
            delay_ms: 0,
            declared_len: Some(minicontainer_bundle::MAX_BUNDLE_LEN as usize + 1),
        };
        let routes = HashMap::from([
            (
                format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
                Route::ok(manifest.clone()),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{CONFIG_DIGEST}"),
                Route::ok(config.clone()),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{LAYER_DIGEST}"),
                oversized,
            ),
        ]);
        let registry = FixtureRegistry::serve(routes);
        assert!(matches!(
            pull_bundle_insecure(&pull_reference(registry.host()), &fast_options()),
            Err(OciError::TooLarge { .. })
        ));

        let truncated = Route {
            status: 200,
            headers: Vec::new(),
            body: manifest[..10].to_vec(),
            delay_ms: 0,
            declared_len: Some(manifest.len()),
        };
        let routes = HashMap::from([(
            format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
            truncated,
        )]);
        let registry = FixtureRegistry::serve(routes);
        assert!(matches!(
            pull_bundle_insecure(&pull_reference(registry.host()), &fast_options()),
            Err(OciError::Registry { .. })
        ));

        let mut tampered = golden_bundle();
        tampered[0] ^= 1;
        let routes = HashMap::from([
            (
                format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
                Route::ok(manifest),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{CONFIG_DIGEST}"),
                Route::ok(config),
            ),
            (
                format!("/v2/test/hello/blobs/sha256:{LAYER_DIGEST}"),
                Route::ok(tampered),
            ),
        ]);
        let registry = FixtureRegistry::serve(routes);
        assert!(matches!(
            pull_bundle_insecure(&pull_reference(registry.host()), &fast_options()),
            Err(OciError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn times_out_a_slow_registry() {
        let routes = HashMap::from([(
            format!("/v2/test/hello/manifests/sha256:{MANIFEST_DIGEST}"),
            Route {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
                delay_ms: 3_000,
                declared_len: None,
            },
        )]);
        let registry = FixtureRegistry::serve(routes);
        let options = PullOptions {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_millis(100),
        };
        let error = pull_bundle_insecure(&pull_reference(registry.host()), &options)
            .expect_err("slow server must time out");
        assert_eq!(
            error,
            OciError::Registry {
                message: "request timed out".to_owned(),
            }
        );
    }

    #[test]
    fn resolves_redirect_locations() {
        assert_eq!(
            resolve_redirect("https://h:1/a/b", "https://x/y").unwrap(),
            "https://x/y"
        );
        assert_eq!(
            resolve_redirect("https://h:1/a/b", "/root").unwrap(),
            "https://h:1/root"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "c").unwrap(),
            "https://h/a/c"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "../c/./d").unwrap(),
            "https://h/c/d"
        );
        assert!(resolve_redirect("https://h/a", "ftp://x").is_err());
        assert!(resolve_redirect("https://h/a", "").is_err());
    }

    #[test]
    fn accepts_https_and_loopback_http_only() {
        for url in [
            "https://registry.example.com/v2/",
            "https://127.0.0.1:5000/v2/",
            "http://localhost:5000/v2/",
            "http://127.0.0.1:5000/v2/",
            "http://[::1]:5000/v2/",
        ] {
            assert!(require_secure_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://registry.example.com/v2/",
            "http://example.com:80/v2/",
            "https://user@example.com/v2/",
            "ftp://h/v2/",
            "not-a-url",
            "https://",
        ] {
            assert!(require_secure_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn diagnostics_carry_no_urls_or_secrets() {
        let registry = FixtureRegistry::serve(HashMap::new());
        let host = registry.host().to_owned();
        drop(registry);
        let error = pull_bundle_insecure(&pull_reference(&host), &fast_options())
            .expect_err("closed port must fail");
        let message = error.to_string();
        assert!(
            !message.contains(&host),
            "diagnostics must not echo the host: {message}"
        );
        assert!(
            !message.contains("sha256:"),
            "diagnostics must not echo digests: {message}"
        );
    }
}
