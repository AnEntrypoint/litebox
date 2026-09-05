// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! OCI image pulling and rootfs extraction.
//!
//! Pulls an OCI container image from a registry (e.g., Docker Hub, GHCR),
//! extracts its filesystem layers into a temporary rootfs directory, then
//! walks the rootfs to discover all ELF files for syscall rewriting.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::config::ConfigFile;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

/// Parsed OCI image execution configuration (ENTRYPOINT, CMD, ENV, WORKDIR).
#[derive(Debug, Default)]
pub struct ImageConfig {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
}

/// Result of pulling and extracting an OCI image.
pub struct ExtractedImage {
    /// Temporary directory holding the extracted rootfs.
    /// Cleaned up when this struct is dropped.
    pub tempdir: tempfile::TempDir,
    /// Path to the rootfs inside the temp directory.
    pub rootfs_path: PathBuf,
    /// Parsed image config (ENTRYPOINT, CMD, ENV, WORKDIR).
    pub config: ImageConfig,
    /// Raw OCI image config JSON blob (the full config descriptor data).
    pub config_json: Vec<u8>,
    /// Symlink map from layer extraction: maps relative paths inside the
    /// rootfs to their (Unix-style) link targets for cross-platform resolution.
    pub symlink_map: HashMap<PathBuf, PathBuf>,
    /// Unix permission modes captured from tar headers during extraction.
    /// Keyed by relative path inside the rootfs. Used instead of querying
    /// filesystem metadata, which loses Unix mode bits on non-Unix hosts.
    pub permissions: HashMap<PathBuf, u32>,
}

/// Result of scanning an extracted rootfs for files to package.
pub struct RootfsFileMap {
    /// Map from host path (inside the extracted rootfs) to the tar path
    /// (the path the file should appear at inside the output tar).
    /// Files with executable permission bits are candidates for rewriting.
    pub files: BTreeMap<PathBuf, RootfsEntry>,
}

/// A single file discovered in the rootfs.
pub struct RootfsEntry {
    /// Path inside the tar archive (relative, no leading `/`).
    pub tar_path: String,
    /// Host path to read the file data from.
    /// For regular files this equals the map key; for symlinks this is the
    /// resolved target path (which may differ from the map key).
    pub read_path: PathBuf,
    /// Whether the file has executable permission bits set.
    pub is_executable: bool,
    /// Unix permission mode (lower 12 bits).
    pub mode: u32,
    /// When `Some`, this entry is a SYMLINK whose target is this string, and it should be
    /// emitted as a real symlink tar entry rather than a copy of `read_path`'s contents.
    ///
    /// A container image is largely DEFINED by its symlink structure
    /// (`/bin/ls -> /bin/busybox`, `/lib64 -> /lib`); resolving links away yields a rootfs
    /// that is no longer the image, and duplicates hugely -- one real layer held 295 copies
    /// of the same 804 KB busybox, 226 MB. The runtime tar filesystem has supported symlinks
    /// for some time (`litebox/src/fs/tar_ro.rs`: `IndexedChild::Symlink`, `read_link`), so
    /// the flattening is no longer necessary.
    pub symlink_target: Option<String>,
}

/// Pull an OCI image from a registry and extract its layers into a temp directory.
///
/// Supports standard image references like:
/// - `docker.io/library/alpine:latest`
/// - `alpine:latest` (defaults to docker.io/library/)
/// - `ghcr.io/org/repo:tag`
///
/// Layers are applied in order (bottom-up), handling whiteout files for
/// layer deletions per the OCI image spec.
///
/// # Authentication
///
/// Currently only anonymous (unauthenticated) pulls are supported. Private
/// registries or images that require credentials will fail with an
/// authorization error from the registry.
pub fn pull_and_extract(image_ref: &str, verbose: bool) -> anyhow::Result<ExtractedImage> {
    // Parse the image reference
    let reference: Reference = image_ref
        .parse()
        .with_context(|| format!("invalid OCI image reference: {image_ref}"))?;

    if verbose {
        eprintln!("Pulling image: {reference}");
    }

    // Create async runtime for the OCI client (which is async-based)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    #[allow(
        clippy::items_after_statements,
        reason = "kept next to its only caller below"
    )]
    /// The OCI architecture name for the host LiteBox is running on.
    fn host_image_arch() -> oci_spec::image::Arch {
        if cfg!(target_arch = "aarch64") {
            oci_spec::image::Arch::ARM64
        } else {
            oci_spec::image::Arch::Amd64
        }
    }

    // Create temp directory for extraction
    let tempdir = tempfile::tempdir().context("failed to create temporary directory for rootfs")?;
    let rootfs_path = tempdir.path().join("rootfs");
    std::fs::create_dir_all(&rootfs_path).context("failed to create rootfs directory")?;

    let mut symlinks: Vec<DeferredSymlink> = Vec::new();
    let mut permissions: HashMap<PathBuf, u32> = HashMap::new();

    let config_data = rt.block_on(async {
        let client_config = ClientConfig {
            protocol: ClientProtocol::Https,
            // Pull the Linux image whose architecture matches the host. LiteBox
            // runs guest instructions natively rather than emulating them, so a
            // guest of any other architecture could not execute here -- pulling
            // one would only defer the failure to run time.
            platform_resolver: Some(Box::new(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry.platform.as_ref().is_some_and(|p| {
                            p.os == oci_spec::image::Os::Linux
                                && p.architecture == host_image_arch()
                        })
                    })
                    .map(|e| e.digest.clone())
            })),
            ..Default::default()
        };
        let client = Client::new(client_config);

        // Authenticate (anonymous for public images)
        let auth = RegistryAuth::Anonymous;

        if verbose {
            eprintln!("  Fetching manifest...");
        }

        // Fetch only the manifest up front; layers are pulled and extracted
        // one at a time below so at most one (de)compressed layer's bytes
        // are held in memory at once. Pulling every layer into memory
        // simultaneously (the crate's own `Client::pull`) is what a real
        // multi-GB Arch-based image (`linuxserver/webtop:arch-xfce`) was
        // observed to OOM the host on -- see the packaging notes for this fix.
        let (manifest, _digest) = client
            .pull_image_manifest(&reference, &auth)
            .await
            .with_context(|| format!("failed to pull manifest for {reference}"))?;

        let mut config_bytes: Vec<u8> = Vec::new();
        client
            .pull_blob(&reference, &manifest.config, &mut config_bytes)
            .await
            .with_context(|| format!("failed to pull image config for {reference}"))?;
        let config = oci_client::client::Config::new(
            config_bytes,
            manifest.config.media_type.clone(),
            manifest.annotations.clone(),
        );

        if verbose {
            eprintln!("  Pulled manifest ({} layer(s))", manifest.layers.len());
        }

        let accepted_media_types = [
            oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
            oci_client::manifest::IMAGE_LAYER_MEDIA_TYPE,
            oci_client::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
        ];
        let num_layers = manifest.layers.len();
        for (i, layer_desc) in manifest.layers.iter().enumerate() {
            if !accepted_media_types.contains(&layer_desc.media_type.as_str()) {
                anyhow::bail!("unsupported layer media type: {}", layer_desc.media_type);
            }

            if verbose {
                eprintln!("  Pulling layer {}/{}...", i + 1, num_layers);
            }

            let mut layer_data: Vec<u8> = Vec::new();
            client
                .pull_blob(&reference, layer_desc, &mut layer_data)
                .await
                .with_context(|| format!("failed to pull layer {}", i + 1))?;

            if verbose {
                eprintln!(
                    "  Extracting layer {}/{} ({} bytes)...",
                    i + 1,
                    num_layers,
                    layer_data.len()
                );
            }
            extract_layer(
                &layer_data,
                &layer_desc.media_type,
                &rootfs_path,
                &mut symlinks,
                &mut permissions,
            )
            .with_context(|| format!("failed to extract layer {}", i + 1))?;
        }

        Ok::<_, anyhow::Error>(config)
    })?;

    // Build the symlink map once for O(1) lookup during resolution.
    let symlink_map: HashMap<PathBuf, PathBuf> = symlinks
        .iter()
        .map(|s| (s.rel_path.clone(), s.link_target.clone()))
        .collect();

    // Materialize symlinks cross-platform: resolve chains through the in-memory
    // map and copy target files (or create directories) instead of OS symlinks.
    if verbose {
        eprintln!("  Resolving {} symlinks...", symlinks.len());
    }
    materialize_symlinks(&symlink_map, &rootfs_path, &mut permissions, verbose)?;

    if verbose {
        eprintln!("  Rootfs extracted to {}", rootfs_path.display());
    }

    // Save the raw config JSON before parsing (try_from consumes it).
    let config_json = config_data.data.to_vec();

    // Parse image config for ENTRYPOINT, CMD, ENV, WORKDIR.
    let config = match ConfigFile::try_from(config_data) {
        Ok(cf) => {
            let exec_config = cf.config.as_ref();
            let ic = ImageConfig {
                entrypoint: exec_config.and_then(|c| c.entrypoint.clone()),
                cmd: exec_config.and_then(|c| c.cmd.clone()),
                env: exec_config.and_then(|c| c.env.clone()),
                working_dir: exec_config.and_then(|c| c.working_dir.clone()),
            };
            if verbose {
                eprintln!(
                    "  Image config: ENTRYPOINT={:?} CMD={:?} WORKDIR={:?} ENV=({} vars)",
                    ic.entrypoint,
                    ic.cmd,
                    ic.working_dir,
                    ic.env.as_ref().map_or(0, Vec::len)
                );
            }
            ic
        }
        Err(e) => {
            eprintln!(
                "warning: failed to parse image config: {e}; config_and_run.sh will not be generated"
            );
            ImageConfig::default()
        }
    };

    Ok(ExtractedImage {
        tempdir,
        rootfs_path,
        config,
        config_json,
        symlink_map,
        permissions,
    })
}

/// Result of pulling an OCI image's layers directly into memory, with NO filesystem writes at
/// all -- not even a temp directory. Each entry in `layers` is one OCI layer's tar bytes,
/// decompressed if needed, in bottom-to-top order exactly as the manifest lists them, ready to
/// hand straight to `litebox::fs::tar_ro::TarRo::from_layers` for whiteout-aware in-memory
/// merging at guest-boot time. This is the runtime-loading counterpart to
/// [`pull_and_extract`]: same registry pull, same one-layer-at-a-time streaming (never buffering
/// every layer simultaneously), but stopping at "bytes in memory" instead of extracting onto a
/// real host rootfs directory.
pub struct PulledLayers {
    /// One entry per OCI layer, rewritten tar bytes, bottom-to-top order. EVERY layer here is
    /// `Cow::Borrowed` over a leaked memory-map (mirrors
    /// `litebox_runner_linux_on_windows_userland`'s `mmapped_file` for `--initial-files`, so every
    /// concurrent process reading the same cached layer shares its physical pages via the OS page
    /// cache) -- both a cache-HIT layer (served directly from the on-disk cache, see [`cache`])
    /// and a cache-MISS layer (rewritten this run, written to the cache, then immediately
    /// re-opened as an mmap via `cache::write_and_map_cached_layer` so it becomes just as
    /// page-cache-evictable as a hit, instead of staying a resident heap `Vec` for the guest's
    /// whole lifetime). Only on the rare failure to write/mmap the cache file does a layer fall
    /// back to `Cow::Owned` (heap-resident) as a correctness-preserving degradation.
    pub layers: Vec<Cow<'static, [u8]>>,
    /// Parsed image execution config (ENTRYPOINT, CMD, ENV, WORKDIR).
    pub config: ImageConfig,
    /// Raw OCI image config JSON blob.
    pub config_json: Vec<u8>,
}

/// On-disk cache of rewritten OCI layers, keyed by `(layer_digest, rewriter_version)`.
///
/// # Why cache the REWRITTEN bytes, not the raw pulled layer
///
/// The expensive, repeatable-per-boot costs are the network pull, gzip decompression, AND the
/// ELF rewrite -- caching only the raw pulled bytes would still pay the rewrite cost (the
/// dominant compute cost, though not the dominant wall-clock cost against a slow network) on
/// every boot. Caching the rewritten output skips all three.
///
/// # Why the cache key is `(layer_digest, rewriter_version)`, not `layer_digest` alone
///
/// `layer_digest` (a `sha256:...` string from the manifest) is a real, content-addressed digest
/// of the layer's RAW content -- a correct and natural key for "is this the same input". But the
/// cached ARTIFACT is `litebox_syscall_rewriter`'s output for that input, which also depends on
/// the exact rewriting logic in effect when the cache entry was written. If that logic ever
/// changes (a bug fix, a newly handled instruction pattern, a trampoline layout change), an old
/// cache entry keyed on `layer_digest` alone would be silently stale and WRONG: the guest would
/// run old, incorrect rewritten code while every other part of the system believes the cache is
/// authoritative. Folding `litebox_syscall_rewriter::REWRITER_CACHE_VERSION` into the key makes a
/// rewriter-logic change (bumping that constant) invalidate every existing cache entry at once,
/// with no silent-staleness window -- see that constant's own doc comment for the bump discipline.
///
/// # Why a project-relative `.litebox-cache/` directory
///
/// This project has no existing durable-but-not-source-controlled artifact directory convention
/// beyond the harness's own `.gm/` (unrelated tooling state, not a place for build artifacts) --
/// no `dirs`/`directories` crate dependency exists anywhere in the workspace to reach a
/// platform user-cache directory, and introducing one purely for this cache would be a bigger
/// footprint than the problem needs. A project-relative, gitignored directory (matching this
/// repo's existing precedent of gitignored local artifact directories like `target-myfork/`) is
/// simple, requires no new dependency, and is trivially discoverable/clearable by a developer
/// (`rm -rf .litebox-cache`).
pub mod cache {
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};

    use anyhow::Context;

    /// Directory holding cached rewritten OCI layers, relative to the current working directory.
    /// Gitignored (see `.gitignore`'s "Local tooling state" section).
    pub(crate) const CACHE_DIR: &str = ".litebox-cache";

    /// Build the cache file path for a given layer digest (e.g. `sha256:abcd...`) and rewriter
    /// version. The digest's `:` is replaced with `_` since `:` is a reserved character in
    /// Windows paths (valid only as the drive-letter separator) -- same constraint already
    /// documented on this module's sibling `is_excluded_path` for pacman's local-install-db
    /// paths.
    fn cache_path(layer_digest: &str, rewriter_version: u32) -> PathBuf {
        let safe_digest = layer_digest.replace(':', "_");
        Path::new(CACHE_DIR).join(format!("{safe_digest}_v{rewriter_version}.tar"))
    }

    /// Look up a cached rewritten layer for `(layer_digest, rewriter_version)`.
    ///
    /// Returns `Ok(None)` on ANY doubt about validity -- missing file, zero-byte file (a crashed
    /// writer's leftover, since a real writer never finalizes an empty file this way), or any I/O
    /// error reading it -- rather than risk serving stale/corrupt content. A cache implementation
    /// that silently serves wrong data would be worse than no cache at all (see this module's own
    /// top-level doc comment).
    ///
    /// The returned bytes are memory-mapped, not heap-copied (mirrors
    /// `litebox_runner_linux_on_windows_userland`'s `mmapped_file` helper for `--initial-files`),
    /// so every concurrent runner process reading the same cached layer shares its physical pages
    /// via the OS page cache instead of each holding a private copy. The mapping is intentionally
    /// leaked (`Box::leak`) to obtain the `'static` lifetime `PulledLayers::layers` requires --
    /// this matches the process-lifetime leak `mmapped_file` already performs for the
    /// `--initial-files` tar, and is bounded (once per distinct layer actually read this process
    /// run), not unbounded.
    pub fn read_cached_layer(
        layer_digest: &str,
        rewriter_version: u32,
        verbose: bool,
    ) -> Option<Cow<'static, [u8]>> {
        let path = cache_path(layer_digest, rewriter_version);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if verbose {
                    eprintln!("  [cache] MISS {layer_digest} (v{rewriter_version}): not cached");
                }
                return None;
            }
            Err(e) => {
                if verbose {
                    eprintln!(
                        "  [cache] MISS {layer_digest} (v{rewriter_version}): failed to open cache file {}: {e}",
                        path.display()
                    );
                }
                return None;
            }
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                if verbose {
                    eprintln!(
                        "  [cache] MISS {layer_digest} (v{rewriter_version}): failed to stat cache file: {e}"
                    );
                }
                return None;
            }
        };
        if meta.len() == 0 {
            // A zero-byte file can only be a crashed/killed writer's leftover (see
            // `write_cached_layer`'s atomic-rename discipline below -- a properly finished write
            // is never empty for a real tar). Treat as a miss, and best-effort clean it up so a
            // future run isn't confused by it either.
            if verbose {
                eprintln!(
                    "  [cache] MISS {layer_digest} (v{rewriter_version}): cache file is empty (stale partial write?)"
                );
            }
            let _ = std::fs::remove_file(&path);
            return None;
        }
        // SAFETY: mirrors `litebox_runner_linux_on_windows_userland::mmapped_file` -- we assume
        // the cache file is not mutated externally while mapped. Cache files are written via the
        // atomic write-temp-then-rename pattern in `write_cached_layer`, so any process that has
        // this file open for reading always sees either a complete prior version (if the file was
        // replaced, rename atomically retargets the directory entry, leaving this mapping's own
        // inode's contents untouched) or nothing (if this is the first read after a fresh write).
        let mmap = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(m) => m,
            Err(e) => {
                if verbose {
                    eprintln!(
                        "  [cache] MISS {layer_digest} (v{rewriter_version}): failed to mmap cache file: {e}"
                    );
                }
                return None;
            }
        };
        if verbose {
            eprintln!(
                "  [cache] HIT {layer_digest} (v{rewriter_version}): {} bytes from {}",
                mmap.len(),
                path.display()
            );
        }
        // Leak to get the 'static lifetime PulledLayers::layers requires -- see this function's
        // doc comment.
        let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mmap));
        Some(Cow::Borrowed(&leaked[..]))
    }

    /// Write a freshly rewritten layer to the cache for `(layer_digest, rewriter_version)`,
    /// atomically: write to a temp file in the SAME directory, then rename into place. Rename is
    /// atomic on the same filesystem, so a concurrent reader (a second runner process pulling the
    /// same image at the same time -- plausible in this project's actual usage) can never observe
    /// a partially-written cache file: it either doesn't exist yet, or is fully present.
    ///
    /// Best-effort: any failure here (e.g. read-only filesystem, disk full) is logged
    /// (verbose-gated) and swallowed rather than propagated -- failing to populate the cache must
    /// never fail the boot that produced the data, since the freshly rewritten bytes are already
    /// available to the caller regardless of whether they get persisted.
    pub fn write_cached_layer(layer_digest: &str, rewriter_version: u32, data: &[u8], verbose: bool) {
        let final_path = cache_path(layer_digest, rewriter_version);
        if let Err(e) = write_cached_layer_inner(&final_path, data) {
            if verbose {
                eprintln!(
                    "  [cache] failed to write cache entry for {layer_digest} (v{rewriter_version}): {e:#}"
                );
            }
            return;
        }
        if verbose {
            eprintln!(
                "  [cache] wrote {} bytes to {}",
                data.len(),
                final_path.display()
            );
        }
    }

    /// Write a freshly rewritten layer to the cache, then immediately re-open and mmap the
    /// JUST-WRITTEN file, returning that mmap'd `Cow::Borrowed` slice instead of leaving the
    /// caller holding the original in-memory `Vec<u8>`.
    ///
    /// This exists so a CACHE-MISS run is exactly as memory-cheap as a cache-hit run, immediately,
    /// in the very same process run that produced the rewritten bytes -- not just on a later run.
    /// Without this, `PulledLayers::layers` would hold every cache-miss layer's rewritten bytes as
    /// a real heap `Vec` for the guest's entire lifetime, which is what caused the OOM-kill
    /// observed pulling `linuxserver/webtop:debian-xfce` (17 layers, ~2.6GB decompressed): every
    /// layer's rewritten bytes stayed resident simultaneously, summed across the whole image,
    /// instead of being page-cache-evictable like a cache-hit layer already is.
    ///
    /// On any failure to write or mmap (read-only filesystem, disk full, etc.) this falls back to
    /// `Cow::Owned(data)` -- the caller still gets correct bytes, just not the memory-cheap path
    /// for this one layer. That failure is exactly what `write_cached_layer` already tolerates
    /// (best-effort, never fails the boot), so this must tolerate it too.
    pub fn write_and_map_cached_layer(
        layer_digest: &str,
        rewriter_version: u32,
        data: Vec<u8>,
        verbose: bool,
    ) -> Cow<'static, [u8]> {
        let final_path = cache_path(layer_digest, rewriter_version);
        if let Err(e) = write_cached_layer_inner(&final_path, &data) {
            if verbose {
                eprintln!(
                    "  [cache] failed to write cache entry for {layer_digest} (v{rewriter_version}): {e:#}; keeping in-memory copy"
                );
            }
            return Cow::Owned(data);
        }
        if verbose {
            eprintln!(
                "  [cache] wrote {} bytes to {}",
                data.len(),
                final_path.display()
            );
        }
        // The in-memory Vec is no longer needed once the write succeeded -- drop it before
        // (re-)opening the file so the two copies (heap Vec + mmap) never coexist longer than
        // this brief window.
        let expected_len = data.len();
        drop(data);

        match read_cached_layer(layer_digest, rewriter_version, verbose) {
            Some(mmapped) => mmapped,
            None => {
                // Extremely unlikely (we just wrote this file successfully) but not impossible
                // (e.g. concurrent external deletion). Re-reading from disk to recover the bytes
                // would defeat the purpose of avoiding a second heap copy, and we've already
                // dropped the original -- so this is a hard failure for this layer.
                panic!(
                    "  [cache] wrote {expected_len} bytes for {layer_digest} (v{rewriter_version}) but immediately failed to re-read/mmap them"
                );
            }
        }
    }

    /// Build a fresh, process-and-call-unique temp file path inside the cache directory
    /// (creating the directory if needed), for a caller that wants to write the rewritten bytes
    /// itself (streaming) rather than handing this module an already-built `Vec<u8>`. Pair with
    /// [`finalize_temp_into_cache`] to atomically publish it as the real cache entry.
    pub fn temp_path_in_cache_dir() -> anyhow::Result<PathBuf> {
        let dir = Path::new(CACHE_DIR);
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create cache directory {}", dir.display()))?;
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Ok(dir.join(format!(".tmp-rewrite-{pid}-{unique}")))
    }

    /// Atomically publish a caller-written temp file (see [`temp_path_in_cache_dir`]) as the real
    /// cache entry for `(layer_digest, rewriter_version)`, then immediately re-open and mmap it --
    /// the streaming-output counterpart to [`write_and_map_cached_layer`]. Since the caller
    /// already wrote the rewritten bytes directly to `tmp_path` (rather than building a `Vec<u8>`
    /// and handing it to this module), this is JUST the atomic rename-into-place plus mmap, with
    /// no second copy of the bytes ever created -- the file the rewrite produced BECOMES the
    /// cache file.
    ///
    /// On any failure (rename, reopen, or mmap), removes the leftover temp file best-effort and
    /// returns `Err` so the caller can fall back to an in-memory `Cow::Owned` rewrite, exactly as
    /// `write_and_map_cached_layer` does for its own failure path.
    pub fn finalize_temp_into_cache(
        tmp_path: &Path,
        layer_digest: &str,
        rewriter_version: u32,
        verbose: bool,
    ) -> anyhow::Result<Cow<'static, [u8]>> {
        let final_path = cache_path(layer_digest, rewriter_version);
        let result = (|| -> anyhow::Result<Cow<'static, [u8]>> {
            std::fs::rename(tmp_path, &final_path).with_context(|| {
                format!(
                    "failed to atomically rename {} -> {}",
                    tmp_path.display(),
                    final_path.display()
                )
            })?;
            let file = std::fs::File::open(&final_path).with_context(|| {
                format!("failed to reopen cache file {}", final_path.display())
            })?;
            // SAFETY: mirrors `read_cached_layer` -- this file was just renamed into place from a
            // process-and-call-unique temp path, so no other writer can be mutating it.
            let mmap = unsafe { memmap2::Mmap::map(&file) }
                .with_context(|| format!("failed to mmap cache file {}", final_path.display()))?;
            let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mmap));
            Ok(Cow::Borrowed(&leaked[..]))
        })();

        match &result {
            Ok(mmapped) => {
                if verbose {
                    eprintln!(
                        "  [cache] wrote {} bytes to {} (streamed directly, no in-memory copy)",
                        mmapped.len(),
                        final_path.display()
                    );
                }
            }
            Err(e) => {
                if verbose {
                    eprintln!(
                        "  [cache] failed to finalize streamed cache entry for {layer_digest} (v{rewriter_version}): {e:#}"
                    );
                }
                let _ = std::fs::remove_file(tmp_path);
            }
        }
        result
    }

    fn write_cached_layer_inner(final_path: &Path, data: &[u8]) -> anyhow::Result<()> {
        let dir = final_path
            .parent()
            .context("cache file path has no parent directory")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create cache directory {}", dir.display()))?;

        // Unique temp file name per (process, call) so two concurrent writers for the SAME layer
        // never collide on the temp path itself -- only the final atomic rename needs to be race-
        // safe, which `std::fs::rename` already guarantees on the same filesystem.
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp_path = dir.join(format!(".tmp-{pid}-{unique}"));

        std::fs::write(&tmp_path, data)
            .with_context(|| format!("failed to write temp cache file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, final_path).with_context(|| {
            format!(
                "failed to atomically rename {} -> {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;
        Ok(())
    }
}

/// Pull an OCI image's manifest and every layer's bytes into memory, decompressing gzip layers
/// as they arrive and rewriting each layer's executable ELFs immediately afterward, before
/// moving to the next layer -- the runtime-loading counterpart to [`pull_and_extract`]. Only ONE
/// layer's decompressed bytes and its rewritten counterpart are ever alive at once; layers are
/// never buffered as a batch across the whole image (a real multi-GB image, e.g.
/// `linuxserver/webtop:debian-xfce`, was observed to fail a single ~5GB allocation when the
/// pull step returned every decompressed layer at once and a separate rewrite step then held
/// both the raw and rewritten copies of every layer simultaneously -- fusing pull+decompress+
/// rewrite into one per-layer step, as done here, is the fix).
pub fn pull_layers_in_memory(image_ref: &str, verbose: bool) -> anyhow::Result<PulledLayers> {
    let reference: Reference = image_ref
        .parse()
        .with_context(|| format!("invalid OCI image reference: {image_ref}"))?;

    if verbose {
        eprintln!("Pulling image (runtime, in-memory): {reference}");
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    #[allow(
        clippy::items_after_statements,
        reason = "kept next to its only caller below"
    )]
    fn host_image_arch() -> oci_spec::image::Arch {
        if cfg!(target_arch = "aarch64") {
            oci_spec::image::Arch::ARM64
        } else {
            oci_spec::image::Arch::Amd64
        }
    }

    let (config, layers) = rt.block_on(async {
        let client_config = ClientConfig {
            protocol: ClientProtocol::Https,
            platform_resolver: Some(Box::new(|entries| {
                entries
                    .iter()
                    .find(|entry| {
                        entry.platform.as_ref().is_some_and(|p| {
                            p.os == oci_spec::image::Os::Linux
                                && p.architecture == host_image_arch()
                        })
                    })
                    .map(|e| e.digest.clone())
            })),
            ..Default::default()
        };
        let client = Client::new(client_config);
        let auth = RegistryAuth::Anonymous;

        if verbose {
            eprintln!("  Fetching manifest...");
        }

        let (manifest, _digest) = client
            .pull_image_manifest(&reference, &auth)
            .await
            .with_context(|| format!("failed to pull manifest for {reference}"))?;

        let mut config_bytes: Vec<u8> = Vec::new();
        client
            .pull_blob(&reference, &manifest.config, &mut config_bytes)
            .await
            .with_context(|| format!("failed to pull image config for {reference}"))?;
        let config = oci_client::client::Config::new(
            config_bytes,
            manifest.config.media_type.clone(),
            manifest.annotations.clone(),
        );

        if verbose {
            eprintln!("  Pulled manifest ({} layer(s))", manifest.layers.len());
        }

        let accepted_media_types = [
            oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
            oci_client::manifest::IMAGE_LAYER_MEDIA_TYPE,
            oci_client::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
        ];
        let num_layers = manifest.layers.len();
        let mut layers: Vec<Cow<'static, [u8]>> = Vec::with_capacity(num_layers);
        let rewriter_version = litebox_syscall_rewriter::REWRITER_CACHE_VERSION;
        for (i, layer_desc) in manifest.layers.iter().enumerate() {
            if !accepted_media_types.contains(&layer_desc.media_type.as_str()) {
                anyhow::bail!("unsupported layer media type: {}", layer_desc.media_type);
            }

            // Cache read path: a hit here skips network pull, gzip decompression, AND ELF
            // rewriting entirely for this layer -- see `cache`'s own doc comment for the key
            // scheme and validity discipline.
            if let Some(cached) =
                cache::read_cached_layer(&layer_desc.digest, rewriter_version, verbose)
            {
                if verbose {
                    eprintln!(
                        "  Layer {}/{} served from cache ({} bytes)",
                        i + 1,
                        num_layers,
                        cached.len()
                    );
                }
                layers.push(cached);
                continue;
            }

            if verbose {
                eprintln!("  Pulling layer {}/{}...", i + 1, num_layers);
            }

            // Pull the compressed blob DIRECTLY to a temp file rather than into a `Vec<u8>` --
            // for a real large layer (e.g. `linuxserver/webtop`'s ~500MB-900MB compressed
            // layers), the pulled bytes themselves are a genuine, avoidable in-memory buffer:
            // this codebase already learned (see the decompression step below) that even a
            // "correctly sized" `Vec` is still ordinary, non-page-cache-evictable heap memory
            // for its whole lifetime. Streaming the pull straight to disk means the compressed
            // bytes never exist as a heap allocation at all -- `oci_client::Client::pull_blob`
            // is generic over any `tokio::io::AsyncWrite` target, so a `tokio::fs::File` works
            // exactly like the `Vec<u8>` it replaces, with zero change to the pull call itself.
            let tmp_dir = Path::new(cache::CACHE_DIR);
            std::fs::create_dir_all(tmp_dir)
                .with_context(|| format!("failed to create cache directory {}", tmp_dir.display()))?;
            let pid = std::process::id();
            let pull_unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let compressed_tmp_path = tmp_dir.join(format!(".tmp-pull-{pid}-{pull_unique}"));
            {
                let compressed_tmp_file = tokio::fs::File::create(&compressed_tmp_path)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to create temp pull file {}",
                            compressed_tmp_path.display()
                        )
                    })?;
                let mut compressed_writer = tokio::io::BufWriter::new(compressed_tmp_file);
                client
                    .pull_blob(&reference, layer_desc, &mut compressed_writer)
                    .await
                    .with_context(|| format!("failed to pull layer {}", i + 1))?;
                use tokio::io::AsyncWriteExt as _;
                compressed_writer
                    .flush()
                    .await
                    .with_context(|| format!("failed to flush pulled layer {}", i + 1))?;
            }

            // Sniff gzip-ness from the first few bytes on disk rather than needing the whole
            // blob in memory just to check a magic number.
            let is_gzip = layer_desc.media_type.contains("gzip") || {
                let mut magic = [0u8; 2];
                std::fs::File::open(&compressed_tmp_path)
                    .ok()
                    .and_then(|mut f| {
                        use std::io::Read as _;
                        f.read_exact(&mut magic).ok()
                    });
                magic == [0x1f, 0x8b]
            };

            // Decompress to a temp file and mmap it, rather than holding the full decompressed
            // layer (~2.5GB for a real large layer, e.g. `linuxserver/webtop:debian-xfce`) as a
            // second simultaneous ordinary heap `Vec` alongside `rewrite_layer_elfs`'s own
            // internal output buffer (also ~2.5GB). Before this, BOTH buffers were alive at once
            // for the whole `rewrite_layer_elfs` call -- a genuine ~5GB simultaneous peak of
            // non-evictable heap memory, confirmed live to push the host over its low-memory
            // watchdog threshold even after every other buffer in this pipeline had already been
            // pre-sized or made cache-mmap-backed. An mmap'd temp file is backed by the OS page
            // cache, so the host can evict its pages under memory pressure instead of the
            // allocation being unconditionally resident -- see `cache::write_and_map_cached_layer`
            // and `litebox_runner_linux_on_windows_userland::mmapped_file` for the identical
            // established pattern this reuses.
            enum DecompressedSource {
                Mmapped { mmap: memmap2::Mmap, tmp_path: PathBuf },
                #[allow(dead_code, reason = "both branches now mmap; kept for fallback shape")]
                InMemory(Vec<u8>),
            }

            let source = if is_gzip {
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default();
                let tmp_path = tmp_dir.join(format!(".tmp-decompress-{pid}-{unique}"));

                {
                    let compressed_file = std::fs::File::open(&compressed_tmp_path)
                        .with_context(|| {
                            format!(
                                "failed to reopen pulled layer {}",
                                compressed_tmp_path.display()
                            )
                        })?;
                    let mut decoder =
                        flate2::read::GzDecoder::new(std::io::BufReader::new(compressed_file));
                    let tmp_file = std::fs::File::create(&tmp_path).with_context(|| {
                        format!(
                            "failed to create temp decompression file {}",
                            tmp_path.display()
                        )
                    })?;
                    let mut writer = std::io::BufWriter::new(tmp_file);
                    std::io::copy(&mut decoder, &mut writer)
                        .with_context(|| format!("failed to decompress layer {}", i + 1))?;
                    writer
                        .flush()
                        .with_context(|| format!("failed to flush decompressed layer {}", i + 1))?;
                }
                // The compressed temp file is no longer needed once decompression has finished.
                let _ = std::fs::remove_file(&compressed_tmp_path);

                let tmp_file = std::fs::File::open(&tmp_path).with_context(|| {
                    format!("failed to reopen decompressed temp file {}", tmp_path.display())
                })?;
                // SAFETY: mirrors `cache::read_cached_layer` / `mmapped_file` -- this temp file is
                // exclusive to this process and this call (unique pid+timestamp name), never
                // mutated externally while mapped.
                let mmap = unsafe { memmap2::Mmap::map(&tmp_file) }.with_context(|| {
                    format!("failed to mmap decompressed temp file {}", tmp_path.display())
                })?;
                DecompressedSource::Mmapped { mmap, tmp_path }
            } else {
                // Not gzip -- the pulled bytes on disk already ARE the tar to rewrite, so mmap
                // that file directly rather than reading it into memory at all.
                let tmp_file = std::fs::File::open(&compressed_tmp_path).with_context(|| {
                    format!(
                        "failed to reopen pulled (uncompressed) layer {}",
                        compressed_tmp_path.display()
                    )
                })?;
                // SAFETY: same discipline as the gzip branch above -- unique pid+timestamp name,
                // exclusive to this process and call.
                let mmap = unsafe { memmap2::Mmap::map(&tmp_file) }.with_context(|| {
                    format!(
                        "failed to mmap pulled layer {}",
                        compressed_tmp_path.display()
                    )
                })?;
                DecompressedSource::Mmapped {
                    mmap,
                    tmp_path: compressed_tmp_path,
                }
            };

            let decompressed_slice: &[u8] = match &source {
                DecompressedSource::Mmapped { mmap, .. } => &mmap[..],
                DecompressedSource::InMemory(v) => &v[..],
            };

            if verbose {
                eprintln!(
                    "  Layer {}/{} decompressed ({} bytes), rewriting ELFs...",
                    i + 1,
                    num_layers,
                    decompressed_slice.len()
                );
            }

            // Stream the rewritten OUTPUT tar directly to a temp file on disk instead of building
            // it as an in-memory `Vec<u8>` -- for a large layer (~2.6GB decompressed, e.g.
            // `linuxserver/webtop:debian-xfce`'s largest layer) that Vec would be potentially the
            // largest single allocation in the process, sitting in ordinary non-evictable heap
            // memory for the whole rewrite. The temp file IS the eventual cache file: on success,
            // `finalize_temp_into_cache` just renames it into place (atomic, same as
            // `write_cached_layer`'s own discipline) and mmaps it -- no second copy of the bytes
            // is ever created. Only if opening the temp file fails do we fall back to the
            // original in-memory `Vec<u8>` path, preserving correctness on any real host where the
            // streaming path can't be used (read-only cache dir, disk full, etc.).
            let mapped = match cache::temp_path_in_cache_dir().and_then(|tmp_path| {
                let tmp_file = std::fs::File::create(&tmp_path)
                    .with_context(|| format!("failed to create temp rewrite file {}", tmp_path.display()))?;
                let mut writer = std::io::BufWriter::new(tmp_file);
                rewrite_layer_elfs(decompressed_slice, &mut writer, verbose)
                    .with_context(|| format!("failed to rewrite ELFs in layer {}", i + 1))?;
                writer
                    .flush()
                    .with_context(|| format!("failed to flush rewritten layer {}", i + 1))?;
                drop(writer);
                Ok(tmp_path)
            }) {
                Ok(tmp_path) => {
                    if verbose {
                        eprintln!(
                            "  Layer {}/{} rewritten directly to disk, publishing to cache...",
                            i + 1,
                            num_layers
                        );
                    }
                    match cache::finalize_temp_into_cache(
                        &tmp_path,
                        &layer_desc.digest,
                        rewriter_version,
                        verbose,
                    ) {
                        Ok(mmapped) => mmapped,
                        Err(_) => {
                            // The streamed file couldn't be published as the cache entry (rename,
                            // reopen, or mmap failed). Fall back to an in-memory rewrite so
                            // correctness is preserved regardless -- this re-runs the (pure,
                            // in-memory) rewrite once more, which is the same cost the old
                            // always-in-memory path always paid, not a regression.
                            let mut out = Vec::new();
                            rewrite_layer_elfs(decompressed_slice, &mut out, verbose).with_context(|| {
                                format!("failed to rewrite ELFs in layer {} (fallback)", i + 1)
                            })?;
                            cache::write_and_map_cached_layer(&layer_desc.digest, rewriter_version, out, verbose)
                        }
                    }
                }
                Err(e) => {
                    if verbose {
                        eprintln!(
                            "  [cache] failed to open temp rewrite file for layer {}: {e:#}; rewriting in memory instead",
                            i + 1
                        );
                    }
                    let mut out = Vec::new();
                    rewrite_layer_elfs(decompressed_slice, &mut out, verbose)
                        .with_context(|| format!("failed to rewrite ELFs in layer {}", i + 1))?;
                    cache::write_and_map_cached_layer(&layer_desc.digest, rewriter_version, out, verbose)
                }
            };

            if let DecompressedSource::Mmapped { mmap, tmp_path } = source {
                drop(mmap);
                let _ = std::fs::remove_file(&tmp_path);
            }

            if verbose {
                eprintln!(
                    "  Layer {}/{} ready ({} bytes after rewrite)",
                    i + 1,
                    num_layers,
                    mapped.len()
                );
            }

            layers.push(mapped);
        }

        Ok::<_, anyhow::Error>((config, layers))
    })?;

    let config_json = config.data.to_vec();
    let parsed_config = match ConfigFile::try_from(config) {
        Ok(cf) => {
            let exec_config = cf.config.as_ref();
            ImageConfig {
                entrypoint: exec_config.and_then(|c| c.entrypoint.clone()),
                cmd: exec_config.and_then(|c| c.cmd.clone()),
                env: exec_config.and_then(|c| c.env.clone()),
                working_dir: exec_config.and_then(|c| c.working_dir.clone()),
            }
        }
        Err(e) => {
            eprintln!("warning: failed to parse image config: {e}");
            ImageConfig::default()
        }
    };

    Ok(PulledLayers {
        layers,
        config: parsed_config,
        config_json,
    })
}

/// Rewrite every executable ELF entry inside one decompressed OCI layer tar's bytes, eagerly,
/// before guest boot -- streaming a fresh tar with rewritten ELF payloads spliced in place of the
/// originals directly into `out` (a rewrite can change a file's size, so entries are rebuilt with
/// a `tar::Builder` rather than patched in place).
///
/// Eager (at image-load time, host-side, before boot) was chosen over lazy (deferred to each
/// binary's first `exec()`) or cached (content-hash-keyed, reused across boots): the rewriter
/// itself (`litebox_syscall_rewriter::hook_syscalls_in_elf`) is a pure, `no_std`-capable,
/// in-memory `&[u8] -> Vec<u8>` transform with no host-only dependency forcing it out of the
/// guest-boot path, so there is no correctness reason to defer it -- only a latency/laziness
/// trade-off. Lazy rewriting would require plumbing a rewrite-on-first-exec cache through every
/// runner's `exec()` path (each of which currently assumes its rootfs backend already serves
/// pre-rewritten bytes), a materially larger change for a benefit (skipping unused binaries) that
/// does not apply to the base-image case this pass targets, where nearly every ELF a small image
/// ships is a real dependency reachable from its entrypoint. Content-hash caching across boots
/// (persisting rewritten bytes keyed by a hash of the original ELF, reused whenever the same
/// image is booted again) is a legitimate, purely additive follow-up once real usage shows
/// eager-every-boot rewriting is a measured latency problem -- deliberately deferred rather than
/// built speculatively against no evidence of that cost.
///
/// Generic over the output sink (`W: Write`) rather than fixed to `Vec<u8>`: the caller
/// (`pull_layers_in_memory`) streams the OUTPUT tar directly to a file (`BufWriter<File>`) so the
/// rewritten layer -- potentially the largest single allocation in the process for a big layer
/// (~2.6GB observed for `linuxserver/webtop:debian-xfce`'s largest layer) -- is never held as one
/// giant ordinary heap `Vec` at all. On any failure to open that file, the caller falls back to
/// passing a `Vec<u8>` as `out` instead, so correctness is preserved regardless of which sink is
/// used; this function itself has no opinion on which sink backs a real file vs. memory.
pub fn rewrite_layer_elfs<W: Write>(layer_tar: &[u8], out: &mut W, verbose: bool) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(layer_tar);
    let mut builder = tar::Builder::new(out);
    for entry_result in archive.entries()? {
        let mut entry = entry_result.context("failed to read tar entry while rewriting")?;
        let mut header = entry.header().clone();
        let entry_type = header.entry_type();
        let path = entry.path()?.into_owned();

        if entry_type != tar::EntryType::Regular {
            // Symlinks, directories, whiteout markers, etc. pass through unchanged --
            // only regular-file payloads can be an ELF worth rewriting.
            builder.append(&header, std::io::empty())?;
            continue;
        }

        let is_executable = header.mode().is_ok_and(|m| m & 0o111 != 0);
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;

        let out_data = if is_executable {
            crate::rewrite_elf(&data, &path, verbose)
        } else {
            data
        };

        header.set_size(out_data.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, &path, out_data.as_slice())?;
    }
    builder.finish()?;
    Ok(())
}

/// Generate a `litebox/config_and_run.sh` shell script from the OCI image config.
///
/// The script:
/// 1. Exports all `ENV` variables from the image config
/// 2. `cd`s to `WORKDIR` (defaults to `/`)
/// 3. If the caller passes arguments (`"$@"`), executes them directly
/// 4. Otherwise falls back to the image's ENTRYPOINT/CMD as the default command
///
/// This allows the runner to either pass a command explicitly:
///   `/litebox/config_and_run.sh python3 -c 'print("hi")'`
/// or rely on the image default:
///   `/litebox/config_and_run.sh`
///
/// Always generates a script — even if the image has no ENV, WORKDIR,
/// ENTRYPOINT, or CMD, the script will simply `exec "$@"` so callers can
/// use `config_and_run.sh` uniformly without checking whether it exists.
pub fn generate_config_and_run_script(config: &ImageConfig) -> String {
    use std::fmt::Write as _;

    let has_entrypoint = config.entrypoint.as_ref().is_some_and(|v| !v.is_empty());
    let has_cmd = config.cmd.as_ref().is_some_and(|v| !v.is_empty());

    let mut script = String::from("#!/bin/sh\n");

    // Export ENV vars.
    if let Some(env_vars) = &config.env {
        for var in env_vars {
            // Each var is "KEY=VALUE". Shell-quote the value.
            if let Some(eq_idx) = var.find('=') {
                let key = &var[..eq_idx];
                let value = &var[eq_idx + 1..];
                let _ = writeln!(script, "export {key}='{}'", shell_escape(value));
            }
        }
    }

    // cd to WORKDIR.
    let workdir = config
        .working_dir
        .as_deref()
        .filter(|w| !w.is_empty())
        .unwrap_or("/");
    let _ = writeln!(script, "cd '{}'", shell_escape(workdir));

    // Build the exec line.
    //
    // If the caller passes arguments, run those as the command.
    // Otherwise fall back to the image's ENTRYPOINT + CMD.
    let quote = |args: &[String]| -> String {
        args.iter()
            .map(|a| format!("'{}'", shell_escape(a)))
            .collect::<Vec<_>>()
            .join(" ")
    };

    // Build the default command from ENTRYPOINT and/or CMD.
    let default_cmd = if has_entrypoint && has_cmd {
        let ep = config.entrypoint.as_deref().unwrap_or_default();
        let cmd = config.cmd.as_deref().unwrap_or_default();
        format!("{} {}", quote(ep), quote(cmd))
    } else if has_entrypoint {
        quote(config.entrypoint.as_deref().unwrap_or_default())
    } else if has_cmd {
        quote(config.cmd.as_deref().unwrap_or_default())
    } else {
        String::new()
    };

    if default_cmd.is_empty() {
        // No default command — just exec whatever the caller passes.
        let _ = writeln!(script, "exec \"$@\"");
    } else {
        let _ = write!(
            script,
            "if [ $# -gt 0 ]; then\n  exec \"$@\"\nelse\n  exec {default_cmd}\nfi\n",
        );
    }

    script
}

/// Escape single quotes for use inside single-quoted shell strings.
fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Extract a single OCI layer (tar or tar+gzip) into the rootfs directory.
///
/// Handles OCI whiteout files (`.wh.*` prefixed entries) which indicate
/// files deleted in upper layers. Symlinks are collected into `symlinks` for
/// cross-platform resolution after all layers are extracted. Permission modes
/// from tar headers are recorded in `permissions` for cross-platform use.
fn extract_layer(
    data: &[u8],
    media_type: &str,
    rootfs: &Path,
    symlinks: &mut Vec<DeferredSymlink>,
    permissions: &mut HashMap<PathBuf, u32>,
) -> anyhow::Result<()> {
    // Determine if the layer is gzipped
    let is_gzip = media_type.contains("gzip") || is_gzip_data(data);

    if is_gzip {
        let decoder = flate2::read::GzDecoder::new(data);
        extract_tar(decoder, rootfs, symlinks, permissions)
    } else {
        extract_tar(data, rootfs, symlinks, permissions)
    }
}

/// Check if data starts with the gzip magic bytes.
fn is_gzip_data(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}

/// A hard link whose target was not yet extracted when encountered.
struct DeferredHardLink {
    /// Destination path inside the rootfs (where the hard link should be created).
    target: PathBuf,
    /// Source path inside the rootfs (the file the hard link points to).
    link_source: PathBuf,
    /// Original link name from the tar header (used for permission lookup).
    link_name: PathBuf,
}

/// Tracked symlink from a container image layer.
struct DeferredSymlink {
    /// Relative path inside the rootfs (e.g., `usr/lib64/ld-linux-x86-64.so.2`).
    rel_path: PathBuf,
    /// Symlink target as stored in the tar (Unix-style, may be relative or absolute).
    link_target: PathBuf,
}

/// Extract a tar archive into the rootfs, handling OCI whiteout files.
///
/// Symlinks are NOT created as OS symlinks. Instead they are tracked in
/// `symlinks` so the caller can resolve them cross-platform after all layers
/// are extracted. Hard links whose targets appear later in the archive are
/// collected during the first pass and resolved after all regular entries
/// have been extracted. Permission modes from tar headers are recorded in
/// `permissions` keyed by relative path.
fn extract_tar<R: Read>(
    reader: R,
    rootfs: &Path,
    symlinks: &mut Vec<DeferredSymlink>,
    permissions: &mut HashMap<PathBuf, u32>,
) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(reader);
    // Preserve Unix permissions when running on Unix hosts.
    // On non-Unix platforms permissions are tracked separately in the
    // `permissions` HashMap from tar headers.
    #[cfg(unix)]
    {
        archive.set_preserve_permissions(true);
    }

    let mut deferred_links: Vec<DeferredHardLink> = Vec::new();

    for entry_result in archive.entries()? {
        let mut entry = entry_result.context("failed to read tar entry")?;
        // Normalize the path to prevent path traversal (../ and absolute paths)
        // and to strip inconsistent ./ prefixes that tar entries may carry.
        let path = normalize_path(&entry.path()?);
        let path_str = path.to_string_lossy();

        if is_excluded_path(&path_str) {
            continue;
        }

        // Handle OCI whiteout files
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if file_name == ".wh..wh..opq" {
                // Opaque whiteout: clear the entire parent directory contents
                if let Some(parent) = path.parent() {
                    let target = rootfs.join(parent);
                    if target.exists() {
                        // Remove all children but keep the directory itself
                        for child in std::fs::read_dir(&target)? {
                            let child = child?;
                            let ft = child.file_type()?;
                            if ft.is_dir() {
                                std::fs::remove_dir_all(child.path())?;
                            } else {
                                std::fs::remove_file(child.path())?;
                            }
                        }
                    }
                    // Also prune in-memory symlinks under this directory so
                    // they are not resurrected by materialize_symlinks.
                    // Guard: Path::starts_with("") matches everything, so skip
                    // pruning when parent is empty (root-level opaque whiteout
                    // already cleared the filesystem above).
                    if parent.as_os_str().is_empty() {
                        symlinks.clear();
                        permissions.clear();
                    } else {
                        symlinks.retain(|s| !s.rel_path.starts_with(parent));
                        // Prune permissions for files under the cleared directory.
                        permissions.retain(|p, _| !p.starts_with(parent));
                    }
                }
                continue;
            }
            if let Some(target_name) = file_name.strip_prefix(".wh.") {
                // Regular whiteout: delete the specific file/directory
                if let Some(parent) = path.parent() {
                    let whiteout_rel = parent.join(target_name);
                    let target = rootfs.join(&whiteout_rel);
                    if target.is_dir() {
                        let _ = std::fs::remove_dir_all(&target);
                        // Prune symlinks under the removed directory.
                        symlinks.retain(|s| !s.rel_path.starts_with(&whiteout_rel));
                        // Prune permissions under the removed directory.
                        permissions.retain(|p, _| !p.starts_with(&whiteout_rel));
                    } else {
                        let _ = std::fs::remove_file(&target);
                        // Prune the exact symlink entry if present.
                        symlinks.retain(|s| s.rel_path != whiteout_rel);
                        // Prune the exact permissions entry.
                        permissions.remove(&whiteout_rel);
                    }
                }
                continue;
            }
        }

        let target = rootfs.join(&path);

        // Ensure parent directory exists
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let entry_type = entry.header().entry_type();

        // Handle hard links: copy the link target instead of creating an OS
        // hard link. The tar crate's unpack() tries std::fs::hard_link which
        // can fail if the target hasn't been extracted yet (ordering issue),
        // and the litebox filesystem doesn't support hard links anyway.
        if entry_type == tar::EntryType::Link {
            let link_name = normalize_path(
                &entry
                    .link_name()?
                    .context("hard link entry has no link name")?,
            );
            let link_source = rootfs.join(&link_name);
            // On Windows' case-insensitive NTFS, two case-distinct Linux paths (e.g.
            // usr/share/terminfo/L/LFT-PC850 vs usr/share/terminfo/l/lft-pc850, a real
            // collision found packaging linuxserver/webtop:arch-xfce) can resolve to the
            // SAME physical file on disk. Copying a file onto itself is a no-op we should
            // skip rather than attempt -- Windows sometimes tolerates a self-copy silently
            // and sometimes fails with "the process cannot access the file" (os error 32)
            // depending on handle/timing state, which is why this reproduced identically
            // on a retry rather than looking like ordinary transient contention.
            // `target` doesn't exist on disk yet at this point (it's the file we're about
            // to create), so `canonicalize()` can't be used to detect the collision -- it
            // requires the path to already exist. Compare the would-be OS path strings
            // case-insensitively instead, which is exactly the comparison NTFS itself uses.
            let is_self_collision = link_source
                .to_str()
                .zip(target.to_str())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
            if is_self_collision {
                eprintln!(
                    "  Skipping self-copy (case-collision on this filesystem): {} -> {}",
                    link_source.display(),
                    target.display()
                );
            } else if link_source.exists() {
                std::fs::copy(&link_source, &target).with_context(|| {
                    format!(
                        "failed to copy hard link target {} -> {}",
                        link_source.display(),
                        target.display()
                    )
                })?;
                // Copy permission mode from the link source.
                let link_rel = normalize_path(&link_name);
                if let Some(&mode) = permissions.get(&link_rel) {
                    permissions.insert(path.clone(), mode);
                }
            } else {
                // Target hasn't been extracted yet — defer to second pass.
                deferred_links.push(DeferredHardLink {
                    target,
                    link_source,
                    link_name: link_name.clone(),
                });
            }
            continue;
        }

        // Track symlinks in memory instead of creating OS symlinks.
        // OS symlinks on Windows require special privileges and don't handle
        // Unix-style relative paths reliably, so we resolve them ourselves
        // after all layers are extracted.
        if entry_type == tar::EntryType::Symlink {
            let link_target = entry
                .link_name()?
                .context("symlink entry has no link name")?
                .into_owned();
            // A later layer may override this symlink, so remove any stale
            // entry with the same rel_path.
            symlinks.retain(|s| s.rel_path != path);
            // If a previous layer extracted a file or directory at this path,
            // remove it so the symlink takes precedence.
            if target.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&target) {
                    eprintln!(
                        "  warning: failed to remove directory for symlink override {path_str}: {e}"
                    );
                }
            } else if target.exists()
                && let Err(e) = std::fs::remove_file(&target)
            {
                eprintln!("  warning: failed to remove file for symlink override {path_str}: {e}");
            }
            symlinks.push(DeferredSymlink {
                rel_path: path.clone(),
                link_target,
            });
            continue;
        }

        // Normal file/directory: use the standard unpack.
        //
        // If a previous layer recorded a symlink at exactly this path, or at an
        // ancestor of this path, the real file/directory from an upper layer takes
        // precedence -- remove that stale symlink entry (and, transitively, any
        // symlink nested under it, since resolving through a symlink that no longer
        // exists would be wrong). This must NOT evict symlinks nested *under* `path`
        // merely because a directory entry for `path` reappears here: OCI layers
        // routinely re-emit an unchanged parent directory (e.g. to update its
        // mtime/mode) whenever any descendant file changes, without that directory
        // becoming a fresh, empty directory that erases previously-extracted
        // descendants -- so a plain `usr/lib` entry in an upper layer must not
        // discard a lower layer's `usr/lib/libz.so.1` symlink.
        symlinks.retain(|s| s.rel_path != path && !path.starts_with(&s.rel_path));
        entry
            .unpack(&target)
            .with_context(|| format!("failed to unpack entry: {path_str}"))?;

        // Record the permission mode from the tar header for cross-platform use.
        if let Ok(mode) = entry.header().mode() {
            permissions.insert(path.clone(), mode);
        }
    }

    // Second pass: resolve deferred hard links now that all entries are extracted.
    for link in &deferred_links {
        if link.link_source.exists() {
            if let Some(parent) = link.target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&link.link_source, &link.target).with_context(|| {
                format!(
                    "failed to copy deferred hard link {} -> {}",
                    link.link_source.display(),
                    link.target.display()
                )
            })?;
            // Copy permission mode from the link source.
            let link_rel = normalize_path(&link.link_name);
            if let Some(&mode) = permissions.get(&link_rel) {
                let target_rel = link.target.strip_prefix(rootfs).unwrap_or(&link.target);
                permissions.insert(target_rel.to_path_buf(), mode);
            }
        } else {
            // Target still doesn't exist after the full layer extraction —
            // this is unusual but not fatal; warn and skip.
            eprintln!(
                "  warning: hard link target {} not found after full extraction, skipping {}",
                link.link_source.display(),
                link.target.display()
            );
        }
    }

    Ok(())
}

/// Resolve a symlink target within the rootfs using the symlink map.
///
/// Handles both absolute targets (e.g., `/lib/x86_64-linux-gnu/ld.so`) and
/// relative targets (e.g., `../lib/x86_64-linux-gnu/ld.so`). Follows symlink
/// chains up to `max_depth` hops.
fn resolve_symlink_in_rootfs(
    rel_path: &Path,
    rootfs: &Path,
    symlink_map: &HashMap<PathBuf, PathBuf>,
    max_depth: u32,
) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }

    // Empty rel_path would resolve to the rootfs directory itself — treat
    // as unresolvable to avoid accidentally matching the entire rootfs.
    if rel_path.as_os_str().is_empty() {
        return None;
    }

    // Check if this rel_path is itself a symlink
    if let Some(link_target) = symlink_map.get(rel_path) {
        // Resolve the target to a new rel_path
        let resolved_rel = if is_unix_absolute(link_target) {
            normalize_path(link_target)
        } else {
            // Relative target: resolve from parent of the symlink
            let parent = rel_path.parent().unwrap_or(Path::new(""));
            normalize_path(&parent.join(link_target))
        };
        // Recurse to follow chains
        return resolve_symlink_in_rootfs(&resolved_rel, rootfs, symlink_map, max_depth - 1);
    }

    // Not a symlink — check if any ancestor is a symlink (e.g., `lib64/foo` where
    // `lib64` → `usr/lib64`).
    let components: Vec<_> = rel_path.components().collect();
    for i in 1..components.len() {
        let prefix: PathBuf = components[..i].iter().collect();
        if let Some(link_target) = symlink_map.get(&prefix) {
            let resolved_prefix = if is_unix_absolute(link_target) {
                normalize_path(link_target)
            } else {
                let parent = prefix.parent().unwrap_or(Path::new(""));
                normalize_path(&parent.join(link_target))
            };
            let suffix: PathBuf = components[i..].iter().collect();
            let new_rel = resolved_prefix.join(suffix);
            return resolve_symlink_in_rootfs(&new_rel, rootfs, symlink_map, max_depth - 1);
        }
    }

    let host_path = rootfs.join(rel_path);
    if host_path.exists() {
        Some(host_path)
    } else {
        None
    }
}

/// Paths excluded from extraction entirely -- package-manager bookkeeping
/// metadata that real guest programs never read at runtime, so it's safe to
/// drop rather than needing to represent it faithfully on the host
/// filesystem. Currently just pacman's (Arch Linux) local install database:
/// `var/lib/pacman/local/<name>-<epoch>:<version>-<release>/` directory names
/// contain a literal `:`, a reserved character in Windows paths (valid only
/// as the drive-letter separator) -- `std::fs::create_dir_all` fails with
/// "The directory name is invalid" (os error 267) on any such entry. Found
/// packaging `linuxserver/webtop:arch-xfce`, an Arch-based image; Alpine-based
/// images (using `apk`, no colon-bearing package-db paths) don't hit this.
fn is_excluded_path(path_str: &str) -> bool {
    path_str.starts_with("var/lib/pacman/local/") || path_str.starts_with("var\\lib\\pacman\\local\\")
}

/// Check if a path starts with `/` (Unix-style absolute).
///
/// On Windows, `Path::is_absolute()` requires a drive letter, so Unix-style
/// paths like `/lib/foo` are not detected as absolute. This helper checks
/// the raw string instead.
fn is_unix_absolute(path: &Path) -> bool {
    path.as_os_str()
        .to_str()
        .is_some_and(|s| s.starts_with('/'))
}

/// Normalize a path by resolving `.` and `..` components without touching the
/// filesystem (no symlink resolution, no existence checks). Strips any root
/// component so the result is always a relative path.
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
            c @ std::path::Component::Normal(_) => result.push(c),
        }
    }
    result.iter().collect()
}

/// Materialize all deferred symlinks by copying or creating directories.
///
/// This is called after all OCI layers have been extracted, so every real file
/// should be on disk. Symlinks are resolved through the in-memory map (handling
/// chains like `lib64` → `usr/lib64` → real dir) and then:
/// - File symlinks: the target file is copied to the symlink location.
///   The resolved target's permission mode is also recorded for the symlink path.
/// - Directory symlinks: an empty directory is created (its contents will be
///   expanded by `scan_rootfs`'s dir-symlink logic).
fn materialize_symlinks(
    symlink_map: &HashMap<PathBuf, PathBuf>,
    rootfs: &Path,
    permissions: &mut HashMap<PathBuf, u32>,
    verbose: bool,
) -> anyhow::Result<()> {
    for (rel_path, link_target) in symlink_map {
        let host_path = rootfs.join(rel_path);
        if host_path.exists() {
            // A later layer may have replaced the symlink with a real file.
            continue;
        }

        if let Some(resolved) = resolve_symlink_in_rootfs(
            rel_path,
            rootfs,
            symlink_map,
            32, // max chain depth
        ) {
            if let Some(parent) = host_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            if resolved.is_dir() {
                // Directory symlink: create directory placeholder.
                // scan_rootfs will discover this is a "dir symlink" and expand
                // it through the symlink_map.
                std::fs::create_dir_all(&host_path)?;
                if verbose {
                    eprintln!(
                        "  [symlink→dir] {} -> {}",
                        rel_path.display(),
                        link_target.display()
                    );
                }
            } else if resolved.is_file() {
                std::fs::copy(&resolved, &host_path).with_context(|| {
                    format!(
                        "failed to materialize symlink {} -> {}",
                        rel_path.display(),
                        resolved.display()
                    )
                })?;
                // Record the resolved target's permission mode for this symlink path.
                let resolved_rel = resolved
                    .strip_prefix(rootfs)
                    .unwrap_or(&resolved)
                    .to_path_buf();
                if let Some(&mode) = permissions.get(&resolved_rel) {
                    permissions.insert(rel_path.clone(), mode);
                }
                if verbose {
                    eprintln!(
                        "  [symlink→file] {} -> {}",
                        rel_path.display(),
                        link_target.display()
                    );
                }
            }
        } else if verbose {
            eprintln!(
                "  [symlink-broken] {} -> {} (unresolvable)",
                rel_path.display(),
                link_target.display()
            );
        }
    }

    Ok(())
}

/// Look up the Unix permission mode for a file.
///
/// Look up the Unix file mode for a rootfs-relative path from the OCI tar
/// header permissions map. Defaults to 0o644 if not found.
fn lookup_mode(rel_path: &Path, permissions: &HashMap<PathBuf, u32>) -> u32 {
    if let Some(&mode) = permissions.get(rel_path) {
        mode & 0o7777
    } else {
        0o644
    }
}

/// Scan an extracted rootfs directory and build a file map for packaging.
///
/// Walks the rootfs directory tree and collects all regular files with their
/// paths and permission bits. After `materialize_symlinks` has been called,
/// file symlinks are already materialized as regular file copies on disk.
///
/// `symlink_map` provides the original symlink mapping from extraction so
/// that **directory symlinks** (e.g., `lib64` → `usr/lib64`) can be expanded:
/// all files under the target directory are duplicated under the symlink's
/// path prefix so that paths like `lib64/ld-linux-x86-64.so.2` exist in the tar.
///
/// `permissions` provides Unix permission modes captured from tar headers
/// during extraction, so permission bits are accurate on non-Unix hosts.
#[allow(clippy::implicit_hasher)]
/// The link target to record for a symlink at `host_path`, as a Unix-style string.
///
/// Prefers `symlink_map`, which carries the target verbatim from the OCI layer's own tar
/// headers -- the only faithful source on a non-Unix host, where extraction cannot create real
/// symlinks and the on-disk entry is a placeholder. Falls back to the OS link (Linux hosts).
///
/// Returning `None` means "emit this as a file copy after all", so a target that cannot be
/// recovered degrades to the previous behaviour rather than producing a dangling link.
fn link_target_for(
    host_path: &Path,
    rootfs: &Path,
    symlink_map: &HashMap<PathBuf, PathBuf>,
) -> Option<String> {
    let rel = host_path.strip_prefix(rootfs).unwrap_or(host_path);
    if let Some(target) = symlink_map.get(rel) {
        return Some(target.to_string_lossy().replace('\\', "/"));
    }
    std::fs::read_link(host_path)
        .ok()
        .map(|t| t.to_string_lossy().replace('\\', "/"))
}

pub fn scan_rootfs(
    rootfs: &Path,
    symlink_map: &HashMap<PathBuf, PathBuf>,
    permissions: &HashMap<PathBuf, u32>,
    verbose: bool,
) -> anyhow::Result<RootfsFileMap> {
    let mut files = BTreeMap::new();

    // Identify directory symlinks and their resolved targets on disk.
    let mut dir_symlinks: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (rel_path, link_target) in symlink_map {
        let host_path = rootfs.join(rel_path);
        if host_path.is_dir() {
            // This dir symlink was materialized as an empty directory.
            // Resolve the target to find the real directory to expand from.
            if let Some(resolved) =
                resolve_symlink_in_rootfs(rel_path, rootfs, symlink_map, 32).filter(|r| r.is_dir())
            {
                if verbose {
                    eprintln!(
                        "  [dir-symlink] {} -> {}",
                        rel_path.display(),
                        link_target.display()
                    );
                }
                dir_symlinks.push((host_path, resolved));
            }
        }
    }

    for entry in walkdir::WalkDir::new(rootfs)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let rel_path = entry.path().strip_prefix(rootfs).unwrap_or(entry.path());

        // Skip the root itself
        if rel_path == Path::new("") {
            continue;
        }

        let tar_path = rel_path.to_string_lossy().to_string();
        // Normalize path separators to Unix-style for the tar archive.
        let tar_path = tar_path.replace('\\', "/");

        if entry.file_type().is_file() {
            // A symlink extracted on a non-Unix host is MATERIALIZED as a regular file copy
            // (see `materialize_symlinks`), because Windows cannot create one without
            // elevation -- so `is_symlink()` below is never true here and the on-disk entry
            // has lost its identity. `symlink_map` still holds the target verbatim from the
            // layer's own tar header, so consult it FIRST and re-emit a real link.
            //
            // Without this the packager silently reproduces the flattening it is meant to fix:
            // a repackaged alpine:latest came out with 417 entries, ZERO symlinks and 305
            // copies of the same 804,648-byte busybox.
            if let Some(target) = symlink_map.get(rel_path) {
                let target = target.to_string_lossy().replace('\\', "/");
                if verbose {
                    eprintln!("  [symlink] {tar_path} -> {target}");
                }
                files.insert(
                    entry.path().to_path_buf(),
                    RootfsEntry {
                        tar_path,
                        read_path: entry.path().to_path_buf(),
                        is_executable: false,
                        mode: lookup_mode(rel_path, permissions),
                        symlink_target: Some(target),
                    },
                );
                continue;
            }

            let mode = lookup_mode(rel_path, permissions);
            let is_executable = mode & 0o111 != 0;

            if verbose && is_executable {
                eprintln!("  [exec] {tar_path}");
            }

            files.insert(
                entry.path().to_path_buf(),
                RootfsEntry {
                    tar_path,
                    read_path: entry.path().to_path_buf(),
                    is_executable,
                    mode,
                    symlink_target: None,
                },
            );
        } else if entry.file_type().is_symlink() {
            // On platforms that still have OS symlinks (Linux), resolve them.
            if let Some(resolved) = resolve_in_rootfs(entry.path(), rootfs, 16) {
                if resolved.is_file() {
                    let resolved_rel = resolved.strip_prefix(rootfs).unwrap_or(&resolved);
                    let mode = lookup_mode(resolved_rel, permissions);
                    let is_executable = mode & 0o111 != 0;

                    files.insert(
                        entry.path().to_path_buf(),
                        RootfsEntry {
                            tar_path,
                            read_path: resolved.clone(),
                            is_executable,
                            mode,
                            symlink_target: link_target_for(entry.path(), rootfs, symlink_map),
                        },
                    );
                } else if resolved.is_dir() {
                    if verbose {
                        eprintln!("  [dir-symlink] {tar_path} -> {}", resolved.display());
                    }
                    dir_symlinks.push((entry.path().to_path_buf(), resolved));
                }
            } else if verbose {
                eprintln!("  [skip] broken symlink: {tar_path}");
            }
        }
        // Directories are created implicitly by the tar builder
    }

    // Expand directory symlinks: walk the resolved target directory and create
    // additional tar entries under the symlink's path prefix. For example, if
    // `lib64` → `usr/lib64`, then `usr/lib64/ld-linux-x86-64.so.2` also
    // appears as `lib64/ld-linux-x86-64.so.2` in the tar.

    // Build a set of existing tar paths for O(1) duplicate checks.
    let mut tar_paths: HashSet<String> = files.values().map(|e| e.tar_path.clone()).collect();

    for (symlink_host_path, resolved_dir) in &dir_symlinks {
        let symlink_rel = symlink_host_path
            .strip_prefix(rootfs)
            .unwrap_or(symlink_host_path);

        for entry in walkdir::WalkDir::new(resolved_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
                continue;
            }

            // Determine the host path to read from and whether it's a file.
            let (read_path, is_file) = if entry.file_type().is_symlink() {
                if let Some(resolved) = resolve_in_rootfs(entry.path(), rootfs, 16) {
                    let is_file = resolved.is_file();
                    (resolved, is_file)
                } else {
                    continue;
                }
            } else {
                (entry.path().to_path_buf(), true)
            };

            if !is_file {
                continue;
            }

            // Build the tar path: replace the resolved_dir prefix with symlink_rel.
            let entry_rel = entry
                .path()
                .strip_prefix(resolved_dir)
                .unwrap_or(entry.path());
            let tar_path = symlink_rel.join(entry_rel).to_string_lossy().to_string();
            // Normalize path separators to Unix-style for the tar archive.
            let tar_path = tar_path.replace('\\', "/");

            // Use symlink_host_path-based key to avoid colliding with the
            // original entry under the resolved directory.
            let map_key = symlink_host_path.join(entry_rel);

            // Skip if we already have this tar path.
            if !tar_paths.insert(tar_path.clone()) {
                continue;
            }

            let read_rel = read_path.strip_prefix(rootfs).unwrap_or(&read_path);
            let mode = lookup_mode(read_rel, permissions);
            let is_executable = mode & 0o111 != 0;

            if verbose {
                eprintln!("  [dir-symlink-expand] {tar_path}");
            }

            files.insert(
                map_key,
                RootfsEntry {
                    tar_path,
                    read_path,
                    is_executable,
                    mode,
                    // Directory-symlink EXPANSION: these are synthesized paths under the
                    // symlink's prefix (e.g. lib64/x from usr/lib64/x), not links themselves,
                    // so they stay real file copies.
                    symlink_target: None,
                },
            );
        }
    }

    if verbose {
        let exec_count = files.values().filter(|e| e.is_executable).count();
        eprintln!("  Found {} files ({} executables)", files.len(), exec_count);
    }

    Ok(RootfsFileMap { files })
}

/// Resolve a symlink within the rootfs context, handling absolute symlinks
/// that would otherwise escape the rootfs boundary.
fn resolve_in_rootfs(path: &Path, rootfs: &Path, max_depth: u32) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }

    let metadata = path.symlink_metadata().ok()?;
    if !metadata.file_type().is_symlink() {
        return if path.exists() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }

    let link_target = std::fs::read_link(path).ok()?;
    let resolved = if is_unix_absolute(&link_target) {
        // Absolute symlink: resolve within rootfs (normalize to prevent traversal)
        rootfs.join(normalize_path(&link_target))
    } else {
        // Relative symlink — join with parent, then canonicalize `..` components
        // to prevent escaping the rootfs boundary.
        let joined = path.parent()?.join(&link_target);
        // Normalize to strip `..` then re-root inside rootfs.
        let normalized = normalize_path(joined.strip_prefix(rootfs).unwrap_or(&joined));
        rootfs.join(normalized)
    };

    resolve_in_rootfs(&resolved, rootfs, max_depth - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_symlink_in_rootfs_happy_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path();
        std::fs::create_dir_all(rootfs.join("usr/lib64")).unwrap();
        std::fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::write(rootfs.join("usr/lib64/libc.so"), b"fake").unwrap();
        std::fs::write(rootfs.join("usr/lib64/foo.so"), b"elf").unwrap();
        std::fs::write(rootfs.join("usr/lib/libfoo.so"), b"elf").unwrap();
        std::fs::write(rootfs.join("usr/bin/sh"), b"elf").unwrap();
        std::fs::write(rootfs.join("c"), b"data").unwrap();

        let mut symlink_map = HashMap::new();
        symlink_map.insert(PathBuf::from("lib64"), PathBuf::from("usr/lib64"));
        symlink_map.insert(PathBuf::from("a"), PathBuf::from("b"));
        symlink_map.insert(PathBuf::from("b"), PathBuf::from("c"));
        symlink_map.insert(PathBuf::from("bin/sh"), PathBuf::from("/usr/bin/sh"));
        symlink_map.insert(
            PathBuf::from("usr/lib64/libfoo.so"),
            PathBuf::from("../lib/libfoo.so"),
        );

        // Direct symlink: lib64 -> usr/lib64
        let r = resolve_symlink_in_rootfs(Path::new("lib64"), rootfs, &symlink_map, 32);
        assert_eq!(r, Some(rootfs.join("usr/lib64")));

        // Chain: a -> b -> c
        let r = resolve_symlink_in_rootfs(Path::new("a"), rootfs, &symlink_map, 32);
        assert_eq!(r, Some(rootfs.join("c")));

        // Absolute target: bin/sh -> /usr/bin/sh
        let r = resolve_symlink_in_rootfs(Path::new("bin/sh"), rootfs, &symlink_map, 32);
        assert_eq!(r, Some(rootfs.join("usr/bin/sh")));

        // Relative target: usr/lib64/libfoo.so -> ../lib/libfoo.so
        let r =
            resolve_symlink_in_rootfs(Path::new("usr/lib64/libfoo.so"), rootfs, &symlink_map, 32);
        assert_eq!(r, Some(rootfs.join("usr/lib/libfoo.so")));

        // Ancestor is symlink: lib64/foo.so resolves via lib64 -> usr/lib64
        let r = resolve_symlink_in_rootfs(Path::new("lib64/foo.so"), rootfs, &symlink_map, 32);
        assert_eq!(r, Some(rootfs.join("usr/lib64/foo.so")));
    }

    #[test]
    fn resolve_symlink_in_rootfs_edge_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path();
        std::fs::write(rootfs.join("hello.txt"), b"hi").unwrap();

        // Cycle: a -> b -> a
        let mut cycle_map = HashMap::new();
        cycle_map.insert(PathBuf::from("a"), PathBuf::from("b"));
        cycle_map.insert(PathBuf::from("b"), PathBuf::from("a"));
        assert!(resolve_symlink_in_rootfs(Path::new("a"), rootfs, &cycle_map, 32).is_none());

        let empty_map = HashMap::new();

        // Empty path
        assert!(resolve_symlink_in_rootfs(Path::new(""), rootfs, &empty_map, 32).is_none());

        // Nonexistent path
        assert!(
            resolve_symlink_in_rootfs(Path::new("does/not/exist"), rootfs, &empty_map, 32)
                .is_none()
        );

        // Regular file (not a symlink) returns host path directly
        let r = resolve_symlink_in_rootfs(Path::new("hello.txt"), rootfs, &empty_map, 32);
        assert_eq!(r, Some(rootfs.join("hello.txt")));
    }
}
