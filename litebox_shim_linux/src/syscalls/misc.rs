// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Miscellaneous Linux syscalls for LiteBox shim.
//!
//! Examples of syscalls handled here include `getrandom`, `uname`, and similar operations.

use crate::{ShimFS, ShimPlatform, Task};
use litebox::{platform::Instant as _, utils::TruncateExt as _};
use litebox_common_linux::errno::Errno;
use litebox_common_linux::user_pointers::UserPtrMut;

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `getrandom`.
    pub(crate) fn sys_getrandom(
        &self,
        buf: UserPtrMut<u8>,
        count: usize,
        _flags: litebox_common_linux::RngFlags,
    ) -> Result<usize, Errno> {
        // Linux guarantees at least 256 bytes of randomness per call before
        // checking for interrupts.
        const KBUF_LEN: usize = 256;
        let mut kbuf = [0; KBUF_LEN];
        let mut offset = 0;
        while offset < count {
            let len = (count - offset).min(kbuf.len());
            let kbuf = &mut kbuf[..len];
            <_ as litebox::platform::CrngProvider>::fill_bytes_crng(self.global.platform, kbuf);
            buf.copy_from_slice::<Platform>(offset, kbuf)
                .ok_or(Errno::EFAULT)?;
            offset += len;
            // TODO: check for interrupt here and break out.
        }
        Ok(offset)
    }
}

/// A const function to convert a str to a fixed-size array of bytes
///
/// Note the fixed-size array is terminated with a null byte, so the string must be
/// at most `N - 1` bytes long.
const fn to_fixed_size_array<const N: usize>(s: &str) -> [u8; N] {
    assert!(
        s.len() < N,
        "String is too long to fit in the fixed-size array"
    );
    let bytes = s.as_bytes();
    let mut arr = [0u8; N];
    let mut i = 0;
    while i < bytes.len() && i < N - 1 {
        arr[i] = bytes[i];
        i += 1;
    }
    arr
}
const SYS_INFO: litebox_common_linux::Utsname = litebox_common_linux::Utsname {
    sysname: to_fixed_size_array::<65>("LiteBox"),
    nodename: to_fixed_size_array::<65>("litebox"),
    release: to_fixed_size_array::<65>("5.11.0"), // libc seems to expect this to be not too old
    version: to_fixed_size_array::<65>("5.11.0"),
    #[cfg(target_arch = "x86_64")]
    machine: to_fixed_size_array::<65>("x86_64"),
    #[cfg(target_arch = "aarch64")]
    machine: to_fixed_size_array::<65>("aarch64"),
    domainname: to_fixed_size_array::<65>(""),
};

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `uname`.
    pub(crate) fn sys_uname(
        &self,
        buf: UserPtrMut<litebox_common_linux::Utsname>,
    ) -> Result<(), Errno> {
        buf.write_at_offset::<Platform>(0, SYS_INFO)
            .ok_or(Errno::EFAULT)
    }

    /// Handle syscall `sysinfo`.
    pub(crate) fn sys_sysinfo(&self) -> litebox_common_linux::Sysinfo {
        let now = self.global.platform.now();
        // The same figures `/proc/meminfo` reports, so `sysconf(_SC_PHYS_PAGES)` and a read of
        // meminfo agree.
        let (total_kb, avail_kb) = self.global.platform.memory_info_kb();
        let total = usize::try_from(total_kb.saturating_mul(1024)).unwrap_or(usize::MAX);
        let avail =
            usize::try_from(avail_kb.min(total_kb).saturating_mul(1024)).unwrap_or(usize::MAX);
        litebox_common_linux::Sysinfo {
            uptime: now.duration_since(&self.global.boot_time).as_secs().trunc(),
            // TODO: Populate these fields with actual values
            loads: [0; 3],
            #[cfg(target_arch = "x86_64")]
            totalram: total,
            freeram: avail,
            sharedram: 0, // We don't support shared memory
            bufferram: 0,
            totalswap: 0,
            freeswap: 0,
            procs: self.process().nr_threads().trunc(),
            totalhigh: 0,
            freehigh: 0,
            mem_unit: 1,
            ..Default::default()
        }
    }
}

const _LINUX_CAPABILITY_VERSION_1: u32 = 0x19980330;
const _LINUX_CAPABILITY_VERSION_2: u32 = 0x20071026; /* deprecated - use v3 */
const _LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;

impl<Platform: ShimPlatform, FS: ShimFS> Task<Platform, FS> {
    /// Handle syscall `capget`: reports this process's capability sets.
    pub(crate) fn sys_capget(
        &self,
        header: UserPtrMut<litebox_common_linux::CapHeader>,
        data: Option<UserPtrMut<litebox_common_linux::CapData>>,
    ) -> Result<(), Errno> {
        let hdr = header.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let words = match hdr.version {
            _LINUX_CAPABILITY_VERSION_1 => 1,
            _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3 => 2,
            _ => {
                header
                    .write_at_offset::<Platform>(
                        0,
                        litebox_common_linux::CapHeader {
                            version: _LINUX_CAPABILITY_VERSION_3,
                            pid: hdr.pid,
                        },
                    )
                    .ok_or(Errno::EFAULT)?;
                return if data.is_none() {
                    Ok(())
                } else {
                    Err(Errno::EINVAL)
                };
            }
        };
        let Some(data_ptr) = data else { return Ok(()) };
        let c = self.creds();
        for i in 0..words {
            let shift = 32 * i;
            let cap = litebox_common_linux::CapData {
                effective: (c.cap_eff >> shift) as u32,
                permitted: (c.cap_perm >> shift) as u32,
                inheritable: (c.cap_inh >> shift) as u32,
            };
            data_ptr
                .write_at_offset::<Platform>(i as isize, cap)
                .ok_or(Errno::EFAULT)?;
        }
        Ok(())
    }

    /// Handle syscall `capset`: a process may only shrink its permitted set, keep effective
    /// within permitted, and keep the inheritable set within the old permitted+inheritable.
    pub(crate) fn sys_capset(
        &self,
        header: UserPtrMut<litebox_common_linux::CapHeader>,
        data: Option<litebox_common_linux::user_pointers::UserPtr<litebox_common_linux::CapData>>,
    ) -> Result<(), Errno> {
        let hdr = header.read_at_offset::<Platform>(0).ok_or(Errno::EFAULT)?;
        let words = match hdr.version {
            _LINUX_CAPABILITY_VERSION_1 => 1,
            _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3 => 2,
            _ => return Err(Errno::EINVAL),
        };
        let data = data.ok_or(Errno::EFAULT)?;
        let (mut eff, mut perm, mut inh) = (0u64, 0u64, 0u64);
        for i in 0..words {
            let d = data
                .read_at_offset::<Platform>(i as isize)
                .ok_or(Errno::EFAULT)?;
            let shift = 32 * i;
            eff |= u64::from(d.effective) << shift;
            perm |= u64::from(d.permitted) << shift;
            inh |= u64::from(d.inheritable) << shift;
        }
        let mut c = (*self.creds()).clone();
        if perm & !c.cap_perm != 0 || eff & !perm != 0 || inh & !(c.cap_perm | c.cap_inh) != 0 {
            return Err(Errno::EPERM);
        }
        c.cap_eff = eff;
        c.cap_perm = perm;
        c.cap_inh = inh;
        self.set_creds(c);
        Ok(())
    }

    /// Handle syscall `personality`: only `PER_LINUX` exists; queries report it.
    #[expect(clippy::unused_self, reason = "syscall handler shape")]
    pub(crate) fn sys_personality(&self, _persona: u32) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use crate::syscalls::tests::init_platform;
    use litebox_common_linux::user_pointers::UserPtrMut;
    use zerocopy::FromZeros as _;

    #[test]
    fn test_getrandom() {
        use litebox_common_linux::RngFlags;

        let task = init_platform(None);

        let mut buf = [0u8; 16];
        let ptr = UserPtrMut::from_ptr(buf.as_mut_ptr());
        let count = task
            .sys_getrandom(ptr, buf.len() - 1, RngFlags::empty())
            .expect("getrandom failed");
        assert_eq!(count, buf.len() - 1);
        assert!(
            !buf.iter().all(|&b| b == 0),
            "buffer should not be all zeros"
        );
        assert!(buf[buf.len() - 1] == 0, "last byte should stay zero");
    }

    #[test]
    fn test_uname() {
        let task = init_platform(None);

        let mut utsname = litebox_common_linux::Utsname::new_zeroed();
        let ptr = UserPtrMut::from_ptr(&raw mut utsname);
        task.sys_uname(ptr).expect("uname failed");

        assert_eq!(utsname.sysname, super::SYS_INFO.sysname);
        assert_eq!(utsname.nodename, super::SYS_INFO.nodename);
        assert_eq!(utsname.release, super::SYS_INFO.release);
        assert_eq!(utsname.version, super::SYS_INFO.version);
        assert_eq!(utsname.machine, super::SYS_INFO.machine);
        assert_eq!(utsname.domainname, super::SYS_INFO.domainname);
    }
}
