// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The path-management/permissions/... layer, that sits above [`super::backend`].

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::fs::UserInfo;
use crate::path::Arg;
use crate::{LiteBox, fd::TypedFd, sync};

use super::errors::{
    ChmodError, ChownError, CloseError, FileStatusError, LinkError, MkdirError, OpenError,
    PathError, ReadDirError, ReadError, ReadLinkError, RenameError, RmdirError, SeekError,
    SetTimesError, SymlinkError, TruncateError, UnlinkError, WalkError, WriteError,
};
use super::{
    FileType, Mode, OFlags, Timestamp,
    backend::{
        DirHandle, FileHandle, PermissionCheck, PermissionInfo, SeekBehavior, WalkOutcome,
        WalkStopReason, WalkingDirHandle,
    },
};

/// The north-facing filesystem entry point, generic over a [`Backend`](super::backend::Backend).
///
/// The resolver _itself_ maintains no state; all state is maintained either by the backend or the
/// [`Context`]. The user may choose to store the [`Context`] as they wish.
// NOTE(jayb): the `Context` separation is in preparation for multi-process support; specifically,
// each guest process would have their own `Context` but would share the resolver. Currently, since
// we are using the `FileSystem` trait for migration, the interfaces do not show the full actual
// separated context support (yet!). Nonetheless, future changes will separate this out.
pub struct Resolver<
    Platform: sync::RawSyncPrimitivesProvider,
    Backend: super::backend::Backend + 'static,
> {
    litebox: LiteBox<Platform>,
    backend: Backend,
}

impl<Platform: sync::RawSyncPrimitivesProvider, Backend: super::backend::Backend + 'static>
    Resolver<Platform, Backend>
{
    /// Construct a new resolver over a `backend`.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>, backend: Backend) -> Self {
        Self {
            litebox: litebox.clone(),
            backend,
        }
    }
}

/// Per-call resolution context.  The user may hold and mutate this as they wish.
#[derive(Clone, Debug)]
pub struct Context {
    /// Current working directory.
    ///
    /// An empty list is equivalent to `/`. Guaranteed to never have `.` or `..`.
    cwd: Vec<String>,
    /// Effective user for permission checks.
    user_info: UserInfo,
}

impl Context {
    /// A new default context, anchored at `/` for a non-root user.
    pub fn new() -> Context {
        Self {
            cwd: vec![],
            user_info: UserInfo {
                user: 1000,
                group: 1000,
            },
        }
    }

    /// Resolve `path` against the current context.
    // XXX(jayb): if/when we support chroot, we might need to tweak this to not allow "escaping"
    // outside the chrooted part.
    // XXX(jayb): since we are migrating all resolution into the resolver, we probably don't need
    // `Arg` anymore, so could get rid of it in the future.
    fn resolve(&self, path: impl Arg) -> Result<ResolvedPath, PathError> {
        let mut components = if path.as_rust_str()?.starts_with('/') {
            vec![]
        } else {
            self.cwd.clone()
        };
        for component in path.components()? {
            match component {
                "" | "." => {}
                ".." => {
                    let _ = components.pop();
                }
                _ => {
                    components.push(component.into());
                }
            }
        }
        Ok(ResolvedPath { components })
    }

    fn can_execute(&self, permissions: &PermissionInfo) -> bool {
        if self.user_info.user == permissions.owner.user {
            permissions.mode.contains(Mode::XUSR)
        } else if self.user_info.group == permissions.owner.group {
            permissions.mode.contains(Mode::XGRP)
        } else {
            permissions.mode.contains(Mode::XOTH)
        }
    }

    fn can_read(&self, permissions: &PermissionInfo) -> bool {
        if self.user_info.user == permissions.owner.user {
            permissions.mode.contains(Mode::RUSR)
        } else if self.user_info.group == permissions.owner.group {
            permissions.mode.contains(Mode::RGRP)
        } else {
            permissions.mode.contains(Mode::ROTH)
        }
    }

    fn can_write(&self, permissions: &PermissionInfo) -> bool {
        if self.user_info.user == permissions.owner.user {
            permissions.mode.contains(Mode::WUSR)
        } else if self.user_info.group == permissions.owner.group {
            permissions.mode.contains(Mode::WGRP)
        } else {
            permissions.mode.contains(Mode::WOTH)
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum number of intermediate-symlink hops a single walk will follow before giving up with
/// `PathError::TooManySymlinkHops` (`ELOOP`). Matches
/// [`super::in_mem::FileSystem`]'s own `MAX_SYMLINK_HOPS` for its (upper/writable) layer.
const MAX_SYMLINK_HOPS: u32 = 8;

/// Absolute normalized path, must only be created from [`Context::resolve`].
struct ResolvedPath {
    components: Vec<String>,
}

impl ResolvedPath {
    fn parent_and_name(&self) -> Option<(Vec<&str>, &str)> {
        let (name, parent) = self.components.split_last()?;
        Some((parent.iter().map(String::as_str).collect(), name.as_str()))
    }
}

impl<Platform: sync::RawSyncPrimitivesProvider, Backend: super::backend::Backend + 'static>
    super::private::Sealed for Resolver<Platform, Backend>
{
}

impl<Platform: sync::RawSyncPrimitivesProvider, Backend: super::backend::Backend + 'static>
    Resolver<Platform, Backend>
{
    fn parent_dir_and_name<'a>(
        &self,
        context: &Context,
        path: &'a ResolvedPath,
    ) -> Result<Option<(WalkingDirHandle<'_>, &'a str)>, WalkError> {
        // Return the walking handle rather than an owned directory handle so backends can keep any
        // locks acquired during path resolution held across the final operation. This lets e.g.
        // "walk parent + mutate child" stay atomic.
        let Some((parent_components, name)) = path.parent_and_name() else {
            return Ok(None);
        };
        let parent = self.walk_to_directory(
            context,
            self.backend.root(),
            &parent_components,
            #[cfg(debug_assertions)]
            &parent_components,
        )?;
        Ok(Some((parent, name)))
    }

    fn owned_parent_dir(&self, dir: WalkingDirHandle<'_>) -> Result<DirHandle, WalkError> {
        self.backend
            .owned_dir_at(dir, OFlags::PATH)
            .map_err(|error| match error {
                OpenError::PathError(PathError::NoSuchFileOrDirectory) => {
                    PathError::MissingComponent.into()
                }
                OpenError::PathError(error) => error.into(),
                _ => WalkError::Io,
            })
    }

    fn walk_to_directory<'a>(
        &'a self,
        context: &Context,
        from: WalkingDirHandle<'a>,
        components: &[&str],
        #[cfg(debug_assertions)] absolute_components: &[&str],
    ) -> Result<WalkingDirHandle<'a>, WalkError> {
        if components.is_empty() {
            // TODO(jayb): Decide whether empty walks from a non-root handle need permission checks.
            return Ok(from);
        }

        let (outcome, walked) = self.walk_path_following_symlinks(
            context,
            from,
            components,
            #[cfg(debug_assertions)]
            absolute_components,
        )?;

        match outcome.stop_reason {
            WalkStopReason::CompleteDirectory => {
                assert_eq!(walked, components.len());
                Ok(outcome.last)
            }
            WalkStopReason::StoppedAtNonDirectory => {
                Err(WalkError::PathError(PathError::ComponentNotADirectory))
            }
            WalkStopReason::Continue => {
                // TODO(jayb): Continue walking from `outcome.last` once partial backend walks are
                // supported by the resolver.
                unimplemented!("partial backend walks are not supported yet")
            }
        }
    }

    fn walk_path<'a>(
        &'a self,
        context: &Context,
        from: WalkingDirHandle<'a>,
        components: &[&str],
        #[cfg(debug_assertions)] absolute_components: &[&str],
    ) -> Result<(WalkOutcome<WalkingDirHandle<'a>>, usize), WalkError> {
        assert!(!components.is_empty());
        self.walk_path_following_symlinks(
            context,
            from,
            components,
            #[cfg(debug_assertions)]
            absolute_components,
        )
    }

    /// Walk `components` from `from`, transparently following a symlink encountered in an
    /// *intermediate* (non-final) position -- e.g. Alpine's usrmerge `/lib -> usr/lib` -- up to
    /// [`MAX_SYMLINK_HOPS`] times, matching the same hop-limit idiom
    /// [`super::in_mem::FileSystem::resolve_final_symlinks`] uses for the writable upper layer.
    ///
    /// A symlink encountered at the requested *final* component is deliberately left unresolved
    /// here: callers that need the final component followed too (e.g. `open()` without
    /// `O_NOFOLLOW`) do so themselves once they have the resolved parent + leaf name, exactly as
    /// before this change. Only intermediate components -- which can never legitimately be
    /// anything but "a directory, or a symlink to one" -- are resolved inline, since a walk cannot
    /// otherwise continue through them.
    ///
    /// Returns the same shape `walk_directories` would: the final [`WalkOutcome`] plus how many of
    /// the *originally requested* `components` were consumed (which, after following any
    /// intermediate symlinks, no longer 1:1 corresponds to `outcome.components.len()`, hence
    /// returned separately).
    fn walk_path_following_symlinks<'a>(
        &'a self,
        context: &Context,
        // Every hop restarts the walk from the backend root (all current call sites already pass
        // `self.backend.root()` here, and a symlink target can point anywhere in the tree), since
        // `WalkingDirHandle` cannot cheaply be "rewound" to an intermediate point once a backend
        // has walked past it. The parameter is retained (rather than dropped in favor of an
        // internal `self.backend.root()` call) so this function's signature keeps documenting that
        // the walk is root-relative, matching `walk_to_directory`/`walk_path`'s existing contract.
        _from: WalkingDirHandle<'a>,
        components: &[&str],
        #[cfg(debug_assertions)] absolute_components: &[&str],
    ) -> Result<(WalkOutcome<WalkingDirHandle<'a>>, usize), WalkError> {
        // Owned, mutable working copy of the remaining path, so a symlink target can be spliced
        // in.
        let mut remaining: Vec<String> = components.iter().map(|c| (*c).to_string()).collect();
        let original_len = components.len();

        for _ in 0..MAX_SYMLINK_HOPS {
            let current: Vec<&str> = remaining.iter().map(String::as_str).collect();
            let outcome = self
                .backend
                .walk_directories(self.backend.root(), &current)?;
            Self::check_walk_permissions(
                context,
                #[cfg(debug_assertions)]
                absolute_components,
                &outcome,
            )?;

            let walked = outcome.components.len();
            let is_final_component_stop = outcome.stop_reason
                == WalkStopReason::StoppedAtNonDirectory
                && walked + 1 == current.len();
            if outcome.stop_reason == WalkStopReason::CompleteDirectory || is_final_component_stop {
                // Either fully walked, or stopped exactly at the requested final component (which
                // is allowed to be a non-directory, e.g. a file or a symlink the caller will
                // resolve itself) -- nothing left for us to do. The returned count must be
                // expressed as an index/length into the *original* `components` the caller passed
                // in (callers like `open`'s `components[walked]` index the original array with
                // it), not into `current`: a symlink hop only ever rewrites a *non-final* prefix of
                // the path (the final component, per this function's contract, is deliberately
                // left unresolved here), so the final element of `current` is always identical to
                // the final element of the original `components` regardless of how many hops
                // happened, and the count is simply `original_len` (fully walked) or
                // `original_len - 1` (stopped exactly at the original final component). This is
                // NOT derived from `current`'s length -- `current` is a rewritten working copy
                // whose total length is not monotonic across hops (a symlink target can expand to
                // more or fewer components than the single component it replaced), so comparing
                // `current.len()` against `original_len` could both underflow (a previous bug here)
                // and, even saturating, return a value that does not correspond to a valid index
                // into the caller's original array.
                let consumed = if is_final_component_stop {
                    original_len - 1
                } else {
                    original_len
                };
                return Ok((outcome, consumed));
            }

            // Stopped at a genuinely intermediate (non-final) component. If it names a symlink,
            // follow it transparently and retry; otherwise this is a real `ComponentNotADirectory`,
            // reported the same way `walk_to_directory`/`walk_path` always have. (`read_link_at`
            // consumes `outcome.last`, so we must have already decided we no longer need
            // `outcome` itself before calling it.)
            let symlink_component = current[walked];
            let Some(target) = self
                .backend
                .read_link_at(outcome.last, symlink_component)
                .ok()
                .flatten()
            else {
                return Err(WalkError::PathError(PathError::ComponentNotADirectory));
            };

            let mut new_remaining: Vec<String> = if let Some(target) = target.strip_prefix('/') {
                target
                    .split('/')
                    .filter(|c| !c.is_empty() && *c != ".")
                    .map(String::from)
                    .collect()
            } else {
                // Relative target: resolve against the directory containing the symlink, i.e. the
                // already-walked prefix (`current[..walked]`).
                let mut v: Vec<String> =
                    current[..walked].iter().map(|c| (*c).to_string()).collect();
                for component in target.split('/') {
                    match component {
                        "" | "." => {}
                        ".." => {
                            v.pop();
                        }
                        c => v.push(c.to_string()),
                    }
                }
                v
            };
            // Append whatever of the original request came after the symlink component itself.
            new_remaining.extend(current[walked + 1..].iter().map(|c| (*c).to_string()));
            remaining = new_remaining;
        }
        Err(WalkError::PathError(PathError::TooManySymlinkHops))
    }

    fn check_walk_permissions(
        context: &Context,
        #[cfg(debug_assertions)] absolute_components: &[&str],
        outcome: &WalkOutcome<WalkingDirHandle<'_>>,
    ) -> Result<(), PathError> {
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        for (idx, walked) in outcome.components.iter().enumerate() {
            match &walked.permissions {
                PermissionCheck::ByBackend => {}
                PermissionCheck::ByResolver(permissions) => {
                    if !context.can_execute(permissions) {
                        return Err(PathError::NoSearchPerms {
                            #[cfg(debug_assertions)]
                            dir: {
                                let mut path = String::new();
                                for component in &absolute_components[..=idx] {
                                    path.push('/');
                                    path.push_str(component);
                                }
                                path
                            },
                            #[cfg(debug_assertions)]
                            perms: permissions.mode,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// This exists purely as a migration feature, until we have completely separated contexts. See
/// comment on `Resolver`.
fn default_context_pre_context_management_changes() -> Context {
    Context::new()
}

impl<Platform: sync::RawSyncPrimitivesProvider, Backend: super::backend::Backend + 'static>
    super::FileSystem for Resolver<Platform, Backend>
{
    fn open(&self, path: impl Arg, flags: OFlags, mode: Mode) -> Result<TypedFd<Self>, OpenError> {
        const CURRENTLY_SUPPORTED_OFLAGS: OFlags = OFlags::CREAT
            .union(OFlags::RDONLY)
            .union(OFlags::WRONLY)
            .union(OFlags::RDWR)
            .union(OFlags::TRUNC)
            .union(OFlags::NOCTTY)
            .union(OFlags::EXCL)
            .union(OFlags::DIRECTORY)
            .union(OFlags::NONBLOCK)
            .union(OFlags::LARGEFILE)
            .union(OFlags::NOFOLLOW)
            .union(OFlags::APPEND)
            .union(OFlags::PATH);

        if flags.intersects(CURRENTLY_SUPPORTED_OFLAGS.complement()) {
            unimplemented!("{flags:?}")
        }
        let path_only = flags.contains(OFlags::PATH);

        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let access_mode = flags & (OFlags::WRONLY | OFlags::RDWR);
        let read_allowed = access_mode == OFlags::RDONLY || access_mode == OFlags::RDWR;
        let write_allowed = access_mode == OFlags::WRONLY || access_mode == OFlags::RDWR;
        let append_mode = flags.contains(OFlags::APPEND);
        let insert = |handle, seek_behavior| {
            self.litebox.descriptor_table_mut().insert(ResolverEntry {
                handle,
                _backend: core::marker::PhantomData,
                read_allowed,
                write_allowed,
                position: 0,
                append_mode,
                path_only,
                seek_behavior,
            })
        };

        if path.components.is_empty() {
            if flags.contains(OFlags::CREAT) && flags.contains(OFlags::EXCL) {
                return Err(OpenError::AlreadyExists);
            }
            return Ok(insert(
                OwnedHandle::Dir(self.backend.owned_dir_at(self.backend.root(), flags)?),
                SeekBehavior::NonSeekable,
            ));
        }

        let components: Vec<_> = path.components.iter().map(String::as_str).collect();
        let walk = self.walk_path(
            &context,
            self.backend.root(),
            &components,
            #[cfg(debug_assertions)]
            &components,
        );
        match walk {
            Ok((outcome, _)) if outcome.stop_reason == WalkStopReason::CompleteDirectory => {
                if flags.contains(OFlags::CREAT) && flags.contains(OFlags::EXCL) {
                    return Err(OpenError::AlreadyExists);
                }
                Ok(insert(
                    OwnedHandle::Dir(self.backend.owned_dir_at(outcome.last, flags)?),
                    SeekBehavior::NonSeekable,
                ))
            }
            Ok((outcome, walked))
                if outcome.stop_reason == WalkStopReason::StoppedAtNonDirectory =>
            {
                let name = components[walked];
                // TODO(jayb): Reject O_CREAT | O_EXCL before invoking the backend, so open-time
                // side effects like truncation cannot happen before AlreadyExists is returned.
                let file = self.backend.open_file_at(outcome.last, name, flags)?;
                if flags.contains(OFlags::CREAT) && flags.contains(OFlags::EXCL) {
                    return Err(OpenError::AlreadyExists);
                }
                if !path_only
                    && let PermissionCheck::ByResolver(permissions) = &file.permissions
                    && ((read_allowed && !context.can_read(permissions))
                        || (write_allowed && !context.can_write(permissions)))
                {
                    return Err(OpenError::AccessNotAllowed);
                }
                let seek_behavior = self.backend.seek_behavior(&file.item);
                Ok(insert(OwnedHandle::File(file.item), seek_behavior))
            }
            Ok(_) => {
                // `walk_path` validates stop reasons before returning.
                unreachable!()
            }
            Err(WalkError::PathError(PathError::NoSuchFileOrDirectory))
                if flags.contains(OFlags::CREAT) =>
            {
                let Some((parent_components, name)) = path.parent_and_name() else {
                    unreachable!("root path was handled above")
                };
                let parent = self
                    .walk_to_directory(
                        &context,
                        self.backend.root(),
                        &parent_components,
                        #[cfg(debug_assertions)]
                        &parent_components,
                    )
                    .map_err(|error| match error {
                        WalkError::Io => OpenError::Io,
                        WalkError::PathError(error) => error.into(),
                    })?;
                let parent = self.owned_parent_dir(parent).map_err(|error| match error {
                    WalkError::Io => OpenError::Io,
                    WalkError::PathError(error) => error.into(),
                })?;
                let file = self.backend.create_file_at(parent, name, mode)?;
                let seek_behavior = self.backend.seek_behavior(&file);
                Ok(insert(OwnedHandle::File(file), seek_behavior))
            }
            Err(error) => match error {
                WalkError::Io => Err(OpenError::Io),
                WalkError::PathError(error) => Err(error.into()),
            },
        }
    }

    fn close(&self, fd: &TypedFd<Self>) -> Result<(), CloseError> {
        self.litebox.descriptor_table_mut().remove(fd);
        Ok(())
    }

    fn read(
        &self,
        fd: &TypedFd<Self>,
        buf: &mut [u8],
        offset: Option<usize>,
    ) -> Result<usize, ReadError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(ReadError::ClosedFd)?;
        let mut entry = entry.get_entry_mut();
        // XXX(jayb): This over-holds the descriptor-entry lock across backend I/O. We need a
        // smaller per-open-file-description primitive for position/append serialization, so the
        // descriptor entry can be unlocked before potentially blocking backend calls.
        let file = match &entry.entry.handle {
            OwnedHandle::File(file) => file,
            OwnedHandle::Dir(_) => return Err(ReadError::NotAFile),
        };
        let seek_behavior = entry.entry.seek_behavior;
        if !entry.entry.read_allowed {
            return Err(ReadError::NotForReading);
        }
        if entry.entry.path_only {
            return Err(ReadError::NotForReading);
        }

        let read_offset = match seek_behavior {
            SeekBehavior::NonSeekable | SeekBehavior::ZeroPosition => 0,
            SeekBehavior::PositionBased => offset.unwrap_or(entry.entry.position),
        };
        let read = self.backend.read(file, buf, read_offset)?;
        if matches!(seek_behavior, SeekBehavior::PositionBased) && offset.is_none() {
            entry.entry.position = read_offset.checked_add(read).unwrap();
        }
        Ok(read)
    }

    fn write(
        &self,
        fd: &TypedFd<Self>,
        buf: &[u8],
        offset: Option<usize>,
    ) -> Result<usize, WriteError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(WriteError::ClosedFd)?;
        let mut entry = entry.get_entry_mut();
        // XXX(jayb): This over-holds the descriptor-entry lock across backend I/O. We need a
        // smaller per-open-file-description primitive for position/append serialization, so the
        // descriptor entry can be unlocked before potentially blocking backend calls.
        let file = match &entry.entry.handle {
            OwnedHandle::File(file) => file,
            OwnedHandle::Dir(_) => return Err(WriteError::NotAFile),
        };
        let seek_behavior = entry.entry.seek_behavior;
        if !entry.entry.write_allowed {
            return Err(WriteError::NotForWriting);
        }
        if entry.entry.path_only {
            return Err(WriteError::NotForWriting);
        }

        let write_offset = match seek_behavior {
            SeekBehavior::NonSeekable | SeekBehavior::ZeroPosition => 0,
            SeekBehavior::PositionBased if entry.entry.append_mode && offset.is_none() => {
                self.backend
                    .file_status(file)
                    .map_err(|_| WriteError::Io)?
                    .size
            }
            SeekBehavior::PositionBased => offset.unwrap_or(entry.entry.position),
        };
        let written = self.backend.write(file, buf, write_offset)?;
        if matches!(seek_behavior, SeekBehavior::PositionBased) && offset.is_none() {
            entry.entry.position = write_offset.checked_add(written).unwrap();
        }
        Ok(written)
    }

    fn seek(
        &self,
        fd: &TypedFd<Self>,
        offset: isize,
        whence: super::SeekWhence,
    ) -> Result<usize, SeekError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(SeekError::ClosedFd)?;
        let mut entry = entry.get_entry_mut();
        let file = match &entry.entry.handle {
            OwnedHandle::File(file) => file,
            OwnedHandle::Dir(_) => return Err(SeekError::NotAFile),
        };
        if entry.entry.path_only {
            return Err(SeekError::PathOnlyFd);
        }

        match entry.entry.seek_behavior {
            SeekBehavior::NonSeekable => Err(SeekError::NonSeekable),
            SeekBehavior::ZeroPosition => Ok(0),
            SeekBehavior::PositionBased => {
                let file_len = self
                    .backend
                    .file_status(file)
                    .map_err(|_| SeekError::Io)?
                    .size;
                let base = match whence {
                    super::SeekWhence::RelativeToBeginning => 0,
                    super::SeekWhence::RelativeToCurrentOffset => entry.entry.position,
                    super::SeekWhence::RelativeToEnd => file_len,
                };
                let new_position = base
                    .checked_add_signed(offset)
                    .ok_or(SeekError::InvalidOffset)?;
                // TODO(jayb): Linux allows regular files to seek past EOF, while some backends or
                // file types may not. Model that distinction instead of using one resolver rule.
                if new_position > file_len {
                    return Err(SeekError::InvalidOffset);
                }
                entry.entry.position = new_position;
                Ok(new_position)
            }
        }
    }

    fn truncate(
        &self,
        fd: &TypedFd<Self>,
        length: usize,
        reset_offset: bool,
    ) -> Result<(), TruncateError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(TruncateError::ClosedFd)?;
        let mut entry = entry.get_entry_mut();
        let file = match &entry.entry.handle {
            OwnedHandle::File(file) => file,
            OwnedHandle::Dir(_) => return Err(TruncateError::IsDirectory),
        };
        if !entry.entry.write_allowed {
            return Err(TruncateError::NotForWriting);
        }
        if entry.entry.path_only {
            return Err(TruncateError::PathOnlyFd);
        }

        self.backend.truncate(file, length)?;
        if reset_offset {
            entry.entry.position = 0;
        }
        Ok(())
    }

    fn chmod_fd(&self, fd: &TypedFd<Self>, mode: Mode) -> Result<(), ChmodError> {
        // Mirrors `truncate` above (see [`super::FileSystem::chmod_fd`]'s doc comment on why
        // this must operate on the already-open handle rather than re-resolving `fd` to a path)
        // -- `Backend::chmod` is likewise scoped to `FileHandle` only, so a directory fd is
        // rejected here the same way `truncate` rejects one, rather than silently no-op'ing.
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(ChmodError::Io)?;
        let entry = entry.get_entry_mut();
        let file = match &entry.entry.handle {
            OwnedHandle::File(file) => file,
            // `Backend::chmod` is scoped to `FileHandle` only (see its own doc comment) -- no
            // backend in this codebase currently needs `fchmod` on a directory fd, matching
            // `truncate`'s identical `OwnedHandle::Dir` handling just above (`TruncateError::
            // IsDirectory`). Real Linux `fchmod` on a directory fd is valid, but nothing in this
            // codebase's actual call sites (wlroots' shm-file dance, the only real `fchmod`
            // caller) ever targets a directory fd, so this stays a hard error rather than
            // growing `Backend`'s surface for an unexercised case.
            OwnedHandle::Dir(_) => return Err(ChmodError::Io),
        };
        self.backend.chmod(file, mode)
    }

    fn chmod(&self, path: impl Arg, mode: Mode) -> Result<(), ChmodError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => ChmodError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            // TODO(jayb): Add backend support for mutating the root directory itself.
            unimplemented!("chmod root directory")
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => ChmodError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.chmod_at(parent, name, mode)
    }

    fn chown(
        &self,
        path: impl Arg,
        user: Option<u16>,
        group: Option<u16>,
    ) -> Result<(), ChownError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => ChownError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            // TODO(jayb): Add backend support for mutating the root directory itself.
            unimplemented!("chown root directory")
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => ChownError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.chown_at(parent, name, user, group)
    }

    fn set_times(
        &self,
        path: impl Arg,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
    ) -> Result<(), SetTimesError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => SetTimesError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            // TODO(jayb): Add backend support for mutating the root directory itself.
            unimplemented!("set_times root directory")
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => SetTimesError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.set_times_at(parent, name, atime, mtime)
    }

    fn unlink(&self, path: impl Arg) -> Result<(), UnlinkError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => UnlinkError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            return Err(UnlinkError::IsADirectory);
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => UnlinkError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.unlink_at(parent, name)
    }

    fn rename(&self, from: impl Arg, to: impl Arg) -> Result<(), RenameError> {
        // `Backend` (the `Composer`-based mount abstraction this `Resolver` sits on) has no
        // `rename_at` operation: every current use of `Resolver<Composer>` in this codebase
        // mounts either genuinely read-only backends (`tar_ro`, the OCI image's read-only rootfs
        // layer) or device backends (`devices`, `/dev`) -- neither can meaningfully support
        // rename, so reporting `ReadOnlyFileSystem` here is accurate rather than a stub. The
        // writable rename path that matters (e.g. `apk` atomically replacing a downloaded temp
        // file) goes through `in_mem::FileSystem::rename` instead, since the writable "upper"
        // layer in this codebase's `layered::FileSystem` setup is always a plain `in_mem`
        // filesystem, never a `Resolver<Composer>`.
        let _ = (from, to);
        Err(RenameError::ReadOnlyFileSystem)
    }

    fn link(&self, oldpath: impl Arg, newpath: impl Arg) -> Result<(), LinkError> {
        // Same rationale as `rename` above: every current use of `Resolver<Composer>` mounts
        // either a genuinely read-only backend (`tar_ro`) or a device backend (`devices`, `/dev`),
        // neither of which can meaningfully support creating a new hard link.
        let _ = (oldpath, newpath);
        Err(LinkError::ReadOnlyFileSystem)
    }

    fn symlink(&self, target: impl Arg, linkpath: impl Arg) -> Result<(), SymlinkError> {
        // Same rationale as `rename` above: every current use of `Resolver<Composer>` mounts
        // either a genuinely read-only backend (`tar_ro`) or a device backend (`devices`, `/dev`),
        // neither of which can meaningfully support creating a new symlink.
        let _ = (target, linkpath);
        Err(SymlinkError::ReadOnlyFileSystem)
    }

    fn read_link(&self, path: impl Arg) -> Result<String, ReadLinkError> {
        // Backends can carry real symlinks now (see `Backend::read_link_at`, needed so
        // intermediate-component symlinks like Alpine's usrmerge `/lib -> usr/lib` can be followed
        // during a walk) even though `symlink()`/`rename()` above remain unsupported for
        // *creating* new entries -- every current mount through this resolver is still read-only
        // (`tar_ro`) or a device backend (`devices`) with nothing to write.
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => ReadLinkError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            // The root itself was requested; it is always a directory, never a symlink.
            return Err(ReadLinkError::NotASymlink);
        };
        match self.backend.read_link_at(parent, name) {
            Ok(Some(target)) => Ok(target),
            Ok(None) => Err(ReadLinkError::NotASymlink),
            Err(OpenError::PathError(error)) => Err(error.into()),
            Err(
                OpenError::Io
                | OpenError::AccessNotAllowed
                | OpenError::NoWritePerms
                | OpenError::ReadOnlyFileSystem
                | OpenError::AlreadyExists
                | OpenError::TruncateError(_),
            ) => Err(ReadLinkError::Io),
        }
    }

    fn mkdir(&self, path: impl Arg, mode: Mode) -> Result<(), MkdirError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => MkdirError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            return Err(MkdirError::AlreadyExists);
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => MkdirError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.mkdir_at(parent, name, mode).map(|_| ())
    }

    fn rmdir(&self, path: impl Arg) -> Result<(), RmdirError> {
        let context = default_context_pre_context_management_changes();
        let path = context.resolve(path)?;
        let Some((parent, name)) =
            self.parent_dir_and_name(&context, &path)
                .map_err(|error| match error {
                    WalkError::Io => RmdirError::Io,
                    WalkError::PathError(error) => error.into(),
                })?
        else {
            return Err(RmdirError::Busy);
        };
        let parent = self.owned_parent_dir(parent).map_err(|error| match error {
            WalkError::Io => RmdirError::Io,
            WalkError::PathError(error) => error.into(),
        })?;
        self.backend.rmdir_at(parent, name)
    }

    fn read_dir(&self, fd: &TypedFd<Self>) -> Result<Vec<super::DirEntry>, ReadDirError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(ReadDirError::ClosedFd)?;
        let entry = entry.get_entry();
        if entry.entry.path_only {
            return Err(ReadDirError::PathOnlyFd);
        }
        let dir = match &entry.entry.handle {
            OwnedHandle::File(_) => return Err(ReadDirError::NotADirectory),
            OwnedHandle::Dir(dir) => dir,
        };

        let mut entries = Vec::new();
        // TODO(jayb): Fill in inode info for synthesized dot entries.
        entries.push(super::DirEntry {
            name: String::from("."),
            file_type: FileType::Directory,
            ino_info: None,
        });
        entries.push(super::DirEntry {
            name: String::from(".."),
            file_type: FileType::Directory,
            ino_info: None,
        });
        entries.extend(self.backend.list_dir_at(dir.clone())?);
        Ok(entries)
    }

    fn file_status(&self, path: impl Arg) -> Result<super::FileStatus, FileStatusError> {
        let fd = self
            .open(path, OFlags::PATH, Mode::empty())
            .map_err(|error| match error {
                OpenError::PathError(error) => error.into(),
                OpenError::Io
                | OpenError::AccessNotAllowed
                | OpenError::NoWritePerms
                | OpenError::ReadOnlyFileSystem
                | OpenError::AlreadyExists
                | OpenError::TruncateError(_) => FileStatusError::Io,
            })?;
        let status = self.fd_file_status(&fd);
        self.close(&fd).unwrap();
        status
    }

    fn symlink_metadata(&self, path: impl Arg) -> Result<super::FileStatus, FileStatusError> {
        // `open()` (and therefore `file_status` above) always transparently follows a
        // final-component symlink -- backends never see one reach their own `open_file_at`, by
        // design (see `Backend::read_link_at`'s doc comment). So check the final component
        // directly via the same `read_link_at` primitive `Self::read_link` uses, *before* ever
        // calling `open()`/`file_status`: a dangling symlink's target need not exist for this to
        // succeed, whereas routing through `file_status` would incorrectly surface `ENOENT`.
        let context = default_context_pre_context_management_changes();
        let resolved = context.resolve(path)?;
        let Some((parent, name)) = self.parent_dir_and_name(&context, &resolved).map_err(
            |error| match error {
                WalkError::Io => FileStatusError::Io,
                WalkError::PathError(error) => error.into(),
            },
        )?
        else {
            // The root itself was requested; it is always a directory, never a symlink.
            return self.file_status("/");
        };
        match self.backend.read_link_at(parent, name) {
            Ok(Some(target)) => {
                // Reuse the containing directory's own status for the fields a symlink has no
                // independent, meaningful value for (owner/timestamps/block size) -- matching
                // this crate's existing precedent of approximating metadata a backend doesn't
                // track natively rather than inventing an unrelated placeholder.
                let Some((parent_components, _)) = resolved.parent_and_name() else {
                    unreachable!("a symlink can never be the root");
                };
                let dir_path = alloc::format!("/{}", parent_components.join("/"));
                let dir_status = self.file_status(dir_path)?;
                Ok(super::FileStatus::symlink(
                    target.len(),
                    dir_status.owner,
                    dir_status.node_info,
                    dir_status.blksize,
                    dir_status.atime,
                    dir_status.mtime,
                ))
            }
            Ok(None) => {
                let full_path = alloc::format!("/{}", resolved.components.join("/"));
                self.file_status(full_path)
            }
            Err(OpenError::PathError(error)) => Err(error.into()),
            Err(
                OpenError::Io
                | OpenError::AccessNotAllowed
                | OpenError::NoWritePerms
                | OpenError::ReadOnlyFileSystem
                | OpenError::AlreadyExists
                | OpenError::TruncateError(_),
            ) => Err(FileStatusError::Io),
        }
    }

    fn fd_file_status(&self, fd: &TypedFd<Self>) -> Result<super::FileStatus, FileStatusError> {
        let entry = self
            .litebox
            .descriptor_table()
            .entry_handle(fd)
            .ok_or(FileStatusError::ClosedFd)?;
        let entry = entry.get_entry();
        match &entry.entry.handle {
            OwnedHandle::File(file) => self.backend.file_status(file),
            OwnedHandle::Dir(dir) => self.backend.dir_status(dir),
        }
    }

    fn get_static_backing_data(&self, fd: &TypedFd<Self>) -> Option<&'static [u8]> {
        let entry = self.litebox.descriptor_table().entry_handle(fd)?;
        let entry = entry.get_entry();
        match &entry.entry.handle {
            OwnedHandle::File(file) => self.backend.get_static_backing_data(file),
            OwnedHandle::Dir(_) => None,
        }
    }
}

/// A file or a directory handle
enum OwnedHandle {
    File(FileHandle),
    Dir(DirHandle),
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "resolver fd entries carry independent descriptor flags"
)]
struct ResolverEntry<Backend: super::backend::Backend> {
    handle: OwnedHandle,
    _backend: core::marker::PhantomData<Backend>,
    read_allowed: bool,
    write_allowed: bool,
    position: usize,
    append_mode: bool,
    path_only: bool,
    seek_behavior: SeekBehavior,
}

crate::fd::enable_fds_for_subsystem! {
    @ Platform: { sync::RawSyncPrimitivesProvider }, Backend: { super::backend::Backend + 'static };
    Resolver<Platform, Backend>;
    @ Backend: { super::backend::Backend + 'static };
    ResolverEntry<Backend>;
    -> ResolverFd<Platform, Backend>;
}
