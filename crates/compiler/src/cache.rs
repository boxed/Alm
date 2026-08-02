//! The incremental build cache: what a module compiled to, so the next build
//! does not compile it again.
//!
//! One file per module under `.alm-stuff/`, holding everything a later build
//! needs from that module *without* looking at its source again: its interface
//! (for checking the modules that import it), the JavaScript it generated, the
//! names it exports, and its lint warnings. A module is reused when its own
//! source is unchanged **and** every interface it was checked against is
//! unchanged — the second half is what makes this safe, since a dependency
//! whose types changed can invalidate a dependent whose text did not.
//!
//! Everything else is treated as a miss rather than as something to reason
//! about: a missing file, a short read, a bad magic number, a format version
//! from another build, a different compiler binary. A miss only costs time, so
//! the cache never has to be *repaired* — but a wrong hit would be a wrong
//! program, which is why invalidation is deliberately blunt.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::ast::canonical as can;
use crate::ast::source::Associativity;
use crate::data::Name;
use crate::interface::{BinopDef, Interface};
use crate::reporting::annotation::{Position, Region};
use crate::reporting::Report;

/// Bump when anything about the format — or about what the fields *mean* —
/// changes. Old entries then miss instead of being misread.
const FORMAT_VERSION: u32 = 1;
const MAGIC: &[u8; 4] = b"ALMC";

/// The cache directory for a project. Versioned, so a format change starts a
/// new directory rather than colliding with entries it cannot read.
pub fn dir_for(project_root: &Path) -> PathBuf {
    project_root.join(".alm-stuff").join(format!("v{FORMAT_VERSION}"))
}

/// Module entries live under the kind of thing they hold — `js` for a compiled
/// module chunk, `check` for type-checker output. They are keyed by module path,
/// so without this a wasm build and a JavaScript build would take turns
/// overwriting each other's entries and neither would ever hit. The module
/// graph sits above both, since the project's shape is the same either way.
pub fn entries_dir(dir: &Path, kind: &str) -> PathBuf {
    dir.join(kind)
}

/// A fingerprint every entry is stamped with: change it and the whole cache
/// misses.
///
/// The compiler's own identity is in here (the executable's length and
/// modification time), so rebuilding alm invalidates every entry. That is the
/// difference between a cache that is merely fast and one that is trustworthy
/// while the compiler is being worked on: a code-generation change would
/// otherwise leave stale JavaScript in place, and the resulting bug would look
/// like it came from the source rather than from the cache.
pub fn fingerprint(flags: &[(&str, &str)]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    FORMAT_VERSION.hash(&mut h);
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(meta) = std::fs::metadata(&exe) {
            meta.len().hash(&mut h);
            if let Ok(modified) = meta.modified() {
                if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                    since.as_nanos().hash(&mut h);
                }
            }
        }
    }
    for (key, value) in flags {
        key.hash(&mut h);
        value.hash(&mut h);
    }
    h.finish()
}

pub fn hash_str(text: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// What one module contributed to the last build.
pub struct Entry {
    pub source_hash: u64,
    /// The interfaces this module was checked against, by name and content
    /// hash. A dependency that now hashes differently makes this entry stale
    /// even when the module's own text is untouched.
    pub deps: Vec<(Name, u64)>,
    /// The hash of `interface` as serialized here — what dependents record.
    pub interface_hash: u64,
    /// Left encoded until something asks for it. A build where nothing changed
    /// never asks: interfaces exist to check modules *against*, and there is
    /// nothing to check. Decoding all of them anyway was half the time of a
    /// no-op build on a 360-module project.
    pub interface: LazyInterface,
    /// The module's slice of the bundle, ready to concatenate.
    pub javascript: String,
    /// Top-level names, in declaration order, for the `Elm` exports object.
    pub exports: Vec<Name>,
    /// Whether the module mentions `Debug` — the `--optimize` check needs this
    /// and has no AST to look at when the module is reused.
    pub uses_debug: bool,
    /// Lint warnings, minus the path and source text, which the build already
    /// has in hand for every module it loaded.
    pub warnings: Vec<Report>,
}

/// An interface that has been read but not decoded.
pub struct LazyInterface {
    bytes: Vec<u8>,
}

impl LazyInterface {
    /// Decode it. Returns `None` if the bytes do not parse, which the caller
    /// must treat as a cache miss like any other.
    pub fn decode(&self) -> Option<Interface> {
        let mut r = Reader { bytes: &self.bytes, at: 0 };
        let interface = r.interface()?;
        (r.at == self.bytes.len()).then_some(interface)
    }
}

/// Where a module's entry lives. Keyed by the module's canonical *path*, not
/// its name: two packages can each declare a `Json.Decode`, and they are
/// different modules with different interfaces.
fn entry_path(dir: &Path, module_path: &Path) -> PathBuf {
    let mut h = std::hash::DefaultHasher::new();
    module_path.hash(&mut h);
    dir.join(format!("{:016x}.almc", h.finish()))
}

pub fn load(dir: &Path, module_path: &Path, fingerprint: u64) -> Option<Entry> {
    let bytes = std::fs::read(entry_path(dir, module_path)).ok()?;
    let mut r = Reader { bytes: &bytes, at: 0 };
    if r.take(4)? != MAGIC || r.u32()? != FORMAT_VERSION || r.u64()? != fingerprint {
        return None;
    }
    let entry = Entry {
        source_hash: r.u64()?,
        deps: r.vec(|r| Some((r.name()?, r.u64()?)))?,
        interface_hash: r.u64()?,
        interface: LazyInterface { bytes: r.interface_bytes()? },
        javascript: r.string()?,
        exports: r.vec(|r| r.name())?,
        uses_debug: r.u8()? != 0,
        warnings: r.vec(|r| r.report())?,
    };
    // A truncated or padded entry means the writer and reader disagree about
    // the format; refuse it rather than trust the part that happened to parse.
    if r.at != bytes.len() {
        return None;
    }
    Some(entry)
}

/// Serialize an interface on its own, to hash it. Dependents record this hash,
/// so it has to be exactly what `store` writes.
pub fn interface_hash(interface: &Interface) -> u64 {
    let mut w = Writer::default();
    w.interface(interface);
    hash_bytes(&w.bytes)
}

/// What `store` writes. Separate from `Entry` because a fresh build has a real
/// `Interface` in hand, while a build reading the cache has bytes it may never
/// need to decode.
pub struct Stored<'a> {
    pub source_hash: u64,
    pub deps: &'a [(Name, u64)],
    pub interface_hash: u64,
    pub interface: &'a Interface,
    pub javascript: &'a str,
    pub exports: &'a [Name],
    pub uses_debug: bool,
    pub warnings: &'a [Report],
}

pub fn store(dir: &Path, module_path: &Path, fingerprint: u64, entry: &Stored) {
    let mut w = Writer::default();
    w.bytes.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(fingerprint);
    w.u64(entry.source_hash);
    w.vec(entry.deps, |w, (name, hash)| {
        w.name(name);
        w.u64(*hash);
    });
    w.u64(entry.interface_hash);
    w.interface(entry.interface);
    w.string(entry.javascript);
    w.vec(entry.exports, |w, name| w.name(name));
    w.u8(entry.uses_debug as u8);
    w.vec(entry.warnings, |w, report| w.report(report));

    // Failing to write is not a build failure: the next build just misses.
    // Write-then-rename so a build killed mid-write cannot leave a torn entry
    // behind (a torn entry would be rejected on read, but only after the
    // partial file had displaced a good one).
    write_atomically(dir, &entry_path(dir, module_path), &w.bytes);
}

/// Failing to write is not a build failure: the next build just misses.
/// Write-then-rename, so a build killed mid-write cannot leave a torn entry
/// behind — a torn entry would be rejected on read, but only after the partial
/// file had displaced a good one.
fn write_atomically(dir: &Path, path: &Path, bytes: &[u8]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // The cache ignores itself, so nobody has to remember to ignore it and no
    // project ends up with build output committed. Cargo does the same for
    // `target/`. At the top of `.alm-stuff` rather than inside the versioned
    // directory, so a later format version is covered without being asked.
    let mut root = dir;
    while root.file_name().is_some_and(|n| n != ".alm-stuff") {
        match root.parent() {
            Some(parent) => root = parent,
            None => break,
        }
    }
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Created by alm. Build cache; safe to delete.\n*\n");
    }
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

// ------------------------------------------------------------------ writing

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, n: u8) {
        self.bytes.push(n);
    }
    fn u32(&mut self, n: u32) {
        self.bytes.extend_from_slice(&n.to_le_bytes());
    }
    fn u64(&mut self, n: u64) {
        self.bytes.extend_from_slice(&n.to_le_bytes());
    }
    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.bytes.extend_from_slice(s.as_bytes());
    }
    fn name(&mut self, n: &Name) {
        self.string(n.as_str());
    }
    fn vec<T>(&mut self, items: &[T], mut each: impl FnMut(&mut Self, &T)) {
        self.u32(items.len() as u32);
        for item in items {
            each(self, item);
        }
    }
    /// Maps and sets are written in SORTED key order. Their iteration order is
    /// not stable between runs, and these bytes are hashed — an unsorted write
    /// would make a module's interface hash differ from itself and invalidate
    /// every dependent on every build.
    fn map<V>(&mut self, m: &HashMap<Name, V>, mut each: impl FnMut(&mut Self, &V)) {
        let mut keys: Vec<&Name> = m.keys().collect();
        keys.sort();
        self.u32(keys.len() as u32);
        for key in keys {
            self.name(key);
            each(self, &m[key]);
        }
    }
    fn set(&mut self, s: &HashSet<Name>) {
        let mut names: Vec<&Name> = s.iter().collect();
        names.sort();
        self.vec(&names, |w, n| w.name(n));
    }

    fn tipe(&mut self, t: &can::Type) {
        use can::Type::*;
        match t {
            Var(n) => {
                self.u8(0);
                self.name(n);
            }
            Lambda(a, b) => {
                self.u8(1);
                self.tipe(a);
                self.tipe(b);
            }
            Type(home, name, args) => {
                self.u8(2);
                self.name(home);
                self.name(name);
                self.vec(args, |w, a| w.tipe(a));
            }
            Record(fields, ext) => {
                self.u8(3);
                self.vec(fields, |w, (n, t)| {
                    w.name(n);
                    w.tipe(t);
                });
                match ext {
                    Some(n) => {
                        self.u8(1);
                        self.name(n);
                    }
                    None => self.u8(0),
                }
            }
            Unit => self.u8(4),
            Tuple(a, b, c) => {
                self.u8(5);
                self.tipe(a);
                self.tipe(b);
                match c {
                    Some(c) => {
                        self.u8(1);
                        self.tipe(c);
                    }
                    None => self.u8(0),
                }
            }
        }
    }

    fn region(&mut self, r: &Region) {
        self.u32(r.start.row);
        self.u32(r.start.col);
        self.u32(r.end.row);
        self.u32(r.end.col);
    }

    fn union(&mut self, u: &can::Union) {
        self.name(&u.name);
        self.vec(&u.vars, |w, v| w.name(v));
        self.vec(&u.ctors, |w, c| {
            w.name(&c.name);
            w.u32(c.index);
            w.vec(&c.args, |w, a| w.tipe(a));
            w.region(&c.region);
        });
    }

    fn interface(&mut self, i: &Interface) {
        self.map(&i.values, |w, t| w.tipe(t));
        self.set(&i.value_names);
        self.map(&i.unions, |w, u| w.union(u));
        self.set(&i.open_unions);
        self.map(&i.aliases, |w, (vars, body)| {
            w.vec(vars, |w, v| w.name(v));
            w.tipe(body);
        });
        self.map(&i.binops, |w, b| {
            w.u8(match b.associativity {
                Associativity::Left => 0,
                Associativity::Non => 1,
                Associativity::Right => 2,
            });
            w.u8(b.precedence);
            w.name(&b.function);
            match &b.tipe {
                Some(t) => {
                    w.u8(1);
                    w.tipe(t);
                }
                None => w.u8(0),
            }
        });
    }

    fn report(&mut self, r: &Report) {
        self.string(&r.title);
        self.region(&r.region);
        self.string(&r.message);
    }
}

// ------------------------------------------------------------------ reading

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.bytes.get(self.at..self.at + n)?;
        self.at += n;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take(len)?).ok().map(str::to_string)
    }
    fn name(&mut self) -> Option<Name> {
        let len = self.u32()? as usize;
        Some(Name::from(std::str::from_utf8(self.take(len)?).ok()?))
    }
    fn vec<T>(&mut self, mut each: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let len = self.u32()? as usize;
        // A corrupt length must not make this allocate gigabytes before it
        // fails: the count cannot exceed what is left to read.
        if len > self.bytes.len() - self.at {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(each(self)?);
        }
        Some(out)
    }
    fn map<V>(&mut self, mut each: impl FnMut(&mut Self) -> Option<V>) -> Option<HashMap<Name, V>> {
        let pairs = self.vec(|r| Some((r.name()?, each(r)?)))?;
        Some(pairs.into_iter().collect())
    }
    fn set(&mut self) -> Option<HashSet<Name>> {
        Some(self.vec(|r| r.name())?.into_iter().collect())
    }
    fn option<T>(&mut self, each: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(each(self)?)),
            _ => None,
        }
    }

    fn tipe(&mut self) -> Option<can::Type> {
        use can::Type::*;
        Some(match self.u8()? {
            0 => Var(self.name()?),
            1 => Lambda(Rc::new(self.tipe()?), Rc::new(self.tipe()?)),
            2 => Type(
                self.name()?,
                self.name()?,
                Rc::new(self.vec(|r| r.tipe())?),
            ),
            3 => Record(
                Rc::new(self.vec(|r| Some((r.name()?, r.tipe()?)))?),
                self.option(|r| r.name())?,
            ),
            4 => Unit,
            5 => Tuple(
                Rc::new(self.tipe()?),
                Rc::new(self.tipe()?),
                self.option(|r| r.tipe())?.map(Rc::new),
            ),
            _ => return None,
        })
    }

    fn region(&mut self) -> Option<Region> {
        Some(Region::new(
            Position::new(self.u32()?, self.u32()?),
            Position::new(self.u32()?, self.u32()?),
        ))
    }

    fn union(&mut self) -> Option<can::Union> {
        Some(can::Union {
            name: self.name()?,
            vars: self.vec(|r| r.name())?,
            ctors: self.vec(|r| {
                Some(can::UnionCtor {
                    name: r.name()?,
                    index: r.u32()?,
                    args: r.vec(|r| r.tipe())?,
                    region: r.region()?,
                })
            })?,
        })
    }

    fn interface(&mut self) -> Option<Interface> {
        Some(Interface {
            values: self.map(|r| r.tipe())?,
            value_names: self.set()?,
            unions: self.map(|r| r.union())?,
            open_unions: self.set()?,
            aliases: self.map(|r| Some((r.vec(|r| r.name())?, r.tipe()?)))?,
            binops: self.map(|r| {
                Some(BinopDef {
                    associativity: match r.u8()? {
                        0 => Associativity::Left,
                        1 => Associativity::Non,
                        2 => Associativity::Right,
                        _ => return None,
                    },
                    precedence: r.u8()?,
                    function: r.name()?,
                    tipe: r.option(|r| r.tipe())?,
                })
            })?,
        })
    }

    /// The interface's encoded bytes, by decoding it and remembering the span
    /// it covered. Decoding twice would defeat the point, so this is only for
    /// the *stored* form — the entry keeps the bytes and decodes on demand.
    fn interface_bytes(&mut self) -> Option<Vec<u8>> {
        let start = self.at;
        self.skip_interface()?;
        Some(self.bytes[start..self.at].to_vec())
    }

    /// Walk an interface's encoding without building anything from it.
    fn skip_interface(&mut self) -> Option<()> {
        self.skip_map(|r| r.skip_type())?; // values
        self.skip_vec(|r| r.skip_str())?; // value_names
        self.skip_map(|r| r.skip_union())?; // unions
        self.skip_vec(|r| r.skip_str())?; // open_unions
        self.skip_map(|r| {
            r.skip_vec(|r| r.skip_str())?;
            r.skip_type()
        })?; // aliases
        self.skip_map(|r| {
            r.u8()?;
            r.u8()?;
            r.skip_str()?;
            r.skip_option(|r| r.skip_type())
        })?; // binops
        Some(())
    }

    fn skip_str(&mut self) -> Option<()> {
        let len = self.u32()? as usize;
        self.take(len)?;
        Some(())
    }
    fn skip_vec(&mut self, mut each: impl FnMut(&mut Self) -> Option<()>) -> Option<()> {
        let len = self.u32()? as usize;
        if len > self.bytes.len() - self.at {
            return None;
        }
        for _ in 0..len {
            each(self)?;
        }
        Some(())
    }
    fn skip_map(&mut self, mut each: impl FnMut(&mut Self) -> Option<()>) -> Option<()> {
        self.skip_vec(|r| {
            r.skip_str()?;
            each(r)
        })
    }
    fn skip_option(&mut self, each: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        match self.u8()? {
            0 => Some(()),
            1 => each(self),
            _ => None,
        }
    }
    fn skip_type(&mut self) -> Option<()> {
        match self.u8()? {
            0 => self.skip_str(),
            1 => {
                self.skip_type()?;
                self.skip_type()
            }
            2 => {
                self.skip_str()?;
                self.skip_str()?;
                self.skip_vec(|r| r.skip_type())
            }
            3 => {
                self.skip_vec(|r| {
                    r.skip_str()?;
                    r.skip_type()
                })?;
                self.skip_option(|r| r.skip_str())
            }
            4 => Some(()),
            5 => {
                self.skip_type()?;
                self.skip_type()?;
                self.skip_option(|r| r.skip_type())
            }
            _ => None,
        }
    }
    fn skip_union(&mut self) -> Option<()> {
        self.skip_str()?;
        self.skip_vec(|r| r.skip_str())?;
        self.skip_vec(|r| {
            r.skip_str()?;
            r.u32()?;
            r.skip_vec(|r| r.skip_type())?;
            r.region().map(|_| ())
        })
    }

    fn report(&mut self) -> Option<Report> {
        Some(Report {
            title: self.string()?,
            region: self.region()?,
            message: self.string()?,
            elm: None,
        })
    }
}

// ------------------------------------------------------- the module graph

/// What a build learned about one file's *place in the project*, as opposed to
/// what it compiled to: the name it declares and the files its imports resolve
/// to. Cached because rediscovering it means reading and parsing every module
/// and re-resolving every import against the search path — on a 360-module
/// project that was 63 ms of a 150 ms incremental build, to arrive at the same
/// graph as last time.
pub struct GraphRecord {
    /// Modification time in nanoseconds since the epoch, and the file's length.
    /// Together these stand in for "unchanged". Unlike the per-module entries,
    /// which hash contents, this is a *timestamp* check — the whole point is to
    /// avoid reading the file. It is what elm, cargo and make all do, and it
    /// accepts one risk in exchange: a file swapped for a same-length variant
    /// with an older timestamp reads as untouched. `ALM_NO_CACHE=1` is the way
    /// out if that ever happens.
    pub mtime_ns: u64,
    pub size: u64,
    pub declared_name: Name,
    /// The source directory this file was found in, which decides where *its*
    /// imports are searched for. Only needed if the file turns out to have
    /// changed, but recorded so that case does not need a second pass.
    pub matched_dir: PathBuf,
    pub imports: Vec<(Name, PathBuf)>,
}

#[derive(Default)]
pub struct Graph {
    pub modules: HashMap<PathBuf, GraphRecord>,
}

/// `(mtime_ns, size)` for a path, or `None` if it cannot be stat'd — which
/// counts as changed.
pub fn stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime as u64, meta.len()))
}

impl Graph {
    /// The record for `path`, if the file on disk is the one it describes.
    pub fn unchanged(&self, path: &Path) -> Option<&GraphRecord> {
        let record = self.modules.get(path)?;
        let (mtime_ns, size) = stamp(path)?;
        (record.mtime_ns == mtime_ns && record.size == size).then_some(record)
    }
}

fn graph_path(dir: &Path) -> PathBuf {
    dir.join("graph.almg")
}

pub fn load_graph(dir: &Path, fingerprint: u64) -> Option<Graph> {
    let bytes = std::fs::read(graph_path(dir)).ok()?;
    let mut r = Reader { bytes: &bytes, at: 0 };
    if r.take(4)? != MAGIC || r.u32()? != FORMAT_VERSION || r.u64()? != fingerprint {
        return None;
    }
    let records = r.vec(|r| {
        Some((
            PathBuf::from(r.string()?),
            GraphRecord {
                mtime_ns: r.u64()?,
                size: r.u64()?,
                declared_name: r.name()?,
                matched_dir: PathBuf::from(r.string()?),
                imports: r.vec(|r| Some((r.name()?, PathBuf::from(r.string()?))))?,
            },
        ))
    })?;
    (r.at == bytes.len()).then(|| Graph { modules: records.into_iter().collect() })
}

pub fn store_graph(dir: &Path, fingerprint: u64, graph: &Graph) {
    let mut w = Writer::default();
    w.bytes.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(fingerprint);
    // Sorted, so the file is reproducible rather than following hash order.
    let mut paths: Vec<&PathBuf> = graph.modules.keys().collect();
    paths.sort();
    w.vec(&paths, |w, path| {
        let record = &graph.modules[*path];
        w.string(&path.display().to_string());
        w.u64(record.mtime_ns);
        w.u64(record.size);
        w.name(&record.declared_name);
        w.string(&record.matched_dir.display().to_string());
        w.vec(&record.imports, |w, (name, path)| {
            w.name(name);
            w.string(&path.display().to_string());
        });
    });

    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = graph_path(dir);
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, &w.bytes).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

// ------------------------------------------------- type checking (wasm/native)

/// What type checking one module produced. Cached for the back ends that need
/// more from the front end than its interface: monomorphization reads every
/// module's canonical AST *and* the type of every expression in it, so those
/// builds cannot reuse a module wholesale the way a JavaScript build can.
///
/// They can, though, skip the expensive half. Parsing and canonicalizing the
/// whole project costs ~74 ms where type checking costs ~711 ms, so the AST is
/// simply rebuilt from source every time and never serialized — which is what
/// keeps this small, since the canonical AST is 23 expression forms and 12
/// pattern forms of surface area to get exactly right.
pub struct CheckEntry {
    pub source_hash: u64,
    pub deps: Vec<(Name, u64)>,
    pub interface_hash: u64,
    /// Every top-level definition's type.
    pub types: HashMap<Name, can::Type>,
    /// Every expression's type, keyed by source region.
    pub node_types: HashMap<Region, can::Type>,
}

pub struct CheckStored<'a> {
    pub source_hash: u64,
    pub deps: &'a [(Name, u64)],
    pub interface_hash: u64,
    pub types: &'a HashMap<Name, can::Type>,
    pub node_types: &'a HashMap<Region, can::Type>,
}

/// Types are written once each and referred to by index afterwards.
///
/// This is what makes the whole thing viable. Written out node by node, the
/// per-expression type table of a 59k-line application is 11 million type nodes
/// — the same types over and over, since every `List.map` call site mentions
/// the same `List`, and inference shares them through `Rc` in memory. Hash-consed
/// it is **21 thousand** distinct types, a 520:1 collapse, and decoding rebuilds
/// the sharing instead of inflating 132 MB of duplicates.
#[derive(Default)]
struct TypeTable {
    nodes: Vec<TypeNode>,
    index: HashMap<TypeNode, u32>,
    /// Interning results keyed by `Rc` identity. Inference shares subtrees
    /// aggressively, so without this the walk re-descends the same type
    /// millions of times to reach the same answer — 11 million visits on a
    /// 59k-line project, which cost more than the type checking being cached.
    /// With it the walk sees each shared subtree once.
    by_pointer: HashMap<usize, u32>,
    /// The same, for argument and field lists behind `Rc<Vec<…>>`.
    lists: HashMap<usize, Vec<u32>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum TypeNode {
    Var(Name),
    Lambda(u32, u32),
    Type(Name, Name, Vec<u32>),
    Record(Vec<(Name, u32)>, Option<Name>),
    Unit,
    Tuple(u32, u32, Option<u32>),
}

impl TypeTable {
    /// The index for `tipe`, adding it (and anything under it) if new. Children
    /// are interned first, so a node only ever refers to smaller indices and a
    /// reader can build the table front to back in one pass.
    fn intern(&mut self, tipe: &can::Type) -> u32 {
        use can::Type::*;
        let node = match tipe {
            Var(n) => TypeNode::Var(n.clone()),
            Lambda(a, b) => TypeNode::Lambda(self.shared(a), self.shared(b)),
            Type(home, name, args) => {
                let args = self.shared_list(args, |table, t| table.intern(t));
                TypeNode::Type(home.clone(), name.clone(), args)
            }
            Record(fields, ext) => {
                let indices = self.shared_list(fields, |table, (_, t)| table.intern(t));
                let names = fields.iter().map(|(n, _)| n.clone());
                TypeNode::Record(names.zip(indices).collect(), ext.clone())
            }
            Unit => TypeNode::Unit,
            Tuple(a, b, c) => TypeNode::Tuple(
                self.shared(a),
                self.shared(b),
                c.as_ref().map(|c| self.shared(c)),
            ),
        };
        if let Some(&at) = self.index.get(&node) {
            return at;
        }
        let at = self.nodes.len() as u32;
        self.nodes.push(node.clone());
        self.index.insert(node, at);
        at
    }

    /// `intern`, answered from the pointer memo when this exact `Rc` has been
    /// seen before.
    fn shared(&mut self, tipe: &Rc<can::Type>) -> u32 {
        let key = Rc::as_ptr(tipe) as usize;
        if let Some(&at) = self.by_pointer.get(&key) {
            return at;
        }
        let at = self.intern(tipe);
        self.by_pointer.insert(key, at);
        at
    }

    /// The same for a shared list, whose elements are interned in order.
    fn shared_list<T>(
        &mut self,
        list: &Rc<Vec<T>>,
        mut each: impl FnMut(&mut Self, &T) -> u32,
    ) -> Vec<u32> {
        let key = Rc::as_ptr(list) as usize;
        if let Some(indices) = self.lists.get(&key) {
            return indices.clone();
        }
        let indices: Vec<u32> = list.iter().map(|item| each(self, item)).collect();
        self.lists.insert(key, indices.clone());
        indices
    }
}

pub fn load_check(dir: &Path, module_path: &Path, fingerprint: u64) -> Option<CheckEntry> {
    let bytes = std::fs::read(entry_path(dir, module_path)).ok()?;
    let mut r = Reader { bytes: &bytes, at: 0 };
    if r.take(4)? != MAGIC || r.u32()? != FORMAT_VERSION || r.u64()? != fingerprint {
        return None;
    }
    let source_hash = r.u64()?;
    let deps = r.vec(|r| Some((r.name()?, r.u64()?)))?;
    let interface_hash = r.u64()?;

    // The table, front to back: every node's children are already built.
    let mut built: Vec<can::Type> = Vec::new();
    let count = r.u32()? as usize;
    if count > r.bytes.len() - r.at {
        return None;
    }
    for _ in 0..count {
        let at = |i: u32, built: &Vec<can::Type>| built.get(i as usize).cloned();
        let node = match r.u8()? {
            0 => can::Type::Var(r.name()?),
            1 => can::Type::Lambda(
                Rc::new(at(r.u32()?, &built)?),
                Rc::new(at(r.u32()?, &built)?),
            ),
            2 => can::Type::Type(
                r.name()?,
                r.name()?,
                Rc::new(r.vec(|r| r.u32())?.into_iter().map(|i| at(i, &built)).collect::<Option<Vec<_>>>()?),
            ),
            3 => can::Type::Record(
                Rc::new(
                    r.vec(|r| Some((r.name()?, r.u32()?)))?
                        .into_iter()
                        .map(|(n, i)| Some((n, at(i, &built)?)))
                        .collect::<Option<Vec<_>>>()?,
                ),
                r.option(|r| r.name())?,
            ),
            4 => can::Type::Unit,
            5 => can::Type::Tuple(
                Rc::new(at(r.u32()?, &built)?),
                Rc::new(at(r.u32()?, &built)?),
                match r.option(|r| r.u32())? {
                    Some(i) => Some(Rc::new(at(i, &built)?)),
                    None => None,
                },
            ),
            _ => return None,
        };
        built.push(node);
    }

    let types = r
        .vec(|r| Some((r.name()?, r.u32()?)))?
        .into_iter()
        .map(|(n, i)| Some((n, built.get(i as usize)?.clone())))
        .collect::<Option<HashMap<_, _>>>()?;
    let node_types = r
        .vec(|r| Some((r.region()?, r.u32()?)))?
        .into_iter()
        .map(|(region, i)| Some((region, built.get(i as usize)?.clone())))
        .collect::<Option<HashMap<_, _>>>()?;

    (r.at == bytes.len()).then_some(CheckEntry {
        source_hash,
        deps,
        interface_hash,
        types,
        node_types,
    })
}

pub fn store_check(dir: &Path, module_path: &Path, fingerprint: u64, entry: &CheckStored) {
    let mut table = TypeTable::default();
    // Sorted, so the bytes do not follow hash order and an unchanged module
    // keeps producing the same file.
    let mut types: Vec<(&Name, u32)> =
        entry.types.iter().map(|(n, t)| (n, table.intern(t))).collect();
    types.sort_by(|a, b| a.0.cmp(b.0));
    let mut node_types: Vec<(&Region, u32)> =
        entry.node_types.iter().map(|(r, t)| (r, table.intern(t))).collect();
    node_types.sort_by_key(|(r, _)| (r.start.row, r.start.col, r.end.row, r.end.col));

    let mut w = Writer::default();
    w.bytes.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(fingerprint);
    w.u64(entry.source_hash);
    w.vec(entry.deps, |w, (name, hash)| {
        w.name(name);
        w.u64(*hash);
    });
    w.u64(entry.interface_hash);

    w.u32(table.nodes.len() as u32);
    for node in &table.nodes {
        match node {
            TypeNode::Var(n) => {
                w.u8(0);
                w.name(n);
            }
            TypeNode::Lambda(a, b) => {
                w.u8(1);
                w.u32(*a);
                w.u32(*b);
            }
            TypeNode::Type(home, name, args) => {
                w.u8(2);
                w.name(home);
                w.name(name);
                w.vec(args, |w, a| w.u32(*a));
            }
            TypeNode::Record(fields, ext) => {
                w.u8(3);
                w.vec(fields, |w, (n, i)| {
                    w.name(n);
                    w.u32(*i);
                });
                match ext {
                    Some(n) => {
                        w.u8(1);
                        w.name(n);
                    }
                    None => w.u8(0),
                }
            }
            TypeNode::Unit => w.u8(4),
            TypeNode::Tuple(a, b, c) => {
                w.u8(5);
                w.u32(*a);
                w.u32(*b);
                match c {
                    Some(c) => {
                        w.u8(1);
                        w.u32(*c);
                    }
                    None => w.u8(0),
                }
            }
        }
    }

    w.vec(&types, |w, (name, at)| {
        w.name(name);
        w.u32(*at);
    });
    w.vec(&node_types, |w, (region, at)| {
        w.region(region);
        w.u32(*at);
    });

    write_atomically(dir, &entry_path(dir, module_path), &w.bytes);
}
