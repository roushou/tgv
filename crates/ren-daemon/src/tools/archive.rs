//! Unpacking a verified download into a folder, refusing any entry that
//! would land outside it: absolute or `..` paths, links that lead out, hard
//! links, and device files.

use ren_depot::{
    job::{HostFailure, ProgramCheck},
    source::{ArchiveFormat, RelativePath},
};
use std::{
    fs,
    io::{self, BufReader},
    path::{Component, Path, PathBuf},
};

/// A downloaded artifact, verified, to unpack.
#[derive(Debug)]
pub(crate) struct Download<'a> {
    pub path: &'a Path,
    pub format: ArchiveFormat,
}

impl Download<'_> {
    /// Unpack into `dest`, a new empty folder, and say whether the program
    /// is at `program_at` inside it. A program found is made executable.
    pub(crate) fn unpack(
        &self,
        dest: &Path,
        program_at: &RelativePath,
    ) -> Result<ProgramCheck, HostFailure> {
        fs::create_dir_all(dest).map_err(|_| HostFailure::Filesystem)?;
        let program = program_at
            .parts()
            .fold(dest.to_path_buf(), |path, part| path.join(part));
        match self.format {
            ArchiveFormat::TarGz => self.untar(dest)?,
            ArchiveFormat::Zip => self.unzip(dest)?,
            ArchiveFormat::Bare => {
                if let Some(parent) = program.parent() {
                    fs::create_dir_all(parent).map_err(|_| HostFailure::Filesystem)?;
                }
                fs::copy(self.path, &program).map_err(|_| HostFailure::Filesystem)?;
            }
        }
        let found = fs::symlink_metadata(&program).is_ok_and(|meta| meta.is_file());
        if !found {
            return Ok(ProgramCheck::Missing);
        }
        Self::make_executable(&program).map_err(|_| HostFailure::Filesystem)?;
        Ok(ProgramCheck::Found)
    }

    fn untar(&self, dest: &Path) -> Result<(), HostFailure> {
        let file = fs::File::open(self.path).map_err(|_| HostFailure::Filesystem)?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
        archive.set_preserve_permissions(true);
        archive.set_overwrite(false);
        let entries = archive.entries().map_err(|_| HostFailure::BadArchive)?;
        for entry in entries {
            let mut entry = entry.map_err(|_| HostFailure::BadArchive)?;
            let path = entry
                .path()
                .map_err(|_| HostFailure::BadArchive)?
                .into_owned();
            let kind = entry.header().entry_type();
            if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
                continue;
            }
            let allowed = if kind.is_file() || kind.is_dir() {
                true
            } else if kind.is_symlink() {
                entry
                    .link_name()
                    .ok()
                    .flatten()
                    .is_some_and(|target| Self::stays_inside(&path, &target))
            } else {
                // Hard links, devices, and the rest: never in a release.
                false
            };
            if !allowed {
                return Err(HostFailure::BadArchive);
            }
            match entry.unpack_in(dest) {
                Ok(true) => {}
                Ok(false) => return Err(HostFailure::BadArchive),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(HostFailure::BadArchive);
                }
                Err(_) => return Err(HostFailure::Filesystem),
            }
        }
        Ok(())
    }

    fn unzip(&self, dest: &Path) -> Result<(), HostFailure> {
        let file = fs::File::open(self.path).map_err(|_| HostFailure::Filesystem)?;
        let mut archive =
            zip::ZipArchive::new(BufReader::new(file)).map_err(|_| HostFailure::BadArchive)?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|_| HostFailure::BadArchive)?;
            if entry.is_symlink() {
                // Releases Ren installs from zip archives carry none.
                return Err(HostFailure::BadArchive);
            }
            let Some(name) = entry.enclosed_name() else {
                return Err(HostFailure::BadArchive);
            };
            let out = dest.join(name);
            if entry.is_dir() {
                fs::create_dir_all(&out).map_err(|_| HostFailure::Filesystem)?;
                continue;
            }
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|_| HostFailure::Filesystem)?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)
                .map_err(|_| HostFailure::BadArchive)?;
            io::copy(&mut entry, &mut file).map_err(|_| HostFailure::BadArchive)?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o755))
                    .map_err(|_| HostFailure::Filesystem)?;
            }
        }
        Ok(())
    }

    /// Whether a link at `entry` (relative to the archive's root) leading to
    /// `target` stays inside the archive's root.
    fn stays_inside(entry: &Path, target: &Path) -> bool {
        let mut depth: Vec<&std::ffi::OsStr> = Vec::new();
        let parent = entry.parent().unwrap_or(Path::new(""));
        for component in parent.components().chain(target.components()) {
            match component {
                Component::Normal(part) => depth.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if depth.pop().is_none() {
                        return false;
                    }
                }
                Component::RootDir | Component::Prefix(_) => return false,
            }
        }
        true
    }

    #[cfg(unix)]
    fn make_executable(program: &PathBuf) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(program)?.permissions().mode();
        fs::set_permissions(program, fs::Permissions::from_mode(mode | 0o755))
    }

    #[cfg(not(unix))]
    fn make_executable(_program: &PathBuf) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Download;
    use std::path::Path;

    #[test]
    fn links_may_not_lead_out_of_the_archive() {
        let inside =
            |entry: &str, target: &str| Download::stays_inside(Path::new(entry), Path::new(target));
        assert!(inside(
            "node/bin/npm",
            "../lib/node_modules/npm/bin/npm-cli.js"
        ));
        assert!(inside("node/bin/x", "y"));
        assert!(!inside("node/bin/npm", "../../../etc/passwd"));
        assert!(!inside("node/bin/npm", "/etc/passwd"));
        assert!(!inside("x", "../y"));
    }
}
