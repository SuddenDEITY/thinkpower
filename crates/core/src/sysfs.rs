//! Minimal filesystem abstraction over sysfs so hardware logic is testable.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub trait Sysfs: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, value: &str) -> io::Result<()>;
    /// Names of the direct children of `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    fn exists(&self, path: &Path) -> bool;
    /// Whether the file mode allows the owner (root) to write it.
    fn owner_writable(&self, path: &Path) -> bool;

    fn read_trimmed(&self, path: &Path) -> io::Result<String> {
        Ok(self.read(path)?.trim().to_string())
    }

    fn read_u64(&self, path: &Path) -> io::Result<u64> {
        let s = self.read_trimmed(path)?;
        s.parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{}: not a number: {s:?}", path.display())))
    }
}

/// The real kernel sysfs.
pub struct RealSysfs;

impl Sysfs for RealSysfs {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, value: &str) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.write_all(value.as_bytes())
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir)? {
            out.push(e?.file_name().to_string_lossy().into_owned());
        }
        Ok(out)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn owner_writable(&self, path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o200 != 0)
            .unwrap_or(false)
    }
}

type WriteHook = Box<dyn Fn(&str) -> io::Result<String> + Send + Sync>;

/// In-memory sysfs for tests. Write hooks let a test simulate firmware that
/// rounds or rejects values.
#[derive(Default)]
pub struct MockSysfs {
    files: Mutex<BTreeMap<PathBuf, String>>,
    read_only: Mutex<BTreeSet<PathBuf>>,
    hooks: Mutex<HashMap<PathBuf, WriteHook>>,
}

impl MockSysfs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, path: impl Into<PathBuf>, value: impl Into<String>) -> &Self {
        self.files.lock().unwrap().insert(path.into(), value.into());
        self
    }

    pub fn set_read_only(&self, path: impl Into<PathBuf>) -> &Self {
        self.read_only.lock().unwrap().insert(path.into());
        self
    }

    pub fn on_write(&self, path: impl Into<PathBuf>, hook: impl Fn(&str) -> io::Result<String> + Send + Sync + 'static) -> &Self {
        self.hooks.lock().unwrap().insert(path.into(), Box::new(hook));
        self
    }

    pub fn get(&self, path: impl AsRef<Path>) -> Option<String> {
        self.files.lock().unwrap().get(path.as_ref()).cloned()
    }
}

impl Sysfs for MockSysfs {
    fn read(&self, path: &Path) -> io::Result<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn write(&self, path: &Path, value: &str) -> io::Result<()> {
        if !self.exists(path) {
            return Err(io::Error::new(io::ErrorKind::NotFound, path.display().to_string()));
        }
        if self.read_only.lock().unwrap().contains(path) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, path.display().to_string()));
        }
        let stored = match self.hooks.lock().unwrap().get(path) {
            Some(hook) => hook(value)?,
            None => value.to_string(),
        };
        self.files.lock().unwrap().insert(path.to_path_buf(), stored);
        Ok(())
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let files = self.files.lock().unwrap();
        let mut names = BTreeSet::new();
        for p in files.keys() {
            if let Ok(rest) = p.strip_prefix(dir) {
                if let Some(first) = rest.components().next() {
                    names.insert(first.as_os_str().to_string_lossy().into_owned());
                }
            }
        }
        if names.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, dir.display().to_string()));
        }
        Ok(names.into_iter().collect())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    fn owner_writable(&self, path: &Path) -> bool {
        self.exists(path) && !self.read_only.lock().unwrap().contains(path)
    }
}
