//! File and package management.

use std::collections::HashMap;
use std::fmt::{self, Debug, Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::RwLock;

use ecow::{eco_format, EcoString};
use once_cell::sync::Lazy;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::is_ident;

/// The global package-path interner.
static INTERNER: Lazy<RwLock<Interner>> =
    Lazy::new(|| RwLock::new(Interner { to_id: HashMap::new(), from_id: Vec::new() }));

/// A package-path interner.
struct Interner {
    to_id: HashMap<Pair, FileId>,
    from_id: Vec<Pair>,
}

/// An interned pair of a package specification and a path.
type Pair = &'static (Option<PackageSpec>, VirtualPath);

/// Identifies a file in a project or package.
///
/// This type is globally interned and thus cheap to copy, compare, and hash.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FileId(u16);

impl FileId {
    /// Create a new interned file specification.
    ///
    /// The path must start with a `/` or this function will panic.
    /// Note that the path is normalized before interning.
    #[track_caller]
    pub fn new(package: Option<PackageSpec>, path: VirtualPath) -> Self {
        // Try to find an existing entry that we can reuse.
        let pair = (package, path);
        if let Some(&id) = INTERNER.read().unwrap().to_id.get(&pair) {
            return id;
        }

        let mut interner = INTERNER.write().unwrap();
        let num = interner.from_id.len().try_into().expect("out of file ids");

        // Create a new entry forever by leaking the pair. We can't leak more
        // than 2^16 pair (and typically will leak a lot less), so its not a
        // big deal.
        let id = FileId(num);
        let leaked = Box::leak(Box::new(pair));
        interner.to_id.insert(leaked, id);
        interner.from_id.push(leaked);
        id
    }

    /// The package the file resides in, if any.
    pub fn package(&self) -> Option<&'static PackageSpec> {
        self.pair().0.as_ref()
    }

    /// The absolute and normalized path to the file _within_ the project or
    /// package.
    pub fn vpath(&self) -> &'static VirtualPath {
        &self.pair().1
    }

    fn is_remote(&self, path: &str) -> bool {
        if let Ok(url) = Url::parse(path) {
            url.scheme() == "http" || url.scheme() == "https"
        } else {
            false
        }
    }

    /// Resolve a file location relative to this file.
    pub fn join(self, path: &str) -> Self {
        Self::new(self.package().cloned(), self.vpath().join(path))
    }

    /// Construct from a raw number.
    pub(crate) const fn from_raw(v: u16) -> Self {
        Self(v)
    }

    /// Extract the raw underlying number.
    pub(crate) const fn into_raw(self) -> u16 {
        self.0
    }

    /// Get the static pair.
    fn pair(&self) -> Pair {
        INTERNER.read().unwrap().from_id[usize::from(self.0)]
    }
}

impl Debug for FileId {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let vpath = self.vpath();
        match self.package() {
            Some(package) => write!(f, "{package:?}{vpath:?}"),
            None => write!(f, "{vpath:?}"),
        }
    }
}

impl Display for FileId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(package) = self.package() {
            write!(f, "[{}] {}", package, self.vpath())
        } else {
            write!(f, "[] {}", self.vpath())
        }
    }
}

/// An absolute path in the virtual file system of a project or package.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VirtualPath(Url);

impl VirtualPath {
    /// Create a new virtual path.
    ///
    /// Even if it doesn't start with `/` or `\`, it is still interpreted as
    /// starting from the root.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::new_impl(path.as_ref())
    }

    /// Non generic new implementation.
    fn new_impl(path: &Path) -> Self {
        if let Ok(url) = Url::parse(path.to_str().unwrap_or("")) {
            return Self(url);
        }

        let mut out = Path::new(&Component::RootDir).to_path_buf();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(component),
                },
                Component::Normal(_) => out.push(component),
            }
        }
        Self(Url::from_file_path(out).unwrap())
    }

    /// Create a virtual path from a real path and a real root.
    ///
    /// Returns `None` if the file path is not contained in the root (i.e. if
    /// `root` is not a lexical prefix of `path`). No file system operations are
    /// performed.
    pub fn within_root(path: &Path, root: &Path) -> Option<Self> {
        path.strip_prefix(root).ok().map(Self::new)
    }

    /// Get the underlying path with a leading `/` or `\`.
    pub fn as_rooted_path(&self) -> PathBuf {
        if self.is_remote() { // todo: add shitload of tests
            return PathBuf::from(self.0.path());
        }
        self.0.to_file_path().unwrap()
    }

    /// Get the underlying path without a leading `/` or `\`.
    pub fn as_rootless_path(&self) -> PathBuf {
        let rooted_path = self.as_rooted_path();
        rooted_path.strip_prefix(Component::RootDir)
            .map(|x|x.to_path_buf())
            .unwrap_or(rooted_path)
    }

    /// Resolve the virtual path relative to an actual file system root
    /// (where the project or package resides).
    ///
    /// Returns `None` if the path lexically escapes the root. The path might
    /// still escape through symlinks.
    pub fn resolve(&self, root: &Path) -> Option<PathBuf> {
        let root_len = root.as_os_str().len();
        let mut out = root.to_path_buf();
        for component in self.as_rooted_path().components() {
            match component {
                Component::Prefix(_) => {}
                Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                    if out.as_os_str().len() < root_len {
                        return None;
                    }
                }
                Component::Normal(_) => out.push(component),
            }
        }
        Some(out)
    }

    /// Resolve a path relative to this virtual path.
    pub fn join(&self, path: impl AsRef<Path>) -> Self {
        if let Ok(url) = Url::parse(path.as_ref().to_str().unwrap_or("")) {
            return Self(url);
        }

        if self.is_remote() {
            let mut ret = self.0.clone();
            let new_path = Path::new(ret.path()).parent().unwrap_or(Path::new("/")).join(path);
            ret.set_path(new_path.to_str().unwrap_or(""));
            println!("Here! {} ", ret);
            return Self(ret);
        }

        if let Some(parent) = self.as_rooted_path().parent() {
            Self::new(parent.join(path))
        } else {
            Self::new(path)
        }
    }

    pub fn is_remote(&self) -> bool {
        self.0.scheme() == "http" || self.0.scheme() == "https"
    }

    pub fn as_url(&self) -> &Url {
        &self.0
    }
}

impl Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_remote() {
            return write!(f, "{}", self.0);
        }
        write!(f, "{}", self.as_rooted_path().to_str().unwrap_or(""))
    }
}

impl Debug for VirtualPath {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests_virtual_path {
    use super::*;

    #[test]
    fn absolute_paths() {
        let vp = VirtualPath::new("/tmp/a/b/c/d.txt");
        assert_eq!(Path::new("tmp/a/b/c/d.txt"),  vp.as_rootless_path());
        assert_eq!(Path::new("/tmp/a/b/c/d.txt"),  vp.as_rooted_path());

        let vp1 = VirtualPath::new("/tmp/a/b/c");
        assert_eq!(Path::new("/tmp/a/b/c/"),  vp1.as_rooted_path());

        let vp2 = VirtualPath::new("/tmp/a/b/c/");
        assert_eq!(Path::new("/tmp/a/b/c/"),  vp2.as_rooted_path());
    }

    #[test]
    fn relative_paths() {
        let vp = VirtualPath::new("c/d.txt");
        assert_eq!(Path::new("c/d.txt"),  vp.as_rootless_path());
        assert_eq!(Path::new("/c/d.txt"),  vp.as_rooted_path());

        let vp2 = VirtualPath::new("./c/d.txt");
        assert_eq!(Path::new("c/d.txt"),  vp2.as_rootless_path());
        assert_eq!(Path::new("/c/d.txt"),  vp2.as_rooted_path());

        let vp3 = VirtualPath::new("./c/../d.txt");
        assert_eq!(Path::new("d.txt"),  vp3.as_rootless_path());
        assert_eq!(Path::new("/d.txt"),  vp3.as_rooted_path());
    }

    #[test]
    fn join_happy_path() {
        let vp_file = VirtualPath::new("/tmp/a/b.txt");
        let vp2 = vp_file.join("x/z.txt");
        assert_eq!(Path::new("/tmp/a/x/z.txt"), vp2.as_rooted_path());
    }

    #[test]
    fn join_file() {
        let vp_dir = VirtualPath::new("/tmp/a/b/");
        let vp2 = vp_dir.join("x/z.txt");
        // the result is strange as vp2 is a directory.  Probably it is good.
        assert_eq!(Path::new("/tmp/a/x/z.txt"), vp2.as_rooted_path());
    }

    #[test]
    fn join_two_remotes_files(){
        let vp1 = VirtualPath::new("https://example.com/a/foo.typ");
        let vp2 = vp1.join("https://example.com/a/foo.typ");
        // the result is strange as vp2 is a directory.  Probably it is good.
        assert_eq!("https://example.com/a/foo.typ", format!("{}", vp2));
    }

    #[test]
    fn join_one_remote_file_to_one_local_file(){
        let vp1 = VirtualPath::new("https://example.com/a/foo.typ");
        let vp2 = vp1.join("./b/toto.typ");
        // the result is strange as vp2 is a directory.  Probably it is good.
        assert_eq!("https://example.com/a/b/toto.typ", format!("{}", vp2));
    }

    #[test]
    fn resolve(){
        let vp = VirtualPath::new("/tmp/a/foo.typ");
        assert_eq!(PathBuf::from("/tmp/a/tmp/a/foo.typ"),
                   vp.resolve(Path::new("/tmp/a")).unwrap());

        let vp2 = VirtualPath::new("tmp/a/foo.typ");
        assert_eq!(PathBuf::from("/tmp/a/tmp/a/foo.typ"),
                   vp2.resolve(Path::new("/tmp/a")).unwrap());

        let vp3 = VirtualPath::new("../x/foo.typ");
        assert!(vp3.resolve(Path::new("/tmp/a")).is_none());
    }

    #[test]
    fn within_root(){
        let root = Path::new("/tmp/a/b");

        assert_eq!(VirtualPath::new("/x/foo.txt"),
                   VirtualPath::within_root(Path::new("/tmp/a/b/x/foo.txt"), root).unwrap());

        assert!(VirtualPath::within_root(Path::new("/no-tmp/a/b/x/foo.txt"), root).is_none());

        assert!(VirtualPath::within_root(Path::new("../c"), root).is_none());
    }

    #[test]
    fn url_escaped_char(){
        let vp = VirtualPath::new("/tmp/a/#foo.typ");
        assert_eq!(Path::new("/tmp/a/#foo.typ"), vp.as_rooted_path());

        let vp2 = VirtualPath::new("/tmp/a/?foo.typ");
        assert_eq!(Path::new("/tmp/a/?foo.typ"), vp2.as_rooted_path());
    }

    #[test]
    fn join_with_file_with_url_escaped_char() {
        let vp_dir = VirtualPath::new("/tmp/#a/b/");
        let vp2 = vp_dir.join("x?/z.txt");
        // the result is strange as vp2 is a directory.  Probably it is good.
        assert_eq!(Path::new("/tmp/#a/x?/z.txt"), vp2.as_rooted_path());
    }

    #[test]
    fn is_remote() {
        let vp_file = VirtualPath::new("/tmp/a/foo.txt");
        assert!(!vp_file.is_remote());

        let vp_https = VirtualPath::new("https://google.com/tmp/a/foo.txt");
        assert!(vp_https.is_remote());

        let vp_http = VirtualPath::new("http://google.com/tmp/a/foo.txt");
        assert!(vp_http.is_remote());
    }

    #[test]
    fn display_trait() {
        let vp_file = VirtualPath::new("/tmp/a/foo.txt");
        assert_eq!("/tmp/a/foo.txt", format!("{}", vp_file));

        let vp_https = VirtualPath::new("https://google.com/tmp/a/foo.txt");
        assert_eq!("https://google.com/tmp/a/foo.txt", format!("{}", vp_https));
    }
}

/// Identifies a package.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct PackageSpec {
    /// The namespace the package lives in.
    pub namespace: EcoString,
    /// The name of the package within its namespace.
    pub name: EcoString,
    /// The package's version.
    pub version: PackageVersion,
}

impl FromStr for PackageSpec {
    type Err = EcoString;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut s = unscanny::Scanner::new(s);
        if !s.eat_if('@') {
            Err("package specification must start with '@'")?;
        }

        let namespace = s.eat_until('/');
        if namespace.is_empty() {
            Err("package specification is missing namespace")?;
        } else if !is_ident(namespace) {
            Err(eco_format!("`{namespace}` is not a valid package namespace"))?;
        }

        s.eat_if('/');

        let name = s.eat_until(':');
        if name.is_empty() {
            Err("package specification is missing name")?;
        } else if !is_ident(name) {
            Err(eco_format!("`{name}` is not a valid package name"))?;
        }

        s.eat_if(':');

        let version = s.after();
        if version.is_empty() {
            Err("package specification is missing version")?;
        }

        Ok(Self {
            namespace: namespace.into(),
            name: name.into(),
            version: version.parse()?,
        })
    }
}

impl Debug for PackageSpec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(self, f)
    }
}

impl Display for PackageSpec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "@{}/{}:{}", self.namespace, self.name, self.version)
    }
}

/// A package's version.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PackageVersion {
    /// The package's major version.
    pub major: u32,
    /// The package's minor version.
    pub minor: u32,
    /// The package's patch version.
    pub patch: u32,
}

impl PackageVersion {
    /// The current compiler version.
    pub fn compiler() -> Self {
        Self {
            major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
            minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
            patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
        }
    }
}

impl FromStr for PackageVersion {
    type Err = EcoString;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let mut next = |kind| {
            let part = parts
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| eco_format!("version number is missing {kind} version"))?;
            part.parse::<u32>()
                .map_err(|_| eco_format!("`{part}` is not a valid {kind} version"))
        };

        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if let Some(rest) = parts.next() {
            Err(eco_format!("version number has unexpected fourth component: `{rest}`"))?;
        }

        Ok(Self { major, minor, patch })
    }
}

impl Debug for PackageVersion {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(self, f)
    }
}

impl Display for PackageVersion {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for PackageVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PackageVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let string = EcoString::deserialize(d)?;
        string.parse().map_err(serde::de::Error::custom)
    }
}

