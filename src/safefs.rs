//! Symlink-refusing file operations. Every hop below a root is `openat` with
//! `O_NOFOLLOW` relative to the previous dir's fd, and files are created with
//! `O_CREAT|O_EXCL|O_NOFOLLOW`: a symlink anywhere on a walk fails (ELOOP or
//! ENOTDIR) instead of being followed. Whatever a job (or a box) made is only
//! ever touched through this module (learnings.md: never follow links a job made).

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// What an entry is, seen without following it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File {
        size: u64,
        exec: bool,
    },
    Link {
        target: OsString,
    },
    /// fifo, socket, device: never opened
    Other,
}

/// An open directory; everything below it is reached relative to its fd.
pub struct Dir(OwnedFd);

fn cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name contains a NUL byte"))
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

impl Dir {
    /// Open a root the caller trusts (the daemon's job dir, the client's
    /// chosen dir). Only its last component must not be a symlink.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let c = cstr(path.as_os_str())?;
        let fd = check(unsafe { libc::open(c.as_ptr(), DIR_FLAGS) })?;
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn open_dir(&self, name: &OsStr) -> io::Result<Dir> {
        let c = cstr(name)?;
        let fd = check(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), DIR_FLAGS) })?;
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// mkdirat never follows its last component; an existing real dir is reused.
    pub fn open_or_mkdir(&self, name: &OsStr) -> io::Result<Dir> {
        let c = cstr(name)?;
        let rc = unsafe { libc::mkdirat(self.0.as_raw_fd(), c.as_ptr(), 0o755) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
        self.open_dir(name)
    }

    /// A new file; fails if anything (a file, a link) already has the name.
    pub fn create_file(&self, name: &OsStr, mode: u32) -> io::Result<File> {
        let c = cstr(name)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = check(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags, mode as libc::c_uint) })?;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// A regular file for reading. O_NONBLOCK so a fifo planted under the
    /// name can't block the open; anything not regular is refused.
    pub fn open_file(&self, name: &OsStr) -> io::Result<File> {
        let c = cstr(name)?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let fd = check(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags) })?;
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        Ok(file)
    }

    pub fn symlink(&self, target: &OsStr, name: &OsStr) -> io::Result<()> {
        let t = cstr(target)?;
        let c = cstr(name)?;
        check(unsafe { libc::symlinkat(t.as_ptr(), self.0.as_raw_fd(), c.as_ptr()) })?;
        Ok(())
    }

    pub fn kind(&self, name: &OsStr) -> io::Result<Kind> {
        let c = cstr(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(self.0.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFREG => Kind::File {
                size: st.st_size as u64,
                exec: st.st_mode & 0o100 != 0,
            },
            libc::S_IFLNK => {
                let mut buf = vec![0u8; libc::PATH_MAX as usize];
                let n = check(unsafe {
                    libc::readlinkat(self.0.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
                        as libc::c_int
                })?;
                buf.truncate(n as usize);
                Kind::Link {
                    target: OsString::from_vec(buf),
                }
            }
            _ => Kind::Other,
        })
    }

    /// The names in this dir, sorted, without following anything.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        // /proc/self/fd/N resolves to this very dir, whatever its path is now
        let mut names = std::fs::read_dir(format!("/proc/self/fd/{}", self.0.as_raw_fd()))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }
}

/// Check a relative path from the wire and split it into components: no
/// absolute paths, no `.`/`..`, no empty components, bounded length and depth.
pub fn split_rel(path: &str) -> io::Result<Vec<&OsStr>> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("bad path {path:?}: {why}"));
    if path.is_empty() || path.len() > 4096 {
        return Err(bad("empty or too long"));
    }
    if path.starts_with('/') {
        return Err(bad("absolute"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 64 {
        return Err(bad("too deep"));
    }
    for p in &parts {
        if p.is_empty() || *p == "." || *p == ".." || p.contains('\0') || p.len() > 255 {
            return Err(bad("empty, '.', '..' or invalid component"));
        }
    }
    Ok(parts.into_iter().map(OsStr::new).collect())
}

/// Walk to the parent of `parts`' last component, creating missing dirs,
/// never following a link. Returns the parent and the last name.
pub fn walk_parent<'a>(root: &Dir, parts: &[&'a OsStr]) -> io::Result<(Dir, &'a OsStr)> {
    let (last, dirs) = parts.split_last().expect("split_rel never returns an empty path");
    let mut dir = root.open_dir(OsStr::new("."))?;
    for d in dirs {
        dir = dir.open_or_mkdir(d)?;
    }
    Ok((dir, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_rel_refuses_escapes() {
        for p in ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a/.", "a/"] {
            assert!(split_rel(p).is_err(), "{p:?} must be refused");
        }
        assert_eq!(split_rel("a/b.c/d").unwrap().len(), 3);
        assert!(split_rel(&"a/".repeat(65)[..129]).is_err());
    }

    #[test]
    fn walks_never_follow_links() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = Dir::open(t.path()).unwrap();
        root.symlink(outside.path().as_os_str(), OsStr::new("l")).unwrap();
        // a dir hop through the link fails
        assert!(walk_parent(&root, &split_rel("l/x").unwrap()).is_err());
        // creating a file over the link fails
        assert!(root.create_file(OsStr::new("l"), 0o644).is_err());
        // reading through it fails
        assert!(root.open_file(OsStr::new("l")).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
        assert!(matches!(root.kind(OsStr::new("l")).unwrap(), Kind::Link { .. }));
    }

    #[test]
    fn a_fifo_is_neither_opened_nor_blocking() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("f");
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let root = Dir::open(t.path()).unwrap();
        assert_eq!(root.kind(OsStr::new("f")).unwrap(), Kind::Other);
        assert!(root.open_file(OsStr::new("f")).is_err());
    }
}
