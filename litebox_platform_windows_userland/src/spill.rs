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
