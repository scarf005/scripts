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
//! the binary can delete anything without this program. A run keeps the /tmp it opened: if /tmp moves
//! during a run, the run continues to remove under the old /tmp.
//!
//! An entry that cannot go is reported, and the run continues with its siblings. `--dry-run` prints
//! each entry that would go, contents before their directory.
//!
//! Build and install the static binary: `just tmp-rm`.
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
                "has the component '{}'",
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

#[derive(Default)]
struct Tally {
    removed: u64,
    failed: u64,
}

/// Reports `e` for `path` and counts it; false.
fn fail(tally: &mut Tally, path: &str, e: impl std::fmt::Display) -> bool {
    say(&format!("tmp-rm: {path}: {e}"));
    tally.failed += 1;
    false
}

/// Removes `name` of `parent`, shown as `path`, its contents first. Reports each entry that stays and
/// continues with the rest; false when something under `name` stayed.
fn remove(
    parent: &OwnedFd,
    name: &CStr,
    path: &str,
    depth: usize,
    dry_run: bool,
    tally: &mut Tally,
) -> bool {
    if depth > MAX_DEPTH {
        return fail(tally, path, "deeper than the limit");
    }
    let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(e) => return fail(tally, path, e),
    };
    let flags = if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        let dir = match open_dir(parent, name) {
            Ok(dir) => dir,
            Err(e) => return fail(tally, path, e),
        };
        let children = match entries(&dir) {
            Ok(children) => children,
            Err(e) => return fail(tally, path, e),
        };
        let mut emptied = true;
        for child in children {
            let child_path = format!("{path}/{}", child.to_string_lossy());
            emptied &= remove(&dir, &child, &child_path, depth + 1, dry_run, tally);
        }
        if !emptied {
            return false;
        }
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    if dry_run {
        let _ = writeln!(std::io::stdout(), "{}", escaped(path));
    } else if let Err(e) = unlinkat(parent, name, flags) {
        return fail(tally, path, e);
    }
    tally.removed += 1;
    true
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

/// Removes `path`; the error when its parents cannot be opened.
fn remove_path(
    tmp: &OwnedFd,
    mounts: &[u64],
    path: &OsStr,
    dry_run: bool,
    tally: &mut Tally,
) -> Result<(), String> {
    let parts = components(path)?;
    let (last, parents) = parts.split_last().expect("at least one component");
    let mut parent = rustix::io::fcntl_dupfd_cloexec(tmp, 0).map_err(|e| e.to_string())?;
    for part in parents {
        parent = open_parent(&parent, part, mounts)?;
    }
    let shown = path.to_string_lossy();
    remove(
        &parent,
        last,
        shown.strip_suffix('/').unwrap_or(&shown),
        0,
        dry_run,
        tally,
    );
    Ok(())
}

/// `text` with its control characters escaped.
fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// `text` on stderr, escaped; a stderr that takes nothing changes nothing.
fn say(text: &str) {
    let _ = writeln!(std::io::stderr(), "{}", escaped(text));
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
    let mut tally = Tally::default();
    for path in &paths {
        if let Err(e) = remove_path(&tmp, &mounts, path, dry_run, &mut tally) {
            fail(&mut tally, &path.to_string_lossy(), e);
        }
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    let _ = writeln!(
        std::io::stdout(),
        "{verb} {} entries from {} paths, {} failed",
        tally.removed,
        paths.len(),
        tally.failed
    );
    if tally.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::{Tally, check_mount, components, remove};
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

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

    /// Refuses /tmp as an ext4, at another mount point, or as a subdirectory of its tmpfs.
    #[test]
    fn refuses_tmp_that_is_not_the_root_of_a_tmpfs_at_tmp() {
        for (line, error) in [
            (
                "9 1 8:1 / /tmp rw - ext4 /dev/sda1 rw",
                "/tmp is a ext4, not a tmpfs",
            ),
            (
                "9 1 0:51 / /var/tmp rw - tmpfs tmpfs rw",
                "the mount of /tmp names /var/tmp",
            ),
            (
                "9 1 0:51 /sub /tmp rw - tmpfs tmpfs rw",
                "/tmp shows /sub of its tmpfs, not its root",
            ),
        ] {
            assert_eq!(check_mount(line, 9), Err(error.into()));
        }
    }

    /// Takes the components after /tmp; one trailing slash is allowed.
    #[test]
    fn splits_a_path_under_tmp() {
        let parts = components(OsStr::new("/tmp/a/b/")).unwrap();
        assert_eq!(parts, [c"a".to_owned(), c"b".to_owned()]);
    }

    /// Refuses /tmp itself, other roots, and empty, `.` or `..` components: `/tmp/a/../b` is refused.
    #[test]
    fn refuses_paths_that_are_not_plain_names_under_tmp() {
        for path in [
            "/tmp",
            "/tmp/",
            "/tmpx/a",
            "tmp/a",
            "/tmp//a",
            "/tmp/a//",
            "/tmp/./a",
            "/tmp/a/../b",
        ] {
            assert!(components(OsStr::new(path)).is_err(), "{path}");
        }
    }

    /// An empty directory under the temp directory, unique to `name`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tmp-rm-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn remove_in(dir: &PathBuf, name: &str, dry_run: bool, tally: &mut Tally) -> bool {
        let parent = rustix::fs::open(
            dir,
            rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        remove(&parent, &name, "t", 0, dry_run, tally)
    }

    /// In t/{locked/f, a, b} with locked unreadable, removes a and b, keeps t and locked, counts 1 failure.
    #[test]
    fn continues_with_siblings_after_a_failing_entry() {
        let dir = scratch("siblings");
        let t = dir.join("t");
        fs::create_dir_all(t.join("locked")).unwrap();
        fs::write(t.join("locked/f"), "").unwrap();
        fs::write(t.join("a"), "").unwrap();
        fs::write(t.join("b"), "").unwrap();
        fs::set_permissions(t.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let mut tally = Tally::default();
        let emptied = remove_in(&dir, "t", false, &mut tally);
        fs::set_permissions(t.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let left: Vec<_> = fs::read_dir(&t)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!emptied);
        assert_eq!(left, ["locked"]);
        assert_eq!((tally.removed, tally.failed), (2, 1));
    }

    /// A dry run over t/{d/f} counts 3 entries and leaves all of them.
    #[test]
    fn dry_run_counts_and_keeps_every_entry() {
        let dir = scratch("dry");
        fs::create_dir_all(dir.join("t/d")).unwrap();
        fs::write(dir.join("t/d/f"), "").unwrap();
        let mut tally = Tally::default();
        let emptied = remove_in(&dir, "t", true, &mut tally);
        let kept = dir.join("t/d/f").exists();
        fs::remove_dir_all(&dir).unwrap();
        assert!(emptied && kept);
        assert_eq!((tally.removed, tally.failed), (3, 0));
    }
}
