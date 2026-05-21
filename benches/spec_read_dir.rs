#![feature(min_specialization)]
#![feature(io_const_error)]
#![feature(ffi_const)]
#![feature(try_blocks)]
#![feature(decl_macro)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![feature(linkage)]
#![feature(macro_metavar_expr_concat)]
#![feature(generic_atomic)]
#![allow(dead_code)]
#![allow(unused)]
#![allow(non_camel_case_types)]

use std::{borrow::Cow, ffi::{CStr, CString, OsStr, OsString, c_char, c_int}, hint::black_box, io::{self, Error, ErrorKind}, mem::{self, MaybeUninit}, os::unix::ffi::OsStrExt, path::{Path, PathBuf}, ptr, slice, sync::Arc};

use criterion::{Criterion, criterion_group, criterion_main};
use libc::{lstat64, mode_t, off64_t, stat64};

// FIXME: This should be available on Linux with all `target_env`.
// But currently only glibc exposes `statx` fn and structs.
// We don't want to import unverified raw C structs here directly.
// https://github.com/rust-lang/rust/pull/67774
macro_rules! cfg_has_statx {
    ({ $($then_tt:tt)* } else { $($else_tt:tt)* }) => {
        cfg_select! {
            all(target_os = "linux", target_env = "gnu") => {
                $($then_tt)*
            }
            _ => {
                $($else_tt)*
            }
        }
    };
    ($($block_inner:tt)*) => {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            $($block_inner)*
        }
    };
}
// cfg_select! {
//     // On non-ELF targets, use the dlsym approximation of weak linkage.
//     target_vendor = "apple" => {
//         mod dlsym;
//         pub(crate) use dlsym::weak;
//     }

//     // Some targets don't need and support weak linkage at all...
//     target_os = "espidf" => {}

//     // ... but ELF targets support true weak linkage.
//     _ => {
//         // There are a variety of `#[cfg]`s controlling which targets are involved in
//         // each instance of `weak!`. Rather than trying to unify all of
//         // that, we'll just allow that some unix targets don't use this macro at all.
//         #[cfg_attr(not(target_os = "linux"), allow(unused_macros, dead_code))]
//         mod weak_linkage;
//         #[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
//         pub(crate) use weak_linkage::weak;
//     }
// }

pub(crate) macro weak {
    (fn $name:ident($($param:ident : $t:ty),* $(,)?) -> $ret:ty;) => (
        let ref $name: ExternWeak<unsafe extern "C" fn($($t),*) -> $ret> = {
            unsafe extern "C" {
                #[linkage = "extern_weak"]
                static $name: Option<unsafe extern "C" fn($($t),*) -> $ret>;
            }
            #[allow(unused_unsafe)]
            ExternWeak::new(unsafe { $name })
        };
    )
}

pub(crate) struct ExternWeak<F: Copy> {
    weak_ptr: Option<F>,
}

impl<F: Copy> ExternWeak<F> {
    #[inline]
    pub fn new(weak_ptr: Option<F>) -> Self {
        ExternWeak { weak_ptr }
    }

    #[inline]
    pub fn get(&self) -> Option<F> {
        self.weak_ptr
    }
}

// Make sure to stay under 4096 so the compiler doesn't insert a probe frame:
// https://docs.rs/compiler_builtins/latest/compiler_builtins/probestack/index.html
#[cfg(not(target_os = "espidf"))]
const MAX_STACK_ALLOCATION: usize = 384;
#[cfg(target_os = "espidf")]
const MAX_STACK_ALLOCATION: usize = 32;

const NUL_ERR: io::Error =
    io::const_error!(io::ErrorKind::InvalidInput, "file name contained an unexpected NUL byte");

// all DirEntry's will have a reference to this struct
struct InnerReadDir {
    dirp: DirStream,
    root: PathBuf,
}

pub struct ReadDir {
    inner: Arc<InnerReadDir>,
    end_of_stream: bool,
}

impl ReadDir {
    fn new(inner: InnerReadDir) -> Self {
        Self { inner: Arc::new(inner), end_of_stream: false }
    }
}

/// Specialization trait used to construct a `ReadDir`
trait ReadDirFromPath<P> {
    fn from_path(dirp: DirStream, path: P) -> Self;
}

impl<P: AsRef<Path>> ReadDirFromPath<P> for ReadDir {
    default fn from_path(dirp: DirStream, path: P) -> Self {
        let inner = InnerReadDir { dirp, root: path.as_ref().to_path_buf() };
        ReadDir::new(inner)
    }
}

/// This constructs a `ReadDir` for all types that can be converted
/// into `PathBuf` without allocating
macro_rules! impl_read_dir_from_path {
    ($t:ty) => {
        impl ReadDirFromPath<$t> for ReadDir {
            fn from_path(dirp: DirStream, path: $t) -> Self {
                let inner = InnerReadDir { dirp, root: path.into() };
                ReadDir::new(inner)
            }
        }
    };
}

impl_read_dir_from_path!(PathBuf);
impl_read_dir_from_path!(Box<Path>);
impl_read_dir_from_path!(Cow<'_, Path>);
impl_read_dir_from_path!(OsString);
impl_read_dir_from_path!(String);

struct DirStream(*mut libc::DIR);

fn readdir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let ptr = run_path_with_cstr(path.as_ref(), &|p| unsafe { Ok(libc::opendir(p.as_ptr())) })?;
    if ptr.is_null() {
        Err(Error::last_os_error())
    } else {
        Ok(<ReadDir as ReadDirFromPath<P>>::from_path(DirStream(ptr), path))
    }
}

#[allow(non_camel_case_types)]
struct dirent64_min {
    d_ino: u64,
    #[cfg(not(any(
        target_os = "solaris",
        target_os = "illumos",
        target_os = "aix",
        target_os = "nto",
        target_os = "vita",
    )))]
    d_type: u8,
}

pub struct DirEntry {
    dir: Arc<InnerReadDir>,
    entry: dirent64_min,
    // We need to store an owned copy of the entry name on platforms that use
    // readdir() (not readdir_r()), because a) struct dirent may use a flexible
    // array to store the name, b) it lives only until the next readdir() call.
    name: CString,
}

/// Returns the platform-specific value of errno
#[cfg(not(any(target_os = "dragonfly", target_os = "vxworks", target_os = "rtems")))]
#[inline]
fn errno() -> i32 {
    unsafe { (*errno_location()) as i32 }
}
#[cfg(all(not(target_os = "dragonfly"), not(target_os = "vxworks"), not(target_os = "rtems")))]
#[allow(dead_code)] // but not all target cfgs actually end up using it
#[inline]
fn set_errno(e: i32) {
    unsafe { *errno_location() = e as c_int }
}

#[inline]
pub fn is_interrupted(errno: i32) -> bool {
    errno == libc::EINTR
}

pub fn errno_is_interrupted(error: &Error) -> bool {
    if error.kind() == ErrorKind::Interrupted {
        return true;
    } else if is_interrupted(error.raw_os_error().unwrap_or(0)) {
        return true;
    }
    false
}

unsafe extern "C" {
    #[cfg(not(any(target_os = "dragonfly", target_os = "vxworks", target_os = "rtems")))]
    #[cfg_attr(
        any(
            target_os = "linux",
            target_os = "emscripten",
            target_os = "fuchsia",
            target_os = "l4re",
            target_os = "hurd",
        ),
        link_name = "__errno_location"
    )]
    #[cfg_attr(
        any(
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "cygwin",
            target_os = "android",
            target_os = "redox",
            target_os = "nuttx",
            target_env = "newlib"
        ),
        link_name = "__errno"
    )]
    #[cfg_attr(any(target_os = "solaris", target_os = "illumos"), link_name = "___errno")]
    #[cfg_attr(target_os = "nto", link_name = "__get_errno_ptr")]
    #[cfg_attr(any(target_os = "freebsd", target_vendor = "apple"), link_name = "__error")]
    #[cfg_attr(target_os = "haiku", link_name = "_errnop")]
    #[cfg_attr(target_os = "aix", link_name = "_Errno")]
    // SAFETY: this will always return the same pointer on a given thread.
    #[unsafe(ffi_const)]
    pub safe fn errno_location() -> *mut c_int;
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    #[cfg(any(
        target_os = "aix",
        target_os = "android",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "linux",
        target_os = "nto",
        target_os = "redox",
        target_os = "solaris",
        target_os = "vita",
        target_os = "wasi",
    ))]
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        // use crate::sys::io::{errno, set_errno};

        if self.end_of_stream {
            return None;
        }

        unsafe {
            loop {
                // As of POSIX.1-2017, readdir() is not required to be thread safe; only
                // readdir_r() is. However, readdir_r() cannot correctly handle platforms
                // with unlimited or variable NAME_MAX. Many modern platforms guarantee
                // thread safety for readdir() as long an individual DIR* is not accessed
                // concurrently, which is sufficient for Rust.

                use libc::{dirent64, readdir64};
                set_errno(0);
                let entry_ptr: *const dirent64 = readdir64(self.inner.dirp.0);
                if entry_ptr.is_null() {
                    // We either encountered an error, or reached the end. Either way,
                    // the next call to next() should return None.
                    self.end_of_stream = true;

                    // To distinguish between errors and end-of-directory, we had to clear
                    // errno beforehand to check for an error now.
                    return match errno() {
                        0 => None,
                        e => Some(Err(Error::from_raw_os_error(e))),
                    };
                }

                // The dirent64 struct is a weird imaginary thing that isn't ever supposed
                // to be worked with by value. Its trailing d_name field is declared
                // variously as [c_char; 256] or [c_char; 1] on different systems but
                // either way that size is meaningless; only the offset of d_name is
                // meaningful. The dirent64 pointers that libc returns from readdir64 are
                // allowed to point to allocations smaller _or_ LARGER than implied by the
                // definition of the struct.
                //
                // As such, we need to be even more careful with dirent64 than if its
                // contents were "simply" partially initialized data.
                //
                // Like for uninitialized contents, converting entry_ptr to `&dirent64`
                // would not be legal. However, we can use `&raw const (*entry_ptr).d_name`
                // to refer the fields individually, because that operation is equivalent
                // to `byte_offset` and thus does not require the full extent of `*entry_ptr`
                // to be in bounds of the same allocation, only the offset of the field
                // being referenced.

                // d_name is guaranteed to be null-terminated.
                let name = CStr::from_ptr((&raw const (*entry_ptr).d_name).cast());
                let name_bytes = name.to_bytes();
                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }

                // When loading from a field, we can skip the `&raw const`; `(*entry_ptr).d_ino` as
                // a value expression will do the right thing: `byte_offset` to the field and then
                // only access those bytes.
                #[cfg(not(target_os = "vita"))]
                let entry = dirent64_min {
                    #[cfg(target_os = "freebsd")]
                    d_ino: (*entry_ptr).d_fileno,
                    #[cfg(not(target_os = "freebsd"))]
                    d_ino: (*entry_ptr).d_ino as u64,
                    #[cfg(not(any(
                        target_os = "solaris",
                        target_os = "illumos",
                        target_os = "aix",
                        target_os = "nto",
                    )))]
                    d_type: (*entry_ptr).d_type as u8,
                };

                #[cfg(target_os = "vita")]
                let entry = dirent64_min { d_ino: 0u64 };

                return Some(Ok(DirEntry {
                    entry,
                    name: name.to_owned(),
                    dir: Arc::clone(&self.inner),
                }));
            }
        }
    }

    #[cfg(not(any(
        target_os = "aix",
        target_os = "android",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "linux",
        target_os = "nto",
        target_os = "redox",
        target_os = "solaris",
        target_os = "vita",
        target_os = "wasi",
    )))]
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        if self.end_of_stream {
            return None;
        }

        unsafe {
            let mut ret = DirEntry { entry: mem::zeroed(), dir: Arc::clone(&self.inner) };
            let mut entry_ptr = ptr::null_mut();
            loop {
                let err = readdir64_r(self.inner.dirp.0, &mut ret.entry, &mut entry_ptr);
                if err != 0 {
                    if entry_ptr.is_null() {
                        // We encountered an error (which will be returned in this iteration), but
                        // we also reached the end of the directory stream. The `end_of_stream`
                        // flag is enabled to make sure that we return `None` in the next iteration
                        // (instead of looping forever)
                        self.end_of_stream = true;
                    }
                    return Some(Err(Error::from_raw_os_error(err)));
                }
                if entry_ptr.is_null() {
                    return None;
                }
                if ret.name_bytes() != b"." && ret.name_bytes() != b".." {
                    return Some(Ok(ret));
                }
            }
        }
    }
}

#[inline]
fn run_path_with_cstr<T>(path: &Path, f: &dyn Fn(&CStr) -> io::Result<T>) -> io::Result<T> {
    run_with_cstr(path.as_os_str().as_encoded_bytes(), f)
}

#[inline]
fn run_with_cstr<T>(bytes: &[u8], f: &dyn Fn(&CStr) -> io::Result<T>) -> io::Result<T> {
    // Dispatch and dyn erase the closure type to prevent mono bloat.
    // See https://github.com/rust-lang/rust/pull/121101.
    if bytes.len() >= MAX_STACK_ALLOCATION {
        run_with_cstr_allocating(bytes, f)
    } else {
        unsafe { run_with_cstr_stack(bytes, f) }
    }
}

/// # Safety
///
/// `bytes` must have a length less than `MAX_STACK_ALLOCATION`.
unsafe fn run_with_cstr_stack<T>(
    bytes: &[u8],
    f: &dyn Fn(&CStr) -> io::Result<T>,
) -> io::Result<T> {
    let mut buf = MaybeUninit::<[u8; MAX_STACK_ALLOCATION]>::uninit();
    let buf_ptr = buf.as_mut_ptr() as *mut u8;

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, bytes.len());
        buf_ptr.add(bytes.len()).write(0);
    }

    match CStr::from_bytes_with_nul(unsafe { slice::from_raw_parts(buf_ptr, bytes.len() + 1) }) {
        Ok(s) => f(s),
        Err(_) => Err(NUL_ERR),
    }
}

#[cold]
#[inline(never)]
fn run_with_cstr_allocating<T>(bytes: &[u8], f: &dyn Fn(&CStr) -> io::Result<T>) -> io::Result<T> {
    match CString::new(bytes) {
        Ok(s) => f(&s),
        Err(_) => Err(NUL_ERR),
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FilePermissions {
    mode: mode_t,
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.stat.st_size as u64
    }
    pub fn perm(&self) -> FilePermissions {
        FilePermissions { mode: (self.stat.st_mode as mode_t) }
    }

    pub fn file_type(&self) -> FileType {
        FileType { mode: self.stat.st_mode as mode_t }
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.dir.root.join(self.file_name_os_str())
    }

    pub fn file_name(&self) -> OsString {
        self.file_name_os_str().to_os_string()
    }

    #[cfg(all(
        any(
            all(target_os = "linux", not(target_env = "musl")),
            target_os = "android",
            target_os = "fuchsia",
            target_os = "hurd",
            target_os = "illumos",
            target_vendor = "apple",
        ),
        not(miri) // no dirfd on Miri
    ))]
    pub fn metadata(&self) -> io::Result<FileAttr> {
        let fd = cvt(unsafe {
            use libc::dirfd;
 dirfd(self.dir.dirp.0) })?;
        let name = self.name_cstr().as_ptr();

        cfg_has_statx! {
            if let Some(ret) = unsafe { try_statx(
                fd,
                name,
                libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_SYNC_AS_STAT,
                libc::STATX_BASIC_STATS | libc::STATX_BTIME,
            ) } {
                return ret;
            }
        }

        let mut stat: stat64 = unsafe { mem::zeroed() };
        cvt(unsafe {
            use libc::fstatat64;
 fstatat64(fd, name, &mut stat, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(FileAttr::from_stat64(stat))
    }


    #[cfg(any(
        not(any(
            all(target_os = "linux", not(target_env = "musl")),
            target_os = "android",
            target_os = "fuchsia",
            target_os = "hurd",
            target_os = "illumos",
            target_vendor = "apple",
        )),
        miri
    ))]
    pub fn metadata(&self) -> io::Result<FileAttr> {
        run_path_with_cstr(&self.path(), &lstat)
    }

    #[cfg(not(any(
        target_os = "solaris",
        target_os = "illumos",
        target_os = "haiku",
        target_os = "vxworks",
        target_os = "aix",
        target_os = "nto",
        target_os = "vita",
    )))]
    pub fn file_type(&self) -> io::Result<FileType> {
        match self.entry.d_type {
            libc::DT_CHR => Ok(FileType { mode: libc::S_IFCHR }),
            libc::DT_FIFO => Ok(FileType { mode: libc::S_IFIFO }),
            libc::DT_LNK => Ok(FileType { mode: libc::S_IFLNK }),
            libc::DT_REG => Ok(FileType { mode: libc::S_IFREG }),
            libc::DT_SOCK => Ok(FileType { mode: libc::S_IFSOCK }),
            libc::DT_DIR => Ok(FileType { mode: libc::S_IFDIR }),
            libc::DT_BLK => Ok(FileType { mode: libc::S_IFBLK }),
            _ => self.metadata().map(|m| m.file_type()),
        }
    }

    #[cfg(any(
        target_os = "aix",
        target_os = "android",
        target_os = "cygwin",
        target_os = "emscripten",
        target_os = "espidf",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "haiku",
        target_os = "horizon",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "l4re",
        target_os = "linux",
        target_os = "nto",
        target_os = "redox",
        target_os = "rtems",
        target_os = "solaris",
        target_os = "vita",
        target_os = "vxworks",
        target_os = "wasi",
        target_vendor = "apple",
    ))]
    pub fn ino(&self) -> u64 {
        self.entry.d_ino as u64
    }

    #[cfg(not(any(
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_vendor = "apple",
    )))]
    fn name_bytes(&self) -> &[u8] {
        self.name_cstr().to_bytes()
    }

    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "fuchsia",
        target_os = "redox",
        target_os = "aix",
        target_os = "nto",
        target_os = "vita",
        target_os = "hurd",
        target_os = "wasi",
    ))]
    fn name_cstr(&self) -> &CStr {
        &self.name
    }

    pub fn file_name_os_str(&self) -> &OsStr {
        OsStr::from_bytes(self.name_bytes())
    }
}

pub struct FileType {
    mode: mode_t,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is(libc::S_IFDIR)
    }
    pub fn is_file(&self) -> bool {
        self.is(libc::S_IFREG)
    }
    pub fn is_symlink(&self) -> bool {
        self.is(libc::S_IFLNK)
    }

    pub fn is(&self, mode: mode_t) -> bool {
        self.masked() == mode
    }

    fn masked(&self) -> mode_t {
        self.mode & libc::S_IFMT
    }
}



// #[doc(hidden)]
pub trait IsMinusOne {
    fn is_minus_one(&self) -> bool;
}

macro_rules! impl_is_minus_one {
    ($($t:ident)*) => ($(impl IsMinusOne for $t {
        fn is_minus_one(&self) -> bool {
            *self == -1
        }
    })*)
}

impl_is_minus_one! { i8 i16 i32 i64 isize }

/// Converts native return values to Result using the *-1 means error is in `errno`*  convention.
/// Non-error values are `Ok`-wrapped.
fn cvt<T: IsMinusOne>(t: T) -> io::Result<T> {
    if t.is_minus_one() { Err(io::Error::last_os_error()) } else { Ok(t) }
}

pub fn lstat(p: &CStr) -> io::Result<FileAttr> {
    cfg_has_statx! {
        if let Some(ret) = unsafe { try_statx(
            libc::AT_FDCWD,
            p.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_BASIC_STATS | libc::STATX_BTIME,
        ) } {
            return ret;
        }
    }

    let mut stat: stat64 = unsafe { mem::zeroed() };
    cvt(unsafe { lstat64(p.as_ptr(), &mut stat) })?;
    Ok(FileAttr::from_stat64(stat))
}

macro syscall {
    (
        fn $name:ident($($param:ident : $t:ty),* $(,)?) -> $ret:ty;
    ) => (
        unsafe fn $name($($param: $t),*) -> $ret {
            weak!(fn $name($($param: $t),*) -> $ret;);

            // Use a weak symbol from libc when possible, allowing `LD_PRELOAD`
            // interposition, but if it's not found just use a raw syscall.
            if let Some(fun) = $name.get() {
                unsafe { fun($($param),*) }
            } else {
                unsafe { libc::syscall(libc::${concat(SYS_, $name)}, $($param),*) as $ret }
            }
        }
    )
}

cfg_has_statx! {{
    #[derive(Clone)]
    pub struct FileAttr {
        stat: stat64,
        statx_extra_fields: Option<StatxExtraFields>,
    }

    #[derive(Clone)]
    struct StatxExtraFields {
        // This is needed to check if btime is supported by the filesystem.
        stx_mask: u32,
        stx_btime: libc::statx_timestamp,
        // With statx, we can overcome 32-bit `time_t` too.
        #[cfg(target_pointer_width = "32")]
        stx_atime: libc::statx_timestamp,
        #[cfg(target_pointer_width = "32")]
        stx_ctime: libc::statx_timestamp,
        #[cfg(target_pointer_width = "32")]
        stx_mtime: libc::statx_timestamp,

    }

    // We prefer `statx` on Linux if available, which contains file creation time,
    // as well as 64-bit timestamps of all kinds.
    // Default `stat64` contains no creation time and may have 32-bit `time_t`.
    unsafe fn try_statx(
        fd: c_int,
        path: *const c_char,
        flags: i32,
        mask: u32,
    ) -> Option<io::Result<FileAttr>> {
        use core::sync::atomic::{Atomic, AtomicU8, Ordering};

        // Linux kernel prior to 4.11 or glibc prior to glibc 2.28 don't support `statx`.
        // We check for it on first failure and remember availability to avoid having to
        // do it again.
        #[repr(u8)]
        enum STATX_STATE{ Unknown = 0, Present, Unavailable }
        static STATX_SAVED_STATE: Atomic<u8> = AtomicU8::new(STATX_STATE::Unknown as u8);

        syscall!(
            fn statx(
                fd: c_int,
                pathname: *const c_char,
                flags: c_int,
                mask: libc::c_uint,
                statxbuf: *mut libc::statx,
            ) -> c_int;
        );

        let statx_availability = STATX_SAVED_STATE.load(Ordering::Relaxed);
        if statx_availability == STATX_STATE::Unavailable as u8 {
            return None;
        }

        let mut buf: libc::statx = unsafe { mem::zeroed() } ;
        if let Err(err) = cvt(unsafe { statx(fd, path, flags, mask, &mut buf) }) {
            if STATX_SAVED_STATE.load(Ordering::Relaxed) == STATX_STATE::Present as u8 {
                return Some(Err(err));
            }

            // We're not yet entirely sure whether `statx` is usable on this kernel
            // or not. Syscalls can return errors from things other than the kernel
            // per se, e.g. `EPERM` can be returned if seccomp is used to block the
            // syscall, or `ENOSYS` might be returned from a faulty FUSE driver.
            //
            // Availability is checked by performing a call which expects `EFAULT`
            // if the syscall is usable.
            //
            // See: https://github.com/rust-lang/rust/issues/65662
            //
            // FIXME what about transient conditions like `ENOMEM`?
            let err2 = cvt(unsafe { statx(0, ptr::null(), 0, libc::STATX_BASIC_STATS | libc::STATX_BTIME, ptr::null_mut()) })
                .err()
                .and_then(|e| e.raw_os_error());
            if err2 == Some(libc::EFAULT) {
                STATX_SAVED_STATE.store(STATX_STATE::Present as u8, Ordering::Relaxed);
                return Some(Err(err));
            } else {
                STATX_SAVED_STATE.store(STATX_STATE::Unavailable as u8, Ordering::Relaxed);
                return None;
            }
        }
        if statx_availability == STATX_STATE::Unknown as u8 {
            STATX_SAVED_STATE.store(STATX_STATE::Present as u8, Ordering::Relaxed);
        }

        // We cannot fill `stat64` exhaustively because of private padding fields.
        let mut stat: stat64 = unsafe { mem::zeroed() } ;
        // `c_ulong` on gnu-mips, `dev_t` otherwise
        stat.st_dev = libc::makedev(buf.stx_dev_major, buf.stx_dev_minor) as _;
        stat.st_ino = buf.stx_ino as libc::ino64_t;
        stat.st_nlink = buf.stx_nlink as libc::nlink_t;
        stat.st_mode = buf.stx_mode as libc::mode_t;
        stat.st_uid = buf.stx_uid as libc::uid_t;
        stat.st_gid = buf.stx_gid as libc::gid_t;
        stat.st_rdev = libc::makedev(buf.stx_rdev_major, buf.stx_rdev_minor) as _;
        stat.st_size = buf.stx_size as off64_t;
        stat.st_blksize = buf.stx_blksize as libc::blksize_t;
        stat.st_blocks = buf.stx_blocks as libc::blkcnt64_t;
        stat.st_atime = buf.stx_atime.tv_sec as libc::time_t;
        // `i64` on gnu-x86_64-x32, `c_ulong` otherwise.
        stat.st_atime_nsec = buf.stx_atime.tv_nsec as _;
        stat.st_mtime = buf.stx_mtime.tv_sec as libc::time_t;
        stat.st_mtime_nsec = buf.stx_mtime.tv_nsec as _;
        stat.st_ctime = buf.stx_ctime.tv_sec as libc::time_t;
        stat.st_ctime_nsec = buf.stx_ctime.tv_nsec as _;

        let extra = StatxExtraFields {
            stx_mask: buf.stx_mask,
            stx_btime: buf.stx_btime,
            // Store full times to avoid 32-bit `time_t` truncation.
            #[cfg(target_pointer_width = "32")]
            stx_atime: buf.stx_atime,
            #[cfg(target_pointer_width = "32")]
            stx_ctime: buf.stx_ctime,
            #[cfg(target_pointer_width = "32")]
            stx_mtime: buf.stx_mtime,
        };

        Some(Ok(FileAttr { stat, statx_extra_fields: Some(extra) }))
    }

} else {
    #[derive(Clone)]
    pub struct FileAttr {
        stat: stat64,
    }
}}

cfg_has_statx! {{
    impl FileAttr {
        fn from_stat64(stat: stat64) -> Self {
            Self { stat, statx_extra_fields: None }
        }

        #[cfg(target_pointer_width = "32")]
        pub fn stx_mtime(&self) -> Option<&libc::statx_timestamp> {
            if let Some(ext) = &self.statx_extra_fields {
                if (ext.stx_mask & libc::STATX_MTIME) != 0 {
                    return Some(&ext.stx_mtime);
                }
            }
            None
        }

        #[cfg(target_pointer_width = "32")]
        pub fn stx_atime(&self) -> Option<&libc::statx_timestamp> {
            if let Some(ext) = &self.statx_extra_fields {
                if (ext.stx_mask & libc::STATX_ATIME) != 0 {
                    return Some(&ext.stx_atime);
                }
            }
            None
        }

        #[cfg(target_pointer_width = "32")]
        pub fn stx_ctime(&self) -> Option<&libc::statx_timestamp> {
            if let Some(ext) = &self.statx_extra_fields {
                if (ext.stx_mask & libc::STATX_CTIME) != 0 {
                    return Some(&ext.stx_ctime);
                }
            }
            None
        }
    }
} else {
    impl FileAttr {
        fn from_stat64(stat: stat64) -> Self {
            Self { stat }
        }
    }
}}

impl Drop for DirStream {
    fn drop(&mut self) {
        // dirfd isn't supported everywhere
        #[cfg(not(any(
            miri,
            target_os = "redox",
            target_os = "nto",
            target_os = "vita",
            target_os = "hurd",
            target_os = "espidf",
            target_os = "horizon",
            target_os = "vxworks",
            target_os = "rtems",
            target_os = "nuttx",
        )))]
        {
            let fd = unsafe { libc::dirfd(self.0) };
            // debug_assert_fd_is_open(fd);
        }
        let r = unsafe { libc::closedir(self.0) };
        assert!(
            r == 0 || errno_is_interrupted(&Error::last_os_error()),
            "unexpected error during closedir: {:?}",
            crate::io::Error::last_os_error()
        );
    }
}


fn recursive_read_dir(read_dir: ReadDir) {
    for child in read_dir {
        let _ = try {
            let child = child?;
            if child.file_type()?.is_dir() {
                recursive_read_dir(readdir(child.path())?);
            }
        };
    }
}


fn bench_read_dir(c: &mut Criterion) {
    let home_path = "put_dir_path_here";

    c.bench_function("Spec Read Dir", |b| {
        b.iter(|| {
            recursive_read_dir(readdir(home_path).unwrap())
        })
    });
}

criterion_group!(benches, bench_read_dir);
criterion_main!(benches);