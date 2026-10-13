//! Folders an automation client is allowed to read and write.
//!
//! `deckcraft-cli mcp` can set these for the whole process. With no roots, nothing is checked, so
//! the desktop app and the rest of the CLI keep their current access. A root grants only its own
//! kind of access. The check follows links and compares whole path components, so a sibling such
//! as `/work2` is not inside `/work`. The check and the later open are not one atomic step.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use std::cell::RefCell;

thread_local! {
    static SCOPE: RefCell<Vec<Option<Arc<AutomationRoots>>>> = const { RefCell::new(Vec::new()) };
}

static PROCESS: OnceLock<Arc<AutomationRoots>> = OnceLock::new();

/// Which side of the filesystem an agent path is about to touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// Canonical directories. Either side may be absent; that absence is not a grant of the other.
#[derive(Debug)]
pub struct AutomationRoots {
    read: Option<PathBuf>,
    write: Option<PathBuf>,
}

impl AutomationRoots {
    /// `None` for both means no confinement. A missing path or a path that is not a folder is an error.
    pub fn new(read: Option<&Path>, write: Option<&Path>) -> Result<Option<Arc<Self>>, String> {
        if read.is_none() && write.is_none() {
            return Ok(None);
        }
        let read = read.map(|p| open_root(p, "read")).transpose()?;
        let write = write.map(|p| open_root(p, "write")).transpose()?;
        Ok(Some(Arc::new(Self { read, write })))
    }
}

fn open_root(path: &Path, kind: &str) -> Result<PathBuf, String> {
    let canon = std::fs::canonicalize(path).map_err(|_| format!("cannot open the automation {kind} root"))?;
    if !canon.is_dir() {
        return Err(format!("cannot open the automation {kind} root"));
    }
    Ok(plain(&canon))
}

/// Roots for this thread, if a scope or the process has set them. A scope of `None` is unconfined.
pub fn current() -> Option<Arc<AutomationRoots>> {
    SCOPE.with(|stack| match stack.borrow().last() {
        Some(top) => top.clone(),
        None => PROCESS.get().cloned(),
    })
}

/// Run `f` with these roots. The previous scope is restored afterwards, including after a panic.
pub fn confine<T>(roots: Option<&Arc<AutomationRoots>>, f: impl FnOnce() -> T) -> T {
    SCOPE.with(|stack| stack.borrow_mut().push(roots.cloned()));
    let _guard = ScopeGuard;
    f()
}

/// Run `f` with no roots, even when the process has them. For the app's own files.
pub fn unconfined<T>(f: impl FnOnce() -> T) -> T {
    confine(None, f)
}

/// Set roots for every thread that has not pushed its own scope. Once only; a second call is false.
pub fn confine_process(roots: &Arc<AutomationRoots>) -> bool {
    PROCESS.set(Arc::clone(roots)).is_ok()
}

struct ScopeGuard;

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

pub fn check_read(path: impl AsRef<Path>) -> Result<(), String> {
    check(Access::Read, path.as_ref())
}

pub fn check_write(path: impl AsRef<Path>) -> Result<(), String> {
    check(Access::Write, path.as_ref())
}

pub fn check(access: Access, path: &Path) -> Result<(), String> {
    let Some(roots) = current() else { return Ok(()) };
    let root = match access {
        Access::Read => roots.read.as_deref(),
        Access::Write => roots.write.as_deref(),
    };
    let Some(root) = root else {
        return Err(format!("automation filesystem access is not granted: {} authority is absent", access_name(access)));
    };
    let resolved = resolve(path)?;
    if !inside(&resolved, root) {
        return Err(format!("automation path rejected: outside the {} root", access_name(access)));
    }
    Ok(())
}

fn access_name(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
    }
}

fn resolve(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("path is empty".into());
    }
    #[cfg(windows)]
    reject_device(path)?;
    let absolute = std::path::absolute(path).map_err(|e| format!("cannot resolve: {e}"))?;
    let (resolved, _) = resolve_absolute(&absolute, 0)?;
    Ok(plain(&resolved))
}

/// Walk an absolute path. The bool is whether the final path exists.
fn resolve_absolute(path: &Path, depth: usize) -> Result<(PathBuf, bool), String> {
    if depth > 40 {
        return Err("cannot resolve".into());
    }
    let mut resolved = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if missing || !resolved.pop() {
                    return Err("parent traversal below a folder that doesn't exist".into());
                }
            }
            Component::Normal(name) => {
                if missing {
                    resolved.push(name);
                    continue;
                }
                let next = resolved.join(name);
                match std::fs::symlink_metadata(&next) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        let target = std::fs::read_link(&next).map_err(|_| String::from("a link that leads nowhere"))?;
                        let joined = if target.is_absolute() { target } else { resolved.join(target) };
                        let (followed, exists) = resolve_absolute(&joined, depth + 1)?;
                        if !exists {
                            return Err("a link that leads nowhere".into());
                        }
                        resolved = followed;
                    }
                    Ok(_) => resolved = next,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        missing = true;
                        resolved = next;
                    }
                    Err(e) => return Err(format!("cannot resolve: {e}")),
                }
            }
        }
    }
    Ok((resolved, !missing))
}

fn inside(path: &Path, root: &Path) -> bool {
    let path = plain(path);
    let root = plain(root);
    let mut parts = path.components();
    for root_part in root.components() {
        match parts.next() {
            Some(part) if same_component(part, root_part) => {}
            _ => return false,
        }
    }
    true
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    if cfg!(windows) {
        match (a, b) {
            (Component::Normal(x), Component::Normal(y)) => x.eq_ignore_ascii_case(y),
            _ => a == b,
        }
    } else {
        a == b
    }
}

/// `\\?\` from canonicalize is not a different folder.
fn plain(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.as_ref());
        }
    }
    path.to_path_buf()
}

#[cfg(windows)]
fn reject_device(path: &Path) -> Result<(), String> {
    for component in path.components() {
        if let Component::Normal(name) = component {
            if name.to_str().is_some_and(is_windows_device) {
                return Err("automation path rejected: a device name".into());
            }
        }
    }
    Ok(())
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, with an optional extension.
#[cfg(any(windows, test))]
fn is_windows_device(name: &str) -> bool {
    let trimmed = name.trim_end_matches([' ', '.']);
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered(&stem, "COM") || numbered(&stem, "LPT")
}

#[cfg(any(windows, test))]
fn numbered(stem: &str, prefix: &str) -> bool {
    let Some(rest) = stem.strip_prefix(prefix) else { return false };
    rest.len() == 1 && rest.as_bytes().first().is_some_and(|c| (b'1'..=b'9').contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> (Tmp, PathBuf) {
        let dir = std::env::temp_dir().join(format!("deckcraft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Tmp(dir.clone()), dir)
    }

    #[test]
    fn no_roots_checks_nothing() {
        assert!(AutomationRoots::new(None, None).unwrap().is_none());
        assert!(check_read("/etc/passwd").is_ok());
        assert!(check_write("/etc/passwd").is_ok());
        assert!(check_read("").is_ok());
    }

    #[test]
    fn a_missing_root_is_refused() {
        let err = AutomationRoots::new(Some(Path::new("/no/such/deckcraft-automation-root")), None).unwrap_err();
        assert!(err.contains("cannot open the automation read root"), "{err}");
        let (_tmp, dir) = scratch("root-file");
        let file = dir.join("not-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let err = AutomationRoots::new(None, Some(&file)).unwrap_err();
        assert!(err.contains("cannot open the automation write root"), "{err}");
    }

    #[test]
    fn one_root_grants_only_its_kind() {
        let (_tmp, dir) = scratch("one-kind");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"a").unwrap();
        let read_only = AutomationRoots::new(Some(&dir), None).unwrap().unwrap();
        confine(Some(&read_only), || {
            assert!(check_read(&file).is_ok());
            assert!(check_read(&dir).is_ok());
            let err = check_write(&file).unwrap_err();
            assert!(err.contains("write authority is absent"), "{err}");
        });
        let write_only = AutomationRoots::new(None, Some(&dir)).unwrap().unwrap();
        confine(Some(&write_only), || {
            assert!(check_write(&file).is_ok());
            let err = check_read(&file).unwrap_err();
            assert!(err.contains("read authority is absent"), "{err}");
        });
        assert!(current().is_none());
    }

    #[test]
    fn a_sibling_prefix_is_outside_and_dotdot_is_resolved() {
        let (_tmp, base) = scratch("sibling");
        let work = base.join("work");
        let work2 = base.join("work2");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&work2).unwrap();
        std::fs::create_dir(work.join("sub")).unwrap();
        std::fs::write(work.join("inside.txt"), b"in").unwrap();
        std::fs::write(work2.join("outside.txt"), b"out").unwrap();
        let roots = AutomationRoots::new(Some(&work), Some(&work)).unwrap().unwrap();
        confine(Some(&roots), || {
            assert!(check_read(work.join("inside.txt")).is_ok());
            assert!(check_read(work.join("sub").join("..").join("inside.txt")).is_ok());
            let err = check_read(work2.join("outside.txt")).unwrap_err();
            assert!(err.contains("outside the read root"), "{err}");
            let escaped = work.join("..").join("work2").join("outside.txt");
            let err = check_write(&escaped).unwrap_err();
            assert!(err.contains("outside the write root"), "{err}");
            let below = work.join("missing").join("..").join("inside.txt");
            let err = check_read(&below).unwrap_err();
            assert!(err.contains("doesn't exist"), "{err}");
            assert!(check_read("").unwrap_err().contains("path is empty"));
        });
    }

    #[test]
    fn a_relative_path_is_against_the_working_directory() {
        let (_tmp, dir) = scratch("relative");
        let roots = AutomationRoots::new(Some(&dir), None).unwrap().unwrap();
        confine(Some(&roots), || {
            let err = check_read("deckcraft-not-in-the-root").unwrap_err();
            assert!(err.contains("outside the read root"), "{err}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed() {
        let (_tmp, base) = scratch("links");
        let read = base.join("read");
        let outside = base.join("outside");
        std::fs::create_dir(&read).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let inside = read.join("inside.txt");
        let secret = outside.join("secret.txt");
        std::fs::write(&inside, b"in").unwrap();
        std::fs::write(&secret, b"no").unwrap();
        std::os::unix::fs::symlink(&secret, read.join("leak")).unwrap();
        std::os::unix::fs::symlink(&inside, read.join("ok")).unwrap();
        std::os::unix::fs::symlink(read.join("missing-target"), read.join("dangling")).unwrap();
        let roots = AutomationRoots::new(Some(&read), Some(&read)).unwrap().unwrap();
        confine(Some(&roots), || {
            assert!(check_read(read.join("ok")).is_ok());
            let err = check_read(read.join("leak")).unwrap_err();
            assert!(err.contains("outside the read root"), "{err}");
            let err = check_write(read.join("dangling")).unwrap_err();
            assert!(err.contains("leads nowhere"), "{err}");
        });
    }

    #[test]
    fn scopes_nest_and_a_panic_restores_the_outer_one() {
        let (_tmp, dir) = scratch("scope");
        let roots = AutomationRoots::new(Some(&dir), None).unwrap().unwrap();
        confine(Some(&roots), || {
            assert!(check_write(dir.join("a")).unwrap_err().contains("write authority is absent"));
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                unconfined(|| panic!("restore me"));
            }));
            assert!(panicked.is_err());
            assert!(check_write(dir.join("a")).unwrap_err().contains("write authority is absent"));
        });
        assert!(check_write(dir.join("a")).is_ok());
    }

    #[test]
    fn another_thread_is_not_confined() {
        let (_tmp, dir) = scratch("thread");
        let roots = AutomationRoots::new(Some(&dir), None).unwrap().unwrap();
        confine(Some(&roots), || {
            let open = std::thread::spawn(|| check_read(Path::new("/etc/passwd")).is_ok()).join().unwrap();
            assert!(open);
            assert!(check_read("/etc/passwd").is_err());
        });
    }

    #[test]
    fn windows_device_names() {
        assert!(is_windows_device("CON"));
        assert!(is_windows_device("con.txt"));
        assert!(is_windows_device("AUX."));
        assert!(is_windows_device("COM1"));
        assert!(is_windows_device("LPT9"));
        assert!(!is_windows_device("COM10"));
        assert!(!is_windows_device("COM0"));
        assert!(!is_windows_device("console"));
        assert!(!is_windows_device("file.txt"));
    }
}
