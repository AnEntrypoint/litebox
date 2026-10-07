use std::fs::{File, OpenOptions};
use std::os::windows::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const SPILL_DIRECTORY_ENV_VAR: &str = "LITEBOX_INTERNAL_SHARED_SPILL_DIR";

pub fn spill_directory() -> Option<&'static PathBuf> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            std::env::var_os(SPILL_DIRECTORY_ENV_VAR)
                .map(PathBuf::from)
                .filter(|directory| directory.is_dir())
        })
        .as_ref()
}

pub fn open_spill_file(slot: u32) -> Option<File> {
    let path = spill_directory()?.join(format!("{slot}.bin"));
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .ok()
}

/// Path of the on-disk backing file of the persistent named shared-memory object called `name`
/// (SysV shm -- see `PageManagementProvider::create_file_backed_named_shared_memory`).
///
/// Nested inside the spill directory on purpose, so it inherits that directory's one-per-session
/// scoping instead of inventing a second one: the runner sets `SPILL_DIRECTORY_ENV_VAR` once at
/// startup and every cross-process-fork child inherits it, so two guest processes that are NOT
/// fork-related still resolve the same `name` to the SAME file -- the whole point, since the
/// alternative (a directory derived from `std::process::id()` in each process) would silently
/// give each process its own private copy of the segment. `None` when the spill directory is not
/// configured, which the caller reports as `UnsupportedByPlatform`.
///
/// These files are never cleaned up at session teardown, matching the spill directory's own
/// convention; they are unlinked by `shmctl(IPC_RMID)` (see
/// `PageManagementProvider::delete_file_backed_named_shared_memory`).
pub fn sysvshm_file_path(name: &str) -> Option<PathBuf> {
    spill_directory().map(|directory| directory.join("sysvshm").join(format!("{name}.bin")))
}

pub fn write_all_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> Option<()> {
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset).ok()?;
        if written == 0 {
            return None;
        }
        offset += written as u64;
        bytes = &bytes[written..];
    }
    Some(())
}

pub fn prepare_spill_directory() {
    if std::env::var_os(SPILL_DIRECTORY_ENV_VAR).is_some() {
        return;
    }
    let directory = std::env::temp_dir().join(format!("litebox-spill-{}", std::process::id()));
    if std::fs::create_dir_all(&directory).is_ok() {
        unsafe {
            std::env::set_var(SPILL_DIRECTORY_ENV_VAR, &directory);
        }
    }
}
