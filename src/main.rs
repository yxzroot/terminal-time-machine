use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, String>;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const O_NOFOLLOW: i32 = 0o400_000;

#[derive(Debug)]
struct Store {
    root: PathBuf,
    objects: PathBuf,
    snapshots: PathBuf,
    temporary: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    digest: String,
    size: u64,
    modified: u64,
    mode: u32,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryEntry {
    modified: u64,
    mode: u32,
    path: PathBuf,
}

#[derive(Debug)]
struct Snapshot {
    created: u64,
    source: PathBuf,
    directories: Vec<DirectoryEntry>,
    files: Vec<FileEntry>,
}

#[derive(Debug)]
struct SnapshotSummary {
    id: String,
    directories: usize,
    files: usize,
    bytes: u64,
    skipped: usize,
}

#[derive(Debug)]
struct VerificationSummary {
    snapshots: usize,
    files: usize,
    objects: usize,
    bytes: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{} {error}", paint("error:", "31"));
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();

    if args.is_empty() || args[0] == OsStr::new("--help") || args[0] == OsStr::new("help") {
        print_usage();
        return Ok(());
    }
    if args[0] == OsStr::new("--version") || args[0] == OsStr::new("version") {
        println!("tt {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let store_path = if args.first() == Some(&OsString::from("--store")) {
        if args.len() < 3 {
            return Err("--store needs a directory followed by a command".into());
        }
        args.remove(0);
        Some(PathBuf::from(args.remove(0)))
    } else {
        None
    };

    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .ok_or_else(|| "command must be valid UTF-8".to_string())?
        .to_owned();
    args.remove(0);

    if !matches!(
        command.as_str(),
        "snapshot" | "list" | "show" | "diff" | "verify" | "restore"
    ) {
        return Err(format!("unknown command `{command}`; run `tt --help`"));
    }

    let store = Store::open(store_path);
    match command.as_str() {
        "snapshot" => {
            if args.len() != 1 {
                return Err("usage: tt snapshot <directory>".into());
            }
            let summary = store.snapshot(&PathBuf::from(args.remove(0)))?;
            println!(
                "{} {}  {} directories, {} files, {}",
                paint("Created", "32"),
                paint(&summary.id, "36"),
                summary.directories,
                summary.files,
                format_bytes(summary.bytes),
            );
            if summary.skipped > 0 {
                println!(
                    "{} skipped {} symlink or special file entries",
                    paint("Note:", "33"),
                    summary.skipped
                );
            }
        }
        "list" => {
            if !args.is_empty() {
                return Err("usage: tt list".into());
            }
            let snapshots = store.list()?;
            if snapshots.is_empty() {
                println!("No snapshots yet.");
            } else {
                println!(
                    "{:<30} {:<14} {:<8} {:<8} SOURCE",
                    "ID", "CREATED", "DIRS", "FILES"
                );
                for (id, snapshot) in snapshots {
                    println!(
                        "{:<30} {:<14} {:<8} {:<8} {}",
                        id,
                        snapshot.created,
                        snapshot.directories.len(),
                        snapshot.files.len(),
                        snapshot.source.display()
                    );
                }
            }
        }
        "show" => {
            if args.len() != 1 {
                return Err("usage: tt show <snapshot-id>".into());
            }
            let id = required_id(&args[0])?;
            let snapshot = store.read_snapshot(id)?;
            println!("{} {id}", paint("Snapshot", "36"));
            println!("Created: {}", snapshot.created);
            println!("Source:  {}", snapshot.source.display());
            println!("Dirs:    {}", snapshot.directories.len());
            println!("Files:   {}", snapshot.files.len());
            println!();
            for entry in snapshot.directories {
                println!("{:>10}  {}/", "directory", entry.path.display());
            }
            for entry in snapshot.files {
                println!("{:>10}  {}", format_bytes(entry.size), entry.path.display());
            }
        }
        "diff" => {
            if args.len() != 2 {
                return Err("usage: tt diff <older-snapshot-id> <newer-snapshot-id>".into());
            }
            let older = store.read_snapshot(required_id(&args[0])?)?;
            let newer = store.read_snapshot(required_id(&args[1])?)?;
            let (added, changed, removed) = diff_snapshots(&older, &newer);
            println!(
                "{} added, {} changed, {} removed",
                paint(&added.to_string(), "32"),
                paint(&changed.to_string(), "33"),
                paint(&removed.to_string(), "31")
            );
        }
        "verify" => {
            if args.len() > 1 {
                return Err("usage: tt verify [snapshot-id]".into());
            }
            let id = args.first().map(|value| required_id(value)).transpose()?;
            let summary = store.verify(id)?;
            println!(
                "{} {} snapshot(s), {} files, {} unique objects, {}",
                paint("Verified", "32"),
                summary.snapshots,
                summary.files,
                summary.objects,
                format_bytes(summary.bytes)
            );
        }
        "restore" => {
            let (id, destination, force) = parse_restore_args(&args)?;
            let restored = store.restore(id, &destination, force)?;
            println!(
                "{} {} files into {}",
                paint("Restored", "32"),
                restored,
                destination.display()
            );
        }
        _ => unreachable!("command was checked before opening the snapshot store"),
    }

    Ok(())
}

impl Store {
    fn open(root: Option<PathBuf>) -> Self {
        let root = root.unwrap_or_else(default_store_path);
        let objects = root.join("objects");
        let snapshots = root.join("snapshots");
        let temporary = root.join("tmp");

        Self {
            root,
            objects,
            snapshots,
            temporary,
        }
    }

    fn ensure_layout(&self) -> Result<()> {
        for directory in [&self.root, &self.objects, &self.snapshots, &self.temporary] {
            fs::create_dir_all(directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        }

        Ok(())
    }

    fn snapshot(&self, source: &Path) -> Result<SnapshotSummary> {
        let source = fs::canonicalize(source)
            .map_err(|error| format!("cannot access {}: {error}", source.display()))?;
        let source_metadata = fs::metadata(&source)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        if !source_metadata.is_dir() {
            return Err(format!("{} is not a directory", source.display()));
        }

        let store_root = canonicalize_for_comparison(&self.root)?;
        if store_root.starts_with(&source) {
            return Err(format!(
                "the snapshot store ({}) is inside the source directory; choose a different --store directory",
                store_root.display()
            ));
        }

        let mut paths = Vec::new();
        let mut directories = Vec::new();
        let mut skipped = 0;
        collect_entries(&source, &source, &mut paths, &mut directories, &mut skipped)?;
        paths.sort();
        directories.sort_by(|left, right| left.path.cmp(&right.path));
        self.ensure_layout()?;

        let mut files = Vec::with_capacity(paths.len());
        let mut bytes = 0_u64;
        for path in paths {
            let relative = path
                .strip_prefix(&source)
                .map_err(|_| format!("cannot make {} relative to source", path.display()))?
                .to_path_buf();
            let (digest, size) = self.store_file(&path)?;
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            bytes = bytes.saturating_add(size);
            files.push(FileEntry {
                digest,
                size,
                modified: modified_seconds(&metadata),
                mode: metadata.permissions().mode(),
                path: relative,
            });
        }

        let created = unix_seconds()?;
        let id = format!(
            "snap-{}-{}-{}",
            created,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock is before the Unix epoch".to_string())?
                .subsec_nanos(),
            process::id()
        );
        let snapshot = Snapshot {
            created,
            source,
            directories,
            files,
        };
        self.write_snapshot(&id, &snapshot)?;

        Ok(SnapshotSummary {
            id,
            directories: snapshot.directories.len(),
            files: snapshot.files.len(),
            bytes,
            skipped,
        })
    }

    fn list(&self) -> Result<Vec<(String, Snapshot)>> {
        let entries = match fs::read_dir(&self.snapshots) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!("cannot read {}: {error}", self.snapshots.display()));
            }
        };
        let mut snapshots = Vec::new();

        for entry in entries {
            let entry = entry.map_err(|error| format!("cannot read snapshot entry: {error}"))?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new("manifest")) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(OsStr::to_str)
                .ok_or_else(|| format!("invalid snapshot filename {}", path.display()))?
                .to_owned();
            snapshots.push((id, read_manifest(&path)?));
        }

        snapshots.sort_by(|left, right| right.0.cmp(&left.0));
        Ok(snapshots)
    }

    fn read_snapshot(&self, id: &str) -> Result<Snapshot> {
        read_manifest(&self.snapshot_path(id))
    }

    fn verify(&self, id: Option<&str>) -> Result<VerificationSummary> {
        let snapshots = match id {
            Some(id) => vec![(id.to_owned(), self.read_snapshot(id)?)],
            None => self.list()?,
        };
        let mut objects = BTreeSet::new();
        let mut files = 0;
        let mut bytes = 0_u64;

        for (id, snapshot) in &snapshots {
            for entry in &snapshot.files {
                let object = self.objects.join(&entry.digest);
                let Some(metadata) = path_metadata(&object)? else {
                    return Err(format!("snapshot {id} is missing object {}", entry.digest));
                };
                if !metadata.file_type().is_file() {
                    return Err(format!(
                        "snapshot {id} object {} is not a regular file",
                        entry.digest
                    ));
                }
                if metadata.len() != entry.size {
                    return Err(format!(
                        "snapshot {id} object {} has an unexpected size",
                        entry.digest
                    ));
                }
                files += 1;
                bytes = bytes.saturating_add(entry.size);
                objects.insert(entry.digest.clone());
            }
        }

        for digest in &objects {
            let object = self.objects.join(digest);
            if sha256_file(&object)? != *digest {
                return Err(format!("snapshot object is corrupt: {digest}"));
            }
        }

        Ok(VerificationSummary {
            snapshots: snapshots.len(),
            files,
            objects: objects.len(),
            bytes,
        })
    }

    fn restore(&self, id: &str, destination: &Path, force: bool) -> Result<usize> {
        let snapshot = self.read_snapshot(id)?;
        let mut directories = Vec::with_capacity(snapshot.directories.len());
        for entry in &snapshot.directories {
            validate_restore_path(&entry.path)?;
            let target = destination.join(&entry.path);
            if let Some(metadata) = path_metadata(&target)?
                && !metadata.file_type().is_dir()
            {
                return Err(format!("{} is not a directory", target.display()));
            }
            directories.push((entry, target));
        }
        let mut targets = Vec::with_capacity(snapshot.files.len());
        for entry in &snapshot.files {
            validate_restore_path(&entry.path)?;
            let target = destination.join(&entry.path);
            if let Some(metadata) = path_metadata(&target)? {
                if !force {
                    return Err(format!(
                        "{} already exists; rerun with --force to replace regular files",
                        target.display()
                    ));
                }
                if !metadata.file_type().is_file() {
                    return Err(format!(
                        "refusing to replace non-regular file {}",
                        target.display()
                    ));
                }
            }
            targets.push((entry, target));
        }

        for (entry, _) in &targets {
            let object = self.objects.join(&entry.digest);
            let Some(metadata) = path_metadata(&object)? else {
                return Err(format!("snapshot object is missing: {}", entry.digest));
            };
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "snapshot object is not a regular file: {}",
                    entry.digest
                ));
            }
            if sha256_file(&object)? != entry.digest {
                return Err(format!("snapshot object is corrupt: {}", entry.digest));
            }
        }

        ensure_destination_directory(destination)?;

        for (entry, target) in &directories {
            create_restore_parent(destination, &entry.path)?;
            if path_metadata(target)?.is_none() {
                fs::create_dir(target)
                    .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
            }
        }

        for (entry, target) in targets {
            create_restore_parent(destination, &entry.path)?;
            if force && path_metadata(&target)?.is_some() {
                fs::remove_file(&target)
                    .map_err(|error| format!("cannot replace {}: {error}", target.display()))?;
            }

            let object = self.objects.join(&entry.digest);
            let mut source = open_regular_file(&object)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
            io::copy(&mut source, &mut output)
                .map_err(|error| format!("cannot restore {}: {error}", target.display()))?;
            output
                .sync_all()
                .map_err(|error| format!("cannot sync {}: {error}", target.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode)).map_err(
                |error| format!("cannot set permissions on {}: {error}", target.display()),
            )?;
            let modified = UNIX_EPOCH + Duration::from_secs(entry.modified);
            fs::set_times(&target, fs::FileTimes::new().set_modified(modified)).map_err(
                |error| format!("cannot set timestamp on {}: {error}", target.display()),
            )?;
        }

        // Apply directory metadata after writing children so restrictive modes do not block restore.
        directories.sort_by_key(|entry| std::cmp::Reverse(entry.0.path.components().count()));
        for (entry, target) in directories {
            fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode)).map_err(
                |error| format!("cannot set permissions on {}: {error}", target.display()),
            )?;
            let modified = UNIX_EPOCH + Duration::from_secs(entry.modified);
            fs::set_times(&target, fs::FileTimes::new().set_modified(modified)).map_err(
                |error| format!("cannot set timestamp on {}: {error}", target.display()),
            )?;
        }

        Ok(snapshot.files.len())
    }

    fn store_file(&self, path: &Path) -> Result<(String, u64)> {
        let (temporary_path, mut temporary_file) = self.new_temporary_file("object")?;
        let result = (|| {
            let mut source = open_regular_file(path)?;
            let mut hash = Sha256::new();
            let mut bytes = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];

            loop {
                let count = source
                    .read(&mut buffer)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
                temporary_file
                    .write_all(&buffer[..count])
                    .map_err(|error| format!("cannot stage {}: {error}", path.display()))?;
                bytes = bytes.saturating_add(count as u64);
            }
            temporary_file
                .sync_all()
                .map_err(|error| format!("cannot sync staged {}: {error}", path.display()))?;
            drop(temporary_file);

            let digest = hash.finish_hex();
            let object = self.objects.join(&digest);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&object)
            {
                Ok(mut destination) => {
                    let mut staged = File::open(&temporary_path).map_err(|error| {
                        format!("cannot read staged {}: {error}", temporary_path.display())
                    })?;
                    io::copy(&mut staged, &mut destination).map_err(|error| {
                        format!("cannot store object {}: {error}", object.display())
                    })?;
                    destination.sync_all().map_err(|error| {
                        format!("cannot sync object {}: {error}", object.display())
                    })?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "cannot create object {}: {error}",
                        object.display()
                    ));
                }
            }

            Ok((digest, bytes))
        })();

        let _ = fs::remove_file(&temporary_path);
        result
    }

    fn write_snapshot(&self, id: &str, snapshot: &Snapshot) -> Result<()> {
        let manifest = serialize_manifest(snapshot);
        let path = self.snapshot_path(id);
        let (temporary_path, mut temporary_file) = self.new_temporary_file("manifest")?;
        let result = (|| {
            temporary_file
                .write_all(manifest.as_bytes())
                .map_err(|error| format!("cannot stage snapshot manifest: {error}"))?;
            temporary_file
                .sync_all()
                .map_err(|error| format!("cannot sync snapshot manifest: {error}"))?;
            drop(temporary_file);

            if path.exists() {
                return Err(format!("snapshot {id} already exists"));
            }
            fs::rename(&temporary_path, &path)
                .map_err(|error| format!("cannot save snapshot {id}: {error}"))?;
            Ok(())
        })();

        let _ = fs::remove_file(&temporary_path);
        result
    }

    fn new_temporary_file(&self, kind: &str) -> Result<(PathBuf, File)> {
        for _ in 0..100 {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock is before the Unix epoch".to_string())?;
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = self.temporary.join(format!(
                "{kind}-{}-{}-{}-{}",
                process::id(),
                now.as_secs(),
                now.subsec_nanos(),
                sequence
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot create temporary file {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Err("cannot allocate a unique temporary file".into())
    }

    fn snapshot_path(&self, id: &str) -> PathBuf {
        self.snapshots.join(format!("{id}.manifest"))
    }
}

fn default_store_path() -> PathBuf {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path).join("terminal-time-machine")
    } else if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("terminal-time-machine")
    } else {
        PathBuf::from(".terminal-time-machine")
    }
}

fn canonicalize_for_comparison(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| format!("cannot determine the current directory: {error}"))?
            .join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();

    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    format!("cannot resolve snapshot store path {}", path.display())
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    format!("cannot resolve snapshot store path {}", path.display())
                })?;
            }
            Err(error) => return Err(format!("cannot access {}: {error}", path.display())),
        }
    }

    let mut canonical = fs::canonicalize(existing)
        .map_err(|error| format!("cannot access {}: {error}", path.display()))?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn collect_entries(
    source: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
    directories: &mut Vec<DirectoryEntry>,
    skipped: &mut usize,
) -> Result<()> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            *skipped += 1;
        } else if file_type.is_dir() {
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let relative = path
                .strip_prefix(source)
                .map_err(|_| format!("cannot make {} relative to source", path.display()))?
                .to_path_buf();
            directories.push(DirectoryEntry {
                modified: modified_seconds(&metadata),
                mode: metadata.permissions().mode(),
                path: relative,
            });
            collect_entries(source, &path, files, directories, skipped)?;
        } else if file_type.is_file() {
            files.push(path);
        } else {
            *skipped += 1;
        }
    }
    Ok(())
}

fn serialize_manifest(snapshot: &Snapshot) -> String {
    let mut output = format!(
        "version\t2\ncreated\t{}\nsource\t{}\n",
        snapshot.created,
        encode_path(&snapshot.source)
    );
    for entry in &snapshot.directories {
        output.push_str(&format!(
            "dir\t{}\t{}\t{}\n",
            entry.modified,
            entry.mode,
            encode_path(&entry.path)
        ));
    }
    for entry in &snapshot.files {
        output.push_str(&format!(
            "file\t{}\t{}\t{}\t{}\t{}\n",
            entry.digest,
            entry.size,
            entry.modified,
            entry.mode,
            encode_path(&entry.path)
        ));
    }
    output
}

fn read_manifest(path: &Path) -> Result<Snapshot> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse_manifest(&contents)
        .map_err(|error| format!("invalid manifest {}: {error}", path.display()))
}

fn parse_manifest(contents: &str) -> Result<Snapshot> {
    let mut created = None;
    let mut source = None;
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut version = None;

    for (line_number, line) in contents.lines().enumerate() {
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", "1"] if line_number == 0 => version = Some(1),
            ["version", "2"] if line_number == 0 => version = Some(2),
            ["created", value] if created.is_none() => {
                created = Some(value.parse().map_err(|_| "invalid created timestamp")?);
            }
            ["source", value] if source.is_none() => source = Some(decode_path(value)?),
            ["dir", modified, mode, path] if version == Some(2) => {
                let path = decode_path(path)?;
                validate_restore_path(&path)?;
                directories.push(DirectoryEntry {
                    modified: modified
                        .parse()
                        .map_err(|_| "invalid directory modification timestamp")?,
                    mode: mode.parse().map_err(|_| "invalid directory mode")?,
                    path,
                });
            }
            ["file", digest, size, modified, mode, path] => {
                validate_digest(digest)?;
                let path = decode_path(path)?;
                validate_restore_path(&path)?;
                files.push(FileEntry {
                    digest: (*digest).to_owned(),
                    size: size.parse().map_err(|_| "invalid file size")?,
                    modified: modified
                        .parse()
                        .map_err(|_| "invalid modification timestamp")?,
                    mode: mode.parse().map_err(|_| "invalid file mode")?,
                    path,
                });
            }
            _ => {
                return Err(format!(
                    "invalid manifest record at line {}",
                    line_number + 1
                ));
            }
        }
    }

    if version.is_none() {
        return Err("missing manifest version".into());
    }
    validate_snapshot_entries(&directories, &files)?;
    Ok(Snapshot {
        created: created.ok_or_else(|| "missing created timestamp".to_string())?,
        source: source.ok_or_else(|| "missing source directory".to_string())?,
        directories,
        files,
    })
}

fn validate_snapshot_entries(directories: &[DirectoryEntry], files: &[FileEntry]) -> Result<()> {
    let mut directory_paths = BTreeSet::new();
    for entry in directories {
        if !directory_paths.insert(entry.path.as_path()) {
            return Err(format!(
                "duplicate directory path: {}",
                entry.path.display()
            ));
        }
    }

    let mut file_paths = BTreeSet::new();
    for entry in files {
        if !file_paths.insert(entry.path.as_path()) {
            return Err(format!("duplicate file path: {}", entry.path.display()));
        }
        if directory_paths.contains(entry.path.as_path()) {
            return Err(format!(
                "path is both a file and directory: {}",
                entry.path.display()
            ));
        }
    }

    for entry in files {
        let mut parent = entry.path.parent();
        while let Some(path) = parent {
            if file_paths.contains(path) {
                return Err(format!(
                    "file path contains another file: {}",
                    entry.path.display()
                ));
            }
            parent = path.parent();
        }
    }
    for entry in directories {
        let mut parent = entry.path.parent();
        while let Some(path) = parent {
            if file_paths.contains(path) {
                return Err(format!(
                    "directory path contains a file: {}",
                    entry.path.display()
                ));
            }
            parent = path.parent();
        }
    }

    Ok(())
}

fn diff_snapshots(older: &Snapshot, newer: &Snapshot) -> (usize, usize, usize) {
    let older_files: BTreeMap<_, _> = older
        .files
        .iter()
        .map(|entry| (entry.path.as_path(), entry))
        .collect();
    let newer_files: BTreeMap<_, _> = newer
        .files
        .iter()
        .map(|entry| (entry.path.as_path(), entry))
        .collect();
    let mut added = 0;
    let mut changed = 0;
    let mut removed = 0;

    for (path, entry) in &older_files {
        match newer_files.get(path) {
            Some(newer_entry) if *entry == *newer_entry => {}
            Some(_) => {
                changed += 1;
                println!("{} {}", paint("~", "33"), path.display());
            }
            None => {
                removed += 1;
                println!("{} {}", paint("-", "31"), path.display());
            }
        }
    }
    for path in newer_files.keys() {
        if !older_files.contains_key(path) {
            added += 1;
            println!("{} {}", paint("+", "32"), path.display());
        }
    }

    (added, changed, removed)
}

fn parse_restore_args(args: &[OsString]) -> Result<(&str, PathBuf, bool)> {
    if args.len() < 3 {
        return Err("usage: tt restore <snapshot-id> --to <directory> [--force]".into());
    }
    let id = required_id(&args[0])?;
    let mut destination = None;
    let mut force = false;
    let mut index = 1;

    while index < args.len() {
        if args[index] == OsStr::new("--to") {
            index += 1;
            let path = args
                .get(index)
                .ok_or_else(|| "--to needs a destination directory".to_string())?;
            if destination.replace(PathBuf::from(path)).is_some() {
                return Err("--to may only be supplied once".into());
            }
        } else if args[index] == OsStr::new("--force") {
            if force {
                return Err("--force may only be supplied once".into());
            }
            force = true;
        } else {
            return Err(format!(
                "unknown restore option `{}`",
                args[index].to_string_lossy()
            ));
        }
        index += 1;
    }

    Ok((
        id,
        destination.ok_or_else(|| "restore needs --to <directory>".to_string())?,
        force,
    ))
}

fn required_id(value: &OsStr) -> Result<&str> {
    let id = value
        .to_str()
        .ok_or_else(|| "snapshot id must be valid UTF-8".to_string())?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(
            "snapshot id may only contain letters, numbers, hyphens, and underscores".into(),
        );
    }
    Ok(id)
}

fn validate_restore_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!("unsafe path in snapshot: {}", path.display()));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!("unsafe path in snapshot: {}", path.display()));
        }
    }
    Ok(())
}

fn path_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn open_regular_file(path: &Path) -> Result<File> {
    let Some(metadata) = path_metadata(path)? else {
        return Err(format!(
            "cannot read {}: file does not exist",
            path.display()
        ));
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "cannot read {}: not a regular file",
            path.display()
        ));
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn ensure_destination_directory(destination: &Path) -> Result<()> {
    if path_metadata(destination)?.is_none() {
        fs::create_dir_all(destination)
            .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
    }

    match path_metadata(destination)? {
        Some(metadata) if metadata.file_type().is_dir() => Ok(()),
        Some(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "destination {} must not be a symlink",
            destination.display()
        )),
        Some(_) => Err(format!("{} is not a directory", destination.display())),
        None => Err(format!("cannot create {}", destination.display())),
    }
}

fn create_restore_parent(destination: &Path, relative: &Path) -> Result<()> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = destination.to_path_buf();

    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(format!("unsafe path in snapshot: {}", relative.display()));
        };
        current.push(name);

        if path_metadata(&current)?.is_none() {
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!("cannot create {}: {error}", current.display()));
                }
            }
        }
        match path_metadata(&current)? {
            Some(metadata) if metadata.file_type().is_dir() => {}
            Some(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to restore through symlinked directory {}",
                    current.display()
                ));
            }
            Some(_) => return Err(format!("{} is not a directory", current.display())),
            None => return Err(format!("cannot create {}", current.display())),
        }
    }

    Ok(())
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid SHA-256 digest".into());
    }
    Ok(())
}

fn encode_path(path: &Path) -> String {
    let mut output = String::with_capacity(path.as_os_str().as_bytes().len() * 2);
    for byte in path.as_os_str().as_bytes() {
        output.push(hex_digit(byte >> 4));
        output.push(hex_digit(byte & 0x0f));
    }
    output
}

fn decode_path(value: &str) -> Result<PathBuf> {
    if !value.len().is_multiple_of(2) {
        return Err("path has an odd number of hexadecimal characters".into());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let source = value.as_bytes();
    for index in (0..source.len()).step_by(2) {
        let high = from_hex_digit(source[index])
            .ok_or_else(|| "path contains invalid hexadecimal".to_string())?;
        let low = from_hex_digit(source[index + 1])
            .ok_or_else(|| "path contains invalid hexadecimal".to_string())?;
        let byte = (high << 4) | low;
        if byte == 0 {
            return Err("path contains a null byte".into());
        }
        bytes.push(byte);
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn from_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + value - 10) as char,
        _ => unreachable!("a hexadecimal digit must be between 0 and 15"),
    }
}

fn modified_seconds(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

fn unix_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before the Unix epoch".into())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finish_hex())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn paint(text: &str, code: &str) -> String {
    if io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn print_usage() {
    println!(
        "Terminal Time Machine\n\n\
Usage:\n\
  tt --version\n\
  tt [--store <directory>] snapshot <directory>\n\
  tt [--store <directory>] list\n\
  tt [--store <directory>] show <snapshot-id>\n\
  tt [--store <directory>] diff <older-snapshot-id> <newer-snapshot-id>\n\
  tt [--store <directory>] verify [snapshot-id]\n\
  tt [--store <directory>] restore <snapshot-id> --to <directory> [--force]\n\n\
Snapshots are stored in $XDG_DATA_HOME/terminal-time-machine, or\n\
$HOME/.local/share/terminal-time-machine when XDG_DATA_HOME is unset."
    );
}

#[derive(Clone)]
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.length = self.length.wrapping_add(input.len() as u64);

        if self.buffer_len > 0 {
            let count = (64 - self.buffer_len).min(input.len());
            self.buffer[self.buffer_len..self.buffer_len + count].copy_from_slice(&input[..count]);
            self.buffer_len += count;
            input = &input[count..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.transform(&block);
                self.buffer_len = 0;
            }
        }

        while input.len() >= 64 {
            let (block, rest) = input.split_at(64);
            self.transform(block);
            input = rest;
        }

        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffer_len = input.len();
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_length = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffer_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_length.to_be_bytes());

        let mut output = String::with_capacity(64);
        for value in self.state {
            for byte in value.to_be_bytes() {
                output.push(hex_digit(byte >> 4));
                output.push(hex_digit(byte & 0x0f));
            }
        }
        output
    }

    fn transform(&mut self, block: &[u8]) {
        const CONSTANTS: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];

        let mut schedule = [0_u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (index, chunk) in words.iter().take(16).enumerate() {
            schedule[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let small_sigma0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let small_sigma1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(small_sigma0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(small_sigma1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let big_sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(big_sigma1)
                .wrapping_add(choose)
                .wrapping_add(CONSTANTS[index])
                .wrapping_add(schedule[index]);
            let big_sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = big_sigma0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn test_directory(name: &str) -> PathBuf {
        let unique = format!(
            "terminal-time-machine-{name}-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after Unix epoch")
                .as_nanos()
        );
        env::temp_dir().join(unique)
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let mut empty = Sha256::new();
        empty.update(b"");
        assert_eq!(
            empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            abc.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn paths_round_trip_without_text_assumptions() {
        let original = PathBuf::from(OsString::from_vec(b"notes/line\nwith\ttabs".to_vec()));
        assert_eq!(decode_path(&encode_path(&original)).unwrap(), original);
    }

    #[test]
    fn snapshot_and_restore_preserve_file_data() {
        let root = test_directory("snapshot-restore");
        let source = root.join("source");
        let restored = root.join("restored");
        let store_path = root.join("store");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(source.join("empty/deeper")).unwrap();
        fs::set_permissions(source.join("empty"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(source.join("nested/note.txt"), b"original version").unwrap();
        fs::write(source.join("readme.txt"), b"snapshot me").unwrap();

        let store = Store::open(Some(store_path));
        let summary = store.snapshot(&source).unwrap();
        fs::write(source.join("nested/note.txt"), b"changed version").unwrap();
        assert_eq!(store.restore(&summary.id, &restored, false).unwrap(), 2);
        assert_eq!(
            fs::read(restored.join("nested/note.txt")).unwrap(),
            b"original version"
        );
        assert_eq!(
            fs::read(restored.join("readme.txt")).unwrap(),
            b"snapshot me"
        );
        assert!(restored.join("empty/deeper").is_dir());
        assert_eq!(
            fs::metadata(restored.join("empty"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_only_commands_do_not_create_a_store() {
        let root = test_directory("read-only-store");
        let store_path = root.join("store");
        fs::create_dir_all(&root).unwrap();

        let store = Store::open(Some(store_path.clone()));
        assert!(store.list().unwrap().is_empty());
        assert_eq!(store.verify(None).unwrap().snapshots, 0);
        assert!(!store_path.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_rejects_a_new_store_inside_the_source() {
        let root = test_directory("store-inside-source");
        let source = root.join("source");
        let store_path = source.join(".tt");
        fs::create_dir_all(&source).unwrap();

        let store = Store::open(Some(store_path.clone()));
        assert!(
            store
                .snapshot(&source)
                .unwrap_err()
                .contains("inside the source")
        );
        assert!(!store_path.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_reports_deduplicated_objects() {
        let root = test_directory("verify");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("first.txt"), b"same content").unwrap();
        fs::write(source.join("second.txt"), b"same content").unwrap();

        let store = Store::open(Some(root.join("store")));
        let snapshot = store.snapshot(&source).unwrap();
        let summary = store.verify(Some(&snapshot.id)).unwrap();
        assert_eq!(summary.snapshots, 1);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.objects, 1);
        assert_eq!(summary.bytes, 24);

        let digest = store.read_snapshot(&snapshot.id).unwrap().files[0]
            .digest
            .clone();
        fs::write(store.objects.join(digest), b"other things").unwrap();
        assert!(
            store
                .verify(Some(&snapshot.id))
                .unwrap_err()
                .contains("corrupt")
        );

        let digest = store.read_snapshot(&snapshot.id).unwrap().files[0]
            .digest
            .clone();
        let object = store.objects.join(&digest);
        fs::remove_file(&object).unwrap();
        symlink(source.join("first.txt"), &object).unwrap();
        assert!(
            store
                .verify(Some(&snapshot.id))
                .unwrap_err()
                .contains("not a regular file")
        );
        assert!(
            store
                .restore(&snapshot.id, &root.join("restored"), false)
                .is_err()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_one_manifests_remain_readable() {
        let manifest = format!(
            "version\t1\ncreated\t1\nsource\t2f736f75726365\nfile\t{}\t0\t0\t33188\t6e6f74652e747874\n",
            "a".repeat(64)
        );

        let snapshot = parse_manifest(&manifest).unwrap();
        assert!(snapshot.directories.is_empty());
        assert_eq!(snapshot.files[0].path, PathBuf::from("note.txt"));
    }

    #[test]
    fn manifests_reject_file_path_conflicts() {
        let manifest = format!(
            "version\t2\ncreated\t1\nsource\t2f736f75726365\nfile\t{}\t0\t0\t33188\t6e6f6465\nfile\t{}\t0\t0\t33188\t6e6f64652f6368696c64\n",
            "a".repeat(64),
            "b".repeat(64)
        );

        assert!(
            parse_manifest(&manifest)
                .unwrap_err()
                .contains("contains another file")
        );
    }

    #[test]
    fn diff_counts_added_changed_and_removed_files() {
        let digest_a = "a".repeat(64);
        let digest_b = "b".repeat(64);
        let entry = |path: &str, digest: String| FileEntry {
            digest,
            size: 1,
            modified: 0,
            mode: 0o644,
            path: PathBuf::from(path),
        };
        let older = Snapshot {
            created: 1,
            source: PathBuf::from("/source"),
            directories: Vec::new(),
            files: vec![
                entry("changed.txt", digest_a.clone()),
                entry("removed.txt", digest_a),
            ],
        };
        let newer = Snapshot {
            created: 2,
            source: PathBuf::from("/source"),
            directories: Vec::new(),
            files: vec![
                entry("added.txt", digest_b.clone()),
                entry("changed.txt", digest_b),
            ],
        };

        assert_eq!(diff_snapshots(&older, &newer), (1, 1, 1));
    }

    #[test]
    fn restore_rejects_parent_directory_paths() {
        assert!(validate_restore_path(Path::new("../outside")).is_err());
        assert!(validate_restore_path(Path::new("/absolute")).is_err());
        assert!(validate_restore_path(Path::new("safe/file")).is_ok());
    }

    #[test]
    fn restore_rejects_symlinked_directories() {
        let root = test_directory("symlink-restore");
        let source = root.join("source");
        let destination = root.join("destination");
        let outside = root.join("outside");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(source.join("nested/note.txt"), b"keep this contained").unwrap();
        symlink(&outside, destination.join("nested")).unwrap();

        let store = Store::open(Some(root.join("store")));
        let summary = store.snapshot(&source).unwrap();
        assert!(store.restore(&summary.id, &destination, false).is_err());
        assert!(!outside.join("note.txt").exists());

        fs::remove_dir_all(root).unwrap();
    }
}
