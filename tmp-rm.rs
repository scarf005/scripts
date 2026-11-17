---
[package]
edition = "2024"
[dependencies]
rustix = { version = "=1.1.6", features = ["fs"] }
---
//! Removes files and directories under /tmp, and nothing else.
//!
//! Usage: tmp-rm [--dry-run] [--] /tmp/PATH...
//!
//! Why nothing outside /tmp can go:
//! - Each path must be spelled /tmp/a/b, without `.`, `..` or an empty component.
//! - /tmp must be the root of a tmpfs that this process's mount table shows nowhere outside /tmp, so
//!   nothing it can reach outside /tmp is that filesystem and no rename can carry a directory out of it
//!   during a removal. Bind mounts of it under /tmp, as a sandbox makes them, are allowed.
//! - Everything is resolved from a descriptor of /tmp. The kernel refuses to follow a symlink or leave
//!   the start directory (openat2 RESOLVE_NO_SYMLINKS, RESOLVE_BENEATH). The components of a path may
//!   cross into those bind mounts only; a removal crosses no mount (RESOLVE_NO_XDEV).
//! - A symlink is unlinked, never followed.
//! - The installed program is a static binary: no cargo, PATH, config or loader takes part in a run.
//!
//! Outside this: a process that can already change the mounts, in this or another namespace, or write
//! the binary can delete anything without this program. /tmp moved elsewhere during a run keeps losing what was under it at the start.
//!
//! Build, with the musl target of the nightly toolchain:
//!   env -u RUSTC_WRAPPER cargo +nightly -Zscript build --release \
//!     --target x86_64-unknown-linux-musl --target-dir ~/.cache/tmp-rm-build --manifest-path tmp-rm.rs
//!   install -m 755 ~/.cache/tmp-rm-build/x86_64-unknown-linux-musl/release/tmp-rm ~/.local/bin/tmp-rm
//!
//! Exit status: 0 all removed, 1 something was refused or failed, 2 bad usage or an unfit /tmp.

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, ResolveFlags, StatxFlags, fstatfs, openat2, statat,
    unlinkat,
};
use rustix::io::Errno;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::process::ExitCode;

const TMPFS_MAGIC: i64 = 0x0102_1994;
const PROC_MAGIC: i64 = 0x9fa0;
/// Deeper trees are refused, which bounds the stack and the open descriptors.
const MAX_DEPTH: usize = 512;

fn open_dir(parent: impl AsFd, name: &CStr) -> Result<OwnedFd, Errno> {
    openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_XDEV
            | ResolveFlags::NO_MAGICLINKS,
    )
}

/// The mount id of `fd`.
fn mount_id(fd: impl AsFd) -> Result<u64, Errno> {
    Ok(rustix::fs::statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?.stx_mnt_id)
}

/// The descriptor of /tmp, once it is a real directory holding a tmpfs of its own, and the ids of
/// the mounts of that tmpfs, all at or under /tmp.
fn open_tmp() -> Result<(OwnedFd, Vec<u64>), String> {
    let fd = rustix::fs::openat(
        CWD,
        c"/tmp",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("/tmp: {e}"))?;
    let kind = fstatfs(&fd).map_err(|e| format!("/tmp: {e}"))?.f_type as i64;
    if kind != TMPFS_MAGIC {
        return Err("/tmp is not a tmpfs".into());
    }
    let mount = rustix::fs::statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(|e| format!("/tmp: {e}"))?;
    if mount.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err("the kernel names no mount of /tmp".into());
    }
    let table = std::fs::File::open("/proc/self/mountinfo")
        .and_then(|mut file| {
            let kind = fstatfs(&file).map_err(std::io::Error::from)?.f_type as i64;
            if kind != PROC_MAGIC {
                return Err(std::io::Error::other("not in a procfs"));
            }
            let mut bytes = vec![];
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
    let mounts = check_mount(&String::from_utf8_lossy(&table), mount.stx_mnt_id)?;
    Ok((fd, mounts))
}

/// That the mount `id` in the mountinfo `table` is /tmp, the root of a tmpfs mounted nowhere outside
/// /tmp; the ids of all its mounts.
fn check_mount(table: &str, id: u64) -> Result<Vec<u64>, String> {
    let id = id.to_string();
    let lines: Vec<Vec<&str>> = table.lines().map(|l| l.split(' ').collect()).collect();
    let own = lines
        .iter()
        .find(|f| f.first() == Some(&id.as_str()))
        .ok_or("the mount table does not hold /tmp")?;
    let device = own.get(2).ok_or("a short mount table line")?;
    let root = own.get(3).ok_or("a short mount table line")?;
    let point = own.get(4).ok_or("a short mount table line")?;
    let kind = own
        .iter()
        .position(|f| *f == "-")
        .and_then(|at| own.get(at + 1))
        .ok_or("a short mount table line")?;
    if *kind != "tmpfs" {
        return Err(format!("/tmp is a {kind}, not a tmpfs"));
    }
    if *point != "/tmp" {
        return Err(format!("the mount of /tmp names {point}"));
    }
    if *root != "/" {
        return Err(format!("/tmp shows {root} of its tmpfs, not its root"));
    }
    // A mountinfo path escapes a space and a backslash, so a raw `/tmp/` prefix is the real one.
    lines
        .iter()
        .filter(|f| f.get(2) == Some(device))
        .map(|f| match (f[0].parse(), f.get(4)) {
            (Ok(id), Some(&point)) if point == "/tmp" || point.starts_with("/tmp/") => Ok(id),
            (_, point) => Err(format!(
                "the tmpfs of /tmp is mounted at {} too",
                point.unwrap_or(&"?")
            )),
        })
        .collect()
}

/// The components after /tmp of `path`; none of them may be empty, `.` or `..`.
fn components(path: &OsStr) -> Result<Vec<CString>, String> {
    let bytes = path.as_bytes();
    let rest = bytes
        .strip_prefix(b"/tmp/")
        .ok_or_else(|| "not under /tmp/".to_string())?;
    let rest = rest.strip_suffix(b"/").unwrap_or(rest);
    if rest.is_empty() {
        return Err("names /tmp itself".into());
    }
    rest.split(|b| *b == b'/')
        .map(|part| match part {
            b"" | b"." | b".." => Err(format!(
                "has the component {:?}",
                String::from_utf8_lossy(part)
            )),
            _ => CString::new(part).map_err(|_| "holds a NUL".to_string()),
        })
        .collect()
}

fn entries(dir: impl AsFd) -> Result<Vec<CString>, Errno> {
    Dir::read_from(dir)?
        .filter_map(|entry| match entry {
            Ok(entry) => {
                let name = entry.file_name();
                (name != c"." && name != c"..").then(|| Ok(name.to_owned()))
            }
            Err(e) => Some(Err(e)),
        })
        .collect()
}

/// Removes `name` of `parent`, its contents first; counts what went in `removed`.
fn remove(
    parent: &OwnedFd,
    name: &CStr,
    depth: usize,
    dry_run: bool,
    removed: &mut u64,
) -> Result<(), String> {
    let shown = |e: &dyn std::fmt::Display| format!("{}: {e}", name.to_string_lossy());
    if depth > MAX_DEPTH {
        return Err(shown(&"deeper than the limit"));
    }
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| shown(&e))?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        let dir = open_dir(parent, name).map_err(|e| shown(&e))?;
        for child in entries(&dir).map_err(|e| shown(&e))? {
            remove(&dir, &child, depth + 1, dry_run, removed).map_err(|e| shown(&e))?;
        }
        if !dry_run {
            unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| shown(&e))?;
        }
    } else if !dry_run {
        unlinkat(parent, name, AtFlags::empty()).map_err(|e| shown(&e))?;
    }
    *removed += 1;
    Ok(())
}

/// The directory `name` of `parent`, which may be the root of one of `mounts`.
fn open_parent(parent: impl AsFd, name: &CStr, mounts: &[u64]) -> Result<OwnedFd, String> {
    let shown = |e: &dyn std::fmt::Display| format!("{}: {e}", name.to_string_lossy());
    let fd = openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| shown(&e))?;
    if !mounts.contains(&mount_id(&fd).map_err(|e| shown(&e))?) {
        return Err(shown(&"on another filesystem"));
    }
    Ok(fd)
}

fn remove_path(
    tmp: &OwnedFd,
    mounts: &[u64],
    path: &OsStr,
    dry_run: bool,
    removed: &mut u64,
) -> Result<(), String> {
    let parts = components(path)?;
    let (last, parents) = parts.split_last().expect("at least one component");
    let mut parent = rustix::io::fcntl_dupfd_cloexec(tmp, 0).map_err(|e| e.to_string())?;
    for part in parents {
        parent = open_parent(&parent, part, mounts)?;
    }
    remove(&parent, last, 0, dry_run, removed)
}

/// `text` on stderr, its control characters escaped; a stderr that takes nothing changes nothing.
fn say(text: &str) {
    let _ = writeln!(std::io::stderr(), "{}", text.escape_debug());
}

fn main() -> ExitCode {
    let mut dry_run = false;
    let mut paths: Vec<OsString> = vec![];
    let mut options = true;
    for arg in std::env::args_os().skip(1) {
        match arg.as_bytes() {
            b"--" if options => options = false,
            b"--dry-run" if options => dry_run = true,
            bytes if options && bytes.starts_with(b"-") => {
                say(&format!("tmp-rm: unknown option {}", arg.to_string_lossy()));
                return ExitCode::from(2);
            }
            _ => paths.push(OsString::from_vec(arg.into_vec())),
        }
    }
    if paths.is_empty() {
        say("usage: tmp-rm [--dry-run] [--] /tmp/PATH...");
        return ExitCode::from(2);
    }
    let (tmp, mounts) = match open_tmp() {
        Ok(found) => found,
        Err(e) => {
            say(&format!("tmp-rm: refusing: {e}"));
            return ExitCode::from(2);
        }
    };
    let mut removed = 0;
    let mut failed = 0;
    for path in &paths {
        if let Err(e) = remove_path(&tmp, &mounts, path, dry_run, &mut removed) {
            say(&format!("tmp-rm: {}: {e}", path.to_string_lossy()));
            failed += 1;
        }
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    let _ = writeln!(
        std::io::stdout(),
        "{verb} {removed} entries from {} paths, {failed} failed",
        paths.len()
    );
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::check_mount;

    /// /tmp as a Claude Code bubblewrap sandbox shows it: one tmpfs at /tmp twice and under it.
    const SANDBOX: &str = "\
1471 1137 0:51 / /tmp ro,nosuid master:163 - tmpfs tmpfs rw
1572 1471 0:51 /claude-1000 /tmp/claude-1000 rw,nosuid master:163 - tmpfs tmpfs rw
1584 1471 0:51 / /tmp rw,nosuid master:163 - tmpfs tmpfs rw
1710 1584 0:117 / /tmp/cc-daemon-1000 rw,nosuid - tmpfs tmpfs rw
";

    #[test]
    fn accepts_bind_mounts_under_tmp_and_returns_their_ids() {
        assert_eq!(check_mount(SANDBOX, 1584), Ok(vec![1471, 1572, 1584]));
    }

    #[test]
    fn refuses_the_tmpfs_of_tmp_mounted_outside_tmp() {
        let table = format!("{SANDBOX}1800 1 0:51 / /home/x rw - tmpfs tmpfs rw\n");
        assert_eq!(
            check_mount(&table, 1584),
            Err("the tmpfs of /tmp is mounted at /home/x too".into())
        );
    }

    #[test]
    fn refuses_a_sibling_path_that_only_starts_with_tmp() {
        let table = format!("{SANDBOX}1801 1 0:51 / /tmpx rw - tmpfs tmpfs rw\n");
        assert!(check_mount(&table, 1584).is_err());
    }
}
