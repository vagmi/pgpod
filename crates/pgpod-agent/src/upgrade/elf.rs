//! Which shared libraries an old PostgreSQL installation actually needs.
//!
//! Staging an installation out of one image and running it inside another
//! only works if its libraries travel with it. Which ones those are is a
//! question the binaries answer themselves, in their `DT_NEEDED` entries —
//! so this reads them, rather than guessing from a list or copying the
//! whole `/usr/lib` (274 MB on a Debian PostgreSQL image, and still wrong
//! for anything installed elsewhere).
//!
//! `ldd` would answer the same question and is what `ops/build-pgbackrest.sh`
//! uses on the host. It is not available here: the agent runs inside an
//! arbitrary image, `ldd` is a distribution script rather than a
//! guaranteed tool, and running it means *executing* the dynamic loader
//! against a binary built for a different libc. Reading the file is
//! deterministic and testable without a container.
//!
//! **The glibc core is deliberately never staged.** The binaries are
//! executed by the *new* image's loader, and a loader and its `libc.so.6`
//! are one unit — pairing a 2.41 loader with a 2.36 libc fails at startup
//! with an undefined symbol, where relying on the new image's glibc works
//! by glibc's own backward-compatibility guarantee. That is the one
//! compatibility assumption this design makes, and
//! [`super::verify_staged_binaries`] proves it before any data is touched
//! rather than trusting it.

use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What one ELF file says about its dynamic dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicInfo {
    /// `DT_NEEDED` — sonames, not paths.
    pub needed: Vec<String>,
    /// `DT_RPATH` and `DT_RUNPATH`, with `$ORIGIN` already expanded.
    pub search_paths: Vec<PathBuf>,
}

const EI_NIDENT: usize = 16;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

/// Read one ELF file's dynamic section.
///
/// `Ok(None)` for anything that is not a 64-bit little-endian ELF with a
/// dynamic section — a shell script in `bin/`, a `.sql` file in the share
/// directory, a statically linked binary. Those are not errors: a
/// PostgreSQL installation is full of them.
pub fn dynamic_info(path: &Path) -> Result<Option<DynamicInfo>> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to open {}", path.display())),
    };

    let mut ident = [0u8; EI_NIDENT];
    if f.read_exact(&mut ident).is_err() {
        return Ok(None);
    }
    // 64-bit little-endian only. Every platform pgpod runs on is one, and
    // guessing at a foreign-endian ELF would produce nonsense offsets
    // rather than a clear refusal.
    if &ident[0..4] != b"\x7fELF" || ident[4] != 2 || ident[5] != 1 {
        return Ok(None);
    }

    let mut header = [0u8; 64];
    f.rewind()?;
    if f.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let e_phoff = u64le(&header[32..40]);
    let e_phentsize = u16le(&header[54..56]) as u64;
    let e_phnum = u16le(&header[56..58]) as u64;
    if e_phoff == 0 || e_phentsize < 56 || e_phnum == 0 {
        return Ok(None);
    }

    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // vaddr, offset, filesz
    let mut dynamic: Option<(u64, u64)> = None; // offset, filesz
    for i in 0..e_phnum {
        let mut ph = vec![0u8; e_phentsize as usize];
        f.seek(SeekFrom::Start(e_phoff + i * e_phentsize))?;
        if f.read_exact(&mut ph).is_err() {
            return Ok(None);
        }
        let p_type = u32le(&ph[0..4]);
        let p_offset = u64le(&ph[8..16]);
        let p_vaddr = u64le(&ph[16..24]);
        let p_filesz = u64le(&ph[32..40]);
        match p_type {
            PT_LOAD => loads.push((p_vaddr, p_offset, p_filesz)),
            PT_DYNAMIC => dynamic = Some((p_offset, p_filesz)),
            _ => {}
        }
    }

    let Some((dyn_off, dyn_size)) = dynamic else {
        // Statically linked, or an object file. Nothing to resolve.
        return Ok(None);
    };

    let mut entries = vec![0u8; dyn_size as usize];
    f.seek(SeekFrom::Start(dyn_off))?;
    if f.read_exact(&mut entries).is_err() {
        return Ok(None);
    }

    let mut needed_offsets = Vec::new();
    let mut rpath_offsets = Vec::new();
    let mut strtab_vaddr = None;
    let mut strtab_size = 0u64;
    for chunk in entries.chunks_exact(16) {
        let tag = u64le(&chunk[0..8]);
        let val = u64le(&chunk[8..16]);
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed_offsets.push(val),
            DT_RPATH | DT_RUNPATH => rpath_offsets.push(val),
            DT_STRTAB => strtab_vaddr = Some(val),
            DT_STRSZ => strtab_size = val,
            _ => {}
        }
    }

    let Some(strtab_vaddr) = strtab_vaddr else {
        return Ok(None);
    };
    // `DT_STRTAB` is a virtual address; the file offset it corresponds to
    // has to come from the PT_LOAD segment containing it. They are not
    // the same number in a position-independent executable.
    let Some(strtab_off) = vaddr_to_offset(&loads, strtab_vaddr) else {
        return Ok(None);
    };

    let cap = if strtab_size == 0 {
        1 << 20
    } else {
        strtab_size
    };
    let mut strtab = vec![0u8; cap as usize];
    f.seek(SeekFrom::Start(strtab_off))?;
    let read = f.read(&mut strtab)?;
    strtab.truncate(read);

    let origin = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut info = DynamicInfo::default();
    for off in needed_offsets {
        if let Some(s) = string_at(&strtab, off) {
            info.needed.push(s);
        }
    }
    for off in rpath_offsets {
        let Some(s) = string_at(&strtab, off) else {
            continue;
        };
        for part in s.split(':').filter(|p| !p.is_empty()) {
            // `$ORIGIN` is relative to the file being read, which for a
            // staged copy is where it *was*, not where it is. Resolved
            // here, while that is still known.
            let expanded = part
                .replace("${ORIGIN}", &origin.display().to_string())
                .replace("$ORIGIN", &origin.display().to_string());
            info.search_paths.push(PathBuf::from(expanded));
        }
    }
    Ok(Some(info))
}

/// Resolve the full transitive set of libraries `roots` need.
///
/// Returns the paths as they exist in *this* image. Anything matching
/// [`is_glibc_core`] is excluded along with everything it would have
/// dragged in, and anything already inside `skip_below` — the
/// installation directories being staged wholesale — is excluded too,
/// because copying it twice would only make the staged tree bigger.
pub fn resolve_dependencies(
    roots: &[PathBuf],
    skip_below: &[PathBuf],
) -> Result<BTreeSet<PathBuf>> {
    let mut resolved = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut queue: VecDeque<(PathBuf, Vec<PathBuf>)> = VecDeque::new();

    for root in roots {
        if let Some(info) = dynamic_info(root)? {
            queue.push_back((root.clone(), info.search_paths.clone()));
            for name in info.needed {
                if seen_names.insert(name.clone())
                    && let Some(found) = find_library(&name, &info.search_paths)
                {
                    queue.push_back((found, info.search_paths.clone()));
                }
            }
        }
    }

    while let Some((path, inherited)) = queue.pop_front() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let staged_already = skip_below.iter().any(|d| path.starts_with(d));
        if !is_glibc_core(&name) && !staged_already && !roots.contains(&path) {
            resolved.insert(path.clone());
        }
        if is_glibc_core(&name) {
            // Its own dependencies are glibc's business, on the image
            // that supplies it.
            continue;
        }

        let Some(info) = dynamic_info(&path)? else {
            continue;
        };
        let mut search = info.search_paths.clone();
        search.extend(inherited.iter().cloned());
        for needed in info.needed {
            if !seen_names.insert(needed.clone()) {
                continue;
            }
            if is_glibc_core(&needed) {
                continue;
            }
            if let Some(found) = find_library(&needed, &search) {
                queue.push_back((found, search.clone()));
            }
            // A soname that resolves nowhere is left to the loader to
            // complain about, with the file it was needed by in hand.
            // Refusing here would fail a staging step for a library the
            // new image may well have.
        }
    }

    Ok(resolved)
}

/// The glibc runtime, which must always come from the image the binaries
/// are *executed* in.
///
/// The loader and these libraries are versioned together: `ld.so` resolves
/// `GLIBC_PRIVATE` symbols against its own `libc.so.6` and nothing else.
/// Staging them would swap half of that pair.
pub fn is_glibc_core(soname: &str) -> bool {
    let base = soname.rsplit('/').next().unwrap_or(soname);
    if base.starts_with("ld-linux") || base.starts_with("ld64") || base == "ld.so" {
        return true;
    }
    if base.starts_with("libnss_") {
        return true;
    }
    let stem = base.split(".so").next().unwrap_or(base);
    matches!(
        stem,
        "libc"
            | "libm"
            | "libpthread"
            | "libdl"
            | "librt"
            | "libresolv"
            | "libutil"
            | "libnsl"
            | "libanl"
            | "libmvec"
            | "libBrokenLocale"
            | "libthread_db"
            | "libSegFault"
            | "libpcprofile"
    )
}

/// Where the loader would look for a soname, in the order it looks.
///
/// `DT_RPATH`/`DT_RUNPATH` first, then the conventional directories.
/// `/etc/ld.so.conf` is deliberately not parsed: its `include` globbing and
/// cache interaction are a distribution detail, and a library outside
/// these directories is reached through `DT_RUNPATH` in every PostgreSQL
/// packaging pgpod has met. One that is not shows up as a missing library
/// in the verification step, naming itself.
fn find_library(soname: &str, search_paths: &[PathBuf]) -> Option<PathBuf> {
    let default_dirs = [
        "/usr/local/lib",
        "/usr/local/lib64",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/lib64",
        "/usr/lib64",
        "/lib",
        "/usr/lib",
    ];
    search_paths
        .iter()
        .cloned()
        .chain(default_dirs.iter().map(PathBuf::from))
        .map(|dir| dir.join(soname))
        .find(|candidate| candidate.is_file())
}

fn vaddr_to_offset(loads: &[(u64, u64, u64)], vaddr: u64) -> Option<u64> {
    loads
        .iter()
        .find(|(v, _, size)| vaddr >= *v && vaddr < v + size)
        .map(|(v, off, _)| off + (vaddr - v))
}

fn string_at(strtab: &[u8], offset: u64) -> Option<String> {
    let start = usize::try_from(offset).ok()?;
    if start >= strtab.len() {
        return None;
    }
    let end = strtab[start..]
        .iter()
        .position(|b| *b == 0)
        .map(|p| start + p)?;
    Some(String::from_utf8_lossy(&strtab[start..end]).into_owned())
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_glibc_runtime_is_never_staged() {
        for name in [
            "libc.so.6",
            "libm.so.6",
            "libpthread.so.0",
            "libdl.so.2",
            "librt.so.1",
            "libresolv.so.2",
            "ld-linux-x86-64.so.2",
            "libnss_files.so.2",
            "/lib/x86_64-linux-gnu/libc.so.6",
        ] {
            assert!(is_glibc_core(name), "{name} must come from the new image");
        }
    }

    #[test]
    fn everything_postgresql_actually_links_is_staged() {
        // The libraries whose soname changes between distribution
        // releases are precisely the ones that make a staged installation
        // necessary: a bookworm PostgreSQL wants libicuuc.so.72 and a
        // trixie image has only .76.
        for name in [
            "libicuuc.so.72",
            "libicui18n.so.76",
            "libssl.so.3",
            "libcrypto.so.3",
            "libxml2.so.2",
            "liblz4.so.1",
            "libzstd.so.1",
            "libldap.so.2",
            "libcrypt.so.1",
            "libstdc++.so.6",
            "libgcc_s.so.1",
        ] {
            assert!(!is_glibc_core(name), "{name} must travel with the binaries");
        }
    }

    #[test]
    fn a_file_that_is_not_an_elf_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pg_wrapper");
        std::fs::write(&script, "#!/bin/sh\nexec postgres\n").unwrap();
        assert_eq!(dynamic_info(&script).unwrap(), None);

        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(dynamic_info(&empty).unwrap(), None);

        // A share directory is full of these.
        let sql = dir.path().join("vector--0.8.0.sql");
        std::fs::write(&sql, "CREATE FUNCTION ...").unwrap();
        assert_eq!(dynamic_info(&sql).unwrap(), None);
    }

    #[test]
    fn a_missing_file_is_not_an_error_either() {
        assert_eq!(
            dynamic_info(Path::new("/nonexistent/pgpod/postgres")).unwrap(),
            None
        );
    }

    #[test]
    fn dt_needed_is_read_from_a_real_binary() {
        // The test binary itself: dynamically linked against libc on a
        // glibc host, static under musl — which is how the agent itself
        // is built, so both outcomes are legitimate and the test asserts
        // whichever one applies rather than assuming a toolchain.
        let me = std::env::current_exe().expect("test binary path");
        match dynamic_info(&me).expect("readable") {
            Some(info) => assert!(
                info.needed.iter().any(|n| n.starts_with("libc.so")
                    || n.starts_with("libgcc_s")
                    || n.starts_with("ld-")),
                "a dynamic binary names its libc: {:?}",
                info.needed
            ),
            None => {
                // Statically linked. Nothing to resolve, which is exactly
                // what the staging step must not mistake for an error.
            }
        }
    }

    #[test]
    fn a_soname_resolves_through_the_search_path_before_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("libpgpod-test.so.9");
        std::fs::write(&lib, b"not really an elf").unwrap();
        let found = find_library("libpgpod-test.so.9", &[dir.path().to_path_buf()]);
        assert_eq!(found.as_deref(), Some(lib.as_path()));
        assert_eq!(find_library("libpgpod-test.so.9", &[]), None);
    }
}
