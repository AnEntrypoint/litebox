use std::borrow::Cow;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciDescriptor;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};

const ACCEPTED_MEDIA_TYPES: [&str; 3] = [
    oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
    oci_client::manifest::IMAGE_LAYER_MEDIA_TYPE,
    oci_client::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
];

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
}

impl ImageConfig {
    pub fn command(&self) -> Vec<String> {
        let mut command = self.entrypoint.clone().unwrap_or_default();
        command.extend(self.cmd.clone().unwrap_or_default());
        command
    }

    pub fn environment(&self) -> Vec<String> {
        self.env
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.contains('='))
            .collect()
    }
}

pub struct PulledLayers {
    pub layers: Vec<Cow<'static, [u8]>>,
}

pub struct RawImage {
    pub layers: Vec<Cow<'static, [u8]>>,
    pub resolved_layers_json: String,
    pub config: ImageConfig,
}

fn parse_reference(image_ref: &str) -> anyhow::Result<Reference> {
    anyhow::ensure!(!image_ref.is_empty(), "empty image reference");
    let (name_and_tag, digest) = match image_ref.split_once('@') {
        Some((head, digest)) => (head, Some(digest.to_owned())),
        None => (image_ref, None),
    };
    let last_segment_start = name_and_tag.rfind('/').map_or(0, |at| at + 1);
    let (name, tag) = match name_and_tag[last_segment_start..].rfind(':') {
        Some(at) => (
            &name_and_tag[..last_segment_start + at],
            Some(name_and_tag[last_segment_start + at + 1..].to_owned()),
        ),
        None => (name_and_tag, None),
    };
    anyhow::ensure!(!name.is_empty(), "empty repository name");
    let (mut registry, mut repository) = match name.split_once('/') {
        Some((left, right)) if left.contains('.') || left.contains(':') || left == "localhost" => {
            (left.to_owned(), right.to_owned())
        }
        _ => ("docker.io".to_owned(), name.to_owned()),
    };
    if registry == "index.docker.io" {
        registry = "docker.io".to_owned();
    }
    if registry == "docker.io" && !repository.contains('/') {
        repository = format!("library/{repository}");
    }
    Ok(match (tag, digest) {
        (Some(tag), Some(digest)) => {
            Reference::with_tag_and_digest(registry, repository, tag, digest)
        }
        (Some(tag), None) => Reference::with_tag(registry, repository, tag),
        (None, Some(digest)) => Reference::with_digest(registry, repository, digest),
        (None, None) => Reference::with_tag(registry, repository, "latest".to_owned()),
    })
}

fn host_image_arch() -> oci_spec::image::Arch {
    if cfg!(target_arch = "aarch64") {
        oci_spec::image::Arch::ARM64
    } else {
        oci_spec::image::Arch::Amd64
    }
}

fn client_config() -> ClientConfig {
    ClientConfig {
        protocol: ClientProtocol::Https,
        platform_resolver: Some(Box::new(|entries| {
            entries
                .iter()
                .find(|entry| {
                    entry.platform.as_ref().is_some_and(|platform| {
                        platform.os == oci_spec::image::Os::Linux
                            && platform.architecture == host_image_arch()
                    })
                })
                .map(|entry| entry.digest.clone())
        })),
        connect_timeout: Some(Duration::from_secs(10)),
        read_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    }
}

fn strings(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value.and_then(|value| value.as_array()).map(|entries| {
        entries
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect()
    })
}

fn parse_image_config(config_json: &str) -> anyhow::Result<ImageConfig> {
    let parsed: serde_json::Value =
        serde_json::from_str(config_json).context("invalid OCI image config JSON")?;
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let config = parsed.get("config").unwrap_or(&empty);
    Ok(ImageConfig {
        entrypoint: strings(config.get("Entrypoint")),
        cmd: strings(config.get("Cmd")),
        env: strings(config.get("Env")),
        working_dir: config
            .get("WorkingDir")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

pub mod cache {
    use super::*;

    pub const CACHE_DIR: &str = ".litebox-cache";

    fn safe(image_ref: &str) -> String {
        image_ref
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn ref_path(image_ref: &str, suffix: &str) -> PathBuf {
        Path::new(CACHE_DIR).join(format!("ref_{}{}", safe(image_ref), suffix))
    }

    fn store(path: PathBuf, bytes: &[u8]) {
        if std::fs::create_dir_all(CACHE_DIR).is_err() {
            return;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    pub fn store_resolved_layers(image_ref: &str, resolved_layers_json: &str) {
        store(
            ref_path(image_ref, ".layers.json"),
            resolved_layers_json.as_bytes(),
        );
    }

    pub fn load_resolved_layers(image_ref: &str) -> Option<String> {
        std::fs::read_to_string(ref_path(image_ref, ".layers.json"))
            .ok()
            .filter(|json| !json.trim().is_empty())
    }

    pub fn store_image_config(image_ref: &str, config: &ImageConfig) {
        if let Ok(json) = serde_json::to_vec(config) {
            store(ref_path(image_ref, ".config.json"), &json);
        }
    }

    pub fn load_image_config(image_ref: &str) -> Option<ImageConfig> {
        let bytes = std::fs::read(ref_path(image_ref, ".config.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn layer_path(digest: &str) -> PathBuf {
        Path::new(CACHE_DIR).join(format!("raw_{}.tar", digest.replace(':', "_")))
    }

    pub fn temp_path(tag: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(CACHE_DIR)
            .with_context(|| format!("failed to create {CACHE_DIR}"))?;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Ok(Path::new(CACHE_DIR).join(format!(".tmp-{tag}-{}-{unique}", std::process::id())))
    }

    pub fn read_cached_layer(digest: &str, verbose: bool) -> Option<Cow<'static, [u8]>> {
        let path = layer_path(digest);
        let file = std::fs::File::open(&path).ok()?;
        if file.metadata().ok()?.len() == 0 {
            return None;
        }
        let mapped = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mapped));
        if verbose {
            eprintln!("[cache] HIT {digest}: {} bytes", leaked.len());
        }
        Some(Cow::Borrowed(&leaked[..]))
    }

    pub fn publish_layer(
        tmp_path: &Path,
        digest: &str,
        verbose: bool,
    ) -> anyhow::Result<Cow<'static, [u8]>> {
        let final_path = layer_path(digest);
        if std::fs::rename(tmp_path, &final_path).is_err() {
            if !final_path.exists() {
                bail!("failed to publish cached layer for {digest}");
            }
        }
        let file = std::fs::File::open(&final_path)
            .with_context(|| format!("failed to open {}", final_path.display()))?;
        let mapped = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("failed to mmap {}", final_path.display()))?;
        let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mapped));
        if verbose {
            eprintln!("[cache] STORE {digest}: {} bytes", leaked.len());
        }
        Ok(Cow::Borrowed(&leaked[..]))
    }
}

pub fn image_config(image_ref: &str) -> ImageConfig {
    cache::load_image_config(image_ref).unwrap_or_default()
}

pub fn image_env(image_ref: &str) -> Vec<String> {
    if std::env::var_os("LITEBOX_IMAGE_ENV_OFF").is_some_and(|v| v != "0") {
        return Vec::new();
    }
    image_config(image_ref).environment()
}

pub fn pull_layers_in_memory(image_ref: &str, verbose: bool) -> anyhow::Result<PulledLayers> {
    Ok(PulledLayers {
        layers: pull_raw_image(image_ref, verbose)?.layers,
    })
}

pub fn pull_layers_in_memory_with_resolved_digests(
    image_ref: &str,
    verbose: bool,
) -> anyhow::Result<(PulledLayers, String)> {
    let image = pull_raw_image(image_ref, verbose)?;
    Ok((
        PulledLayers {
            layers: image.layers,
        },
        image.resolved_layers_json,
    ))
}

pub fn pull_raw_image(image_ref: &str, verbose: bool) -> anyhow::Result<RawImage> {
    if std::env::var_os("LITEBOX_OCI_USE_LAST_RESOLVED").is_some_and(|v| v != "0") {
        let resolved_layers_json = cache::load_resolved_layers(image_ref).ok_or_else(|| {
            anyhow!(
                "LITEBOX_OCI_USE_LAST_RESOLVED is set but no layer list is recorded for {image_ref}"
            )
        })?;
        eprintln!("using the recorded layer list for {image_ref}, not contacting the registry");
        let layers = pull_layers_with_known_digests(image_ref, &resolved_layers_json, verbose)?;
        return Ok(RawImage {
            layers: layers.layers,
            resolved_layers_json,
            config: image_config(image_ref),
        });
    }
    match pull_raw_impl(image_ref, None, verbose) {
        Ok(image) => {
            cache::store_resolved_layers(image_ref, &image.resolved_layers_json);
            cache::store_image_config(image_ref, &image.config);
            Ok(image)
        }
        Err(err) => {
            let Some(resolved_layers_json) = cache::load_resolved_layers(image_ref) else {
                return Err(err);
            };
            eprintln!(
                "warning: could not resolve {image_ref} ({err:#}); using the recorded layer list"
            );
            let layers = pull_layers_with_known_digests(image_ref, &resolved_layers_json, verbose)?;
            Ok(RawImage {
                layers: layers.layers,
                resolved_layers_json,
                config: image_config(image_ref),
            })
        }
    }
}

pub fn pull_layers_with_known_digests(
    image_ref: &str,
    layers_json: &str,
    verbose: bool,
) -> anyhow::Result<PulledLayers> {
    let known: Vec<OciDescriptor> =
        serde_json::from_str(layers_json).context("invalid recorded OCI layer digest list")?;
    let mut cached = Vec::with_capacity(known.len());
    for descriptor in &known {
        match cache::read_cached_layer(&descriptor.digest, verbose) {
            Some(layer) => cached.push(layer),
            None => break,
        }
    }
    if cached.len() == known.len() {
        return Ok(PulledLayers { layers: cached });
    }
    Ok(PulledLayers {
        layers: pull_raw_impl(image_ref, Some(known), verbose)?.layers,
    })
}

fn pull_raw_impl(
    image_ref: &str,
    known_layers: Option<Vec<OciDescriptor>>,
    verbose: bool,
) -> anyhow::Result<RawImage> {
    let reference = parse_reference(image_ref)
        .with_context(|| format!("invalid image reference {image_ref}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create the OCI client runtime")?;

    runtime.block_on(async {
        let client = Client::new(client_config());
        let auth = RegistryAuth::Anonymous;
        let (descriptors, config) = if let Some(known) = known_layers {
            (known, image_config(image_ref))
        } else {
            if verbose {
                eprintln!("resolving {reference}");
            }
            let mut last_error = None;
            let mut fetched = None;
            for attempt in 1..=3 {
                match tokio::time::timeout(
                    Duration::from_secs(45),
                    client.pull_manifest_and_config(&reference, &auth),
                )
                .await
                {
                    Ok(Ok(found)) => {
                        fetched = Some(found);
                        break;
                    }
                    Ok(Err(err)) => last_error = Some(anyhow::Error::new(err)),
                    Err(_) => last_error = Some(anyhow!("manifest fetch timed out")),
                }
                eprintln!("manifest fetch attempt {attempt}/3 failed, retrying");
            }
            let (manifest, _digest, config_json) = fetched.ok_or_else(|| {
                last_error
                    .unwrap_or_else(|| anyhow!("no manifest fetch attempted"))
                    .context(format!("failed to pull manifest for {reference}"))
            })?;
            (manifest.layers, parse_image_config(&config_json)?)
        };

        let resolved_layers_json = serde_json::to_string(&descriptors)
            .context("failed to serialize the resolved OCI layer list")?;
        let mut layers = Vec::with_capacity(descriptors.len());
        for (index, descriptor) in descriptors.iter().enumerate() {
            if !ACCEPTED_MEDIA_TYPES.contains(&descriptor.media_type.as_str()) {
                bail!("unsupported layer media type: {}", descriptor.media_type);
            }
            if let Some(cached) = cache::read_cached_layer(&descriptor.digest, verbose) {
                layers.push(cached);
                continue;
            }
            if verbose {
                eprintln!(
                    "pulling layer {}/{} {}",
                    index + 1,
                    descriptors.len(),
                    descriptor.digest
                );
            }
            let compressed = cache::temp_path("pull")?;
            pull_layer_resumable(&client, &reference, descriptor, &compressed, verbose).await?;
            let is_gzip = descriptor.media_type.contains("gzip") || {
                let mut magic = [0u8; 2];
                std::fs::File::open(&compressed)
                    .and_then(|mut file| file.read_exact(&mut magic))
                    .is_ok()
                    && magic == [0x1f, 0x8b]
            };
            let plain = if is_gzip {
                let tmp = cache::temp_path("decompress")?;
                {
                    let input = std::fs::File::open(&compressed)
                        .with_context(|| format!("failed to reopen {}", compressed.display()))?;
                    let mut decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(input));
                    let mut writer = std::io::BufWriter::new(
                        std::fs::File::create(&tmp)
                            .with_context(|| format!("failed to create {}", tmp.display()))?,
                    );
                    std::io::copy(&mut decoder, &mut writer)
                        .with_context(|| format!("failed to decompress {}", descriptor.digest))?;
                    writer.flush()?;
                }
                let _ = std::fs::remove_file(&compressed);
                tmp
            } else {
                compressed
            };
            layers.push(cache::publish_layer(&plain, &descriptor.digest, verbose)?);
        }
        Ok(RawImage {
            layers,
            resolved_layers_json,
            config,
        })
    })
}

async fn pull_layer_resumable(
    client: &Client,
    reference: &Reference,
    layer: &OciDescriptor,
    path: &Path,
    verbose: bool,
) -> anyhow::Result<()> {
    use futures_util::StreamExt as _;
    use sha2::Digest as _;
    use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

    const STALL_TIMEOUT: Duration = Duration::from_secs(45);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
    const MAX_ATTEMPTS: u32 = 60;

    let total = u64::try_from(layer.size).unwrap_or(0);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut offset: u64 = 0;
    let started = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();
    let mut complete = false;

    for attempt in 1..=MAX_ATTEMPTS {
        let requested = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client.pull_blob_stream_partial(reference, layer, offset, None),
        )
        .await;
        let response = match requested {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                eprintln!("layer download attempt {attempt} failed at {offset} bytes: {err}");
                continue;
            }
            Err(_) => {
                eprintln!("layer download attempt {attempt} timed out at {offset} bytes");
                continue;
            }
        };
        let mut stream = match response {
            oci_client::client::BlobResponse::Partial(stream) => stream,
            oci_client::client::BlobResponse::Full(stream) => {
                if offset != 0 {
                    file.set_len(0).await?;
                    file.seek(std::io::SeekFrom::Start(0)).await?;
                    offset = 0;
                }
                stream
            }
        };
        loop {
            match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    file.write_all(&chunk).await?;
                    offset += chunk.len() as u64;
                    if verbose && last_report.elapsed() >= Duration::from_secs(10) {
                        last_report = std::time::Instant::now();
                        eprintln!("layer download: {} / {} MB", offset >> 20, total >> 20);
                    }
                }
                Ok(Some(Err(err))) => {
                    eprintln!(
                        "layer download attempt {attempt} interrupted at {offset} bytes: {err}"
                    );
                    break;
                }
                Ok(None) => {
                    if total == 0 || offset >= total {
                        complete = true;
                    }
                    break;
                }
                Err(_) => {
                    eprintln!("layer download attempt {attempt} stalled at {offset} bytes");
                    break;
                }
            }
        }
        if complete || (total != 0 && offset >= total) {
            complete = true;
            break;
        }
    }
    file.flush().await?;
    drop(file);
    anyhow::ensure!(
        complete,
        "layer download did not complete after {MAX_ATTEMPTS} attempts ({offset} of {total} bytes)"
    );
    if total != 0 {
        anyhow::ensure!(
            offset == total,
            "layer download size mismatch: got {offset} bytes, expected {total}"
        );
    }
    if let Some(expected) = layer.digest.strip_prefix("sha256:") {
        let mut hasher = sha2::Sha256::new();
        let mut reader = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        anyhow::ensure!(
            actual == expected,
            "layer digest mismatch: expected sha256:{expected}, got sha256:{actual}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_reference_keeps_repository_and_digest() {
        let reference = parse_reference(
            "docker.io/linuxserver/webtop@sha256:8bb9f1ac0bcf64d6e0be56bcd65dbfa985ee8c2b09c16820ddae29d3e44aeaa3",
        )
        .unwrap();
        assert_eq!(reference.registry(), "docker.io");
        assert_eq!(reference.repository(), "linuxserver/webtop");
        assert!(reference.digest().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn short_reference_defaults_to_docker_hub_library() {
        let reference = parse_reference("alpine:3.20").unwrap();
        assert_eq!(reference.registry(), "docker.io");
        assert_eq!(reference.repository(), "library/alpine");
        assert_eq!(reference.tag(), Some("3.20"));
    }

    #[test]
    fn image_config_exposes_its_own_entrypoint_and_env() {
        let json = r#"{"config":{"Entrypoint":["/init"],"Cmd":null,"Env":["PATH=/bin","HOME=/config"],"WorkingDir":"/"}}"#;
        let config = parse_image_config(json).unwrap();
        assert_eq!(config.command(), vec!["/init"]);
        assert_eq!(config.environment(), vec!["PATH=/bin", "HOME=/config"]);
        assert_eq!(config.working_dir.as_deref(), Some("/"));
    }

    #[test]
    fn cache_names_raw_layers_separately_from_rewritten_ones() {
        let digest = "sha256:deadbeef";
        assert!(cache::CACHE_DIR == ".litebox-cache");
        let path = std::path::Path::new(cache::CACHE_DIR)
            .join(format!("raw_{}.tar", digest.replace(':', "_")));
        assert_eq!(
            path.to_string_lossy(),
            ".litebox-cache/raw_sha256_deadbeef.tar"
        );
    }
}
