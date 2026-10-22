//! Secrets kept in files instead of the OS keyring, for a server that has
//! none (`zapfast serve --secrets-dir <dir>`).
//!
//! Only an explicit switch turns this on: `WHATSPRO_SECRETS_DIR`, which
//! `serve` sets from its flag so the `mcp` and `transcribe` processes it
//! starts inherit it. Without it every secret stays in the OS keyring, and on
//! Linux with no D-Bus session to reach one the keyring fails with a message
//! naming the flag rather than falling back to a throwaway archive.
//!
//! The directory is the user's own (0700) and each secret one file in it
//! (0600): the archive keys as `archive-<digest>.key`, 32 raw bytes, and the
//! AI providers' keys as `ai-<account>`. It plugs in as a keyring store, so
//! the archive's rules (create a key only for a missing or plaintext
//! archive, never replace one an archive depends on, read back before use)
//! are the keyring path's own, unchanged.

use std::{
    any::Any,
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use keyring_core::{
    Entry, Error,
    api::{Credential, CredentialApi, CredentialStoreApi},
};
use zeroize::Zeroizing;

/// The directory of the secrets, when they live in files.
pub const DIR_VAR: &str = "WHATSPRO_SECRETS_DIR";

/// The archive key as 64 hexadecimal digits, instead of its key file. Read
/// only together with [`DIR_VAR`].
pub const ARCHIVE_KEY_VAR: &str = "WHATSPRO_ARCHIVE_KEY";

/// The keyring service of the archive keys (src/archive/encryption.rs).
const ARCHIVE_SERVICE: &str = "rocks.zapfast.ZapFast";

/// The keyring service of the AI providers' keys (src/keys.rs).
const AI_SERVICE: &str = "com.julitomaker.whatspro.ai";

/// What to do when nothing can hold a secret.
const NO_KEYRING: &str = "There is no OS keyring here (no D-Bus session). On a server, start \
     Whats Pro with `zapfast serve --secrets-dir <dir>` (or set WHATSPRO_SECRETS_DIR) to keep \
     its keys in files instead";

/// The secrets directory, when [`DIR_VAR`] names one.
pub fn file_dir() -> Option<PathBuf> {
    std::env::var_os(DIR_VAR)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// The file store when [`DIR_VAR`] is set; otherwise `None`, once it is
/// clear an OS keyring could answer at all.
pub(crate) fn file_store() -> Result<Option<Arc<FileStore>>> {
    match file_dir() {
        Some(dir) => Ok(Some(Arc::new(FileStore::open(&dir)?))),
        None => {
            ensure!(keyring_reachable(), NO_KEYRING);
            Ok(None)
        }
    }
}

/// Whether the platform keyring could be reached. On Linux it is the Secret
/// Service on the session bus, which zbus finds through
/// `DBUS_SESSION_BUS_ADDRESS` or `$XDG_RUNTIME_DIR/bus`.
fn keyring_reachable() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|address| !address.is_empty())
            || std::env::var_os("XDG_RUNTIME_DIR")
                .is_some_and(|runtime| Path::new(&runtime).join("bus").exists())
    }
    #[cfg(not(target_os = "linux"))]
    true
}

/// The archive key [`ARCHIVE_KEY_VAR`] gives, with file secrets on.
pub(crate) fn archive_key_override() -> Result<Option<Zeroizing<[u8; 32]>>> {
    if file_dir().is_none() {
        return Ok(None);
    }
    let Some(value) = std::env::var_os(ARCHIVE_KEY_VAR) else {
        return Ok(None);
    };
    let Ok(value) = value.into_string().map(Zeroizing::new) else {
        bail!("{ARCHIVE_KEY_VAR} must be 64 hexadecimal digits");
    };
    hex_key(value.trim()).map(Some)
}

fn hex_key(text: &str) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(
        text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{ARCHIVE_KEY_VAR} must be 64 hexadecimal digits"
    );
    let mut key = Zeroizing::new([0; 32]);
    for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks(2)) {
        let digits = std::str::from_utf8(pair).expect("ASCII hex digits");
        *byte = u8::from_str_radix(digits, 16).expect("checked hex digits");
    }
    Ok(key)
}

/// A keyring store whose credentials are files in one private directory.
#[derive(Debug)]
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    /// The store in `dir`, made (0700) when missing. A directory others can
    /// read is narrowed to its owner first.
    pub fn open(dir: &Path) -> Result<Self> {
        let dir = std::path::absolute(dir)
            .with_context(|| format!("Could not resolve the secrets folder {}", dir.display()))?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .with_context(|| format!("Could not create the secrets folder {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&dir)?;
            if metadata.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).with_context(
                    || {
                        format!(
                            "Could not make the secrets folder {} private",
                            dir.display()
                        )
                    },
                )?;
            }
        }
        Ok(Self { dir })
    }

    /// The file holding `user`'s secret for `service`.
    fn path(&self, service: &str, user: &str) -> PathBuf {
        let name = match service {
            // Already a fixed prefix and a hex digest.
            ARCHIVE_SERVICE => format!("{}.key", file_name(user)),
            AI_SERVICE => format!("ai-{}", file_name(user)),
            _ => format!("{}-{}", file_name(service), file_name(user)),
        };
        self.dir.join(name)
    }
}

/// `text` as one safe file name: letters, digits, `-`, `_` and `.` stay,
/// every other byte (and a leading dot) becomes `%XX`, so distinct names
/// never share a file.
fn file_name(text: &str) -> String {
    let mut name = String::with_capacity(text.len());
    for (index, byte) in text.bytes().enumerate() {
        let plain = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_')
            || (byte == b'.' && index > 0);
        if plain {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    name
}

impl CredentialStoreApi for FileStore {
    fn vendor(&self) -> String {
        "Whats Pro secrets folder".into()
    }

    fn id(&self) -> String {
        self.dir.display().to_string()
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> keyring_core::Result<Entry> {
        if modifiers.is_some_and(|modifiers| !modifiers.is_empty()) {
            return Err(Error::NotSupportedByStore(
                "The secrets folder takes no entry modifiers".into(),
            ));
        }
        Ok(Entry::new_with_credential(Arc::new(FileCredential {
            path: self.path(service, user),
            service: service.to_owned(),
            user: user.to_owned(),
        })))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// One secret's file.
#[derive(Debug)]
struct FileCredential {
    path: PathBuf,
    service: String,
    user: String,
}

impl FileCredential {
    fn failure(&self, doing: &str, error: io::Error) -> Error {
        Error::PlatformFailure(format!("{doing} {}: {error}", self.path.display()).into())
    }

    fn write(&self, secret: &[u8]) -> io::Result<()> {
        // Written whole beside it and renamed over it, so a crash leaves the
        // old secret or the new one, never half of either.
        let mut staging = self.path.clone().into_os_string();
        staging.push(".saving");
        let staging = PathBuf::from(staging);
        match fs::remove_file(&staging) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staging)?;
        file.write_all(secret)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, &self.path)?;
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

impl CredentialApi for FileCredential {
    fn set_secret(&self, secret: &[u8]) -> keyring_core::Result<()> {
        self.write(secret)
            .map_err(|error| self.failure("Could not save", error))
    }

    fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
        match fs::read(&self.path) {
            Ok(secret) => Ok(secret),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(Error::NoEntry),
            Err(error) => Err(self.failure("Could not read", error)),
        }
    }

    fn delete_credential(&self) -> keyring_core::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(Error::NoEntry),
            Err(error) => Err(self.failure("Could not delete", error)),
        }
    }

    fn get_credential(&self) -> keyring_core::Result<Option<Arc<Credential>>> {
        match self.path.try_exists() {
            Ok(true) => Ok(None),
            Ok(false) => Err(Error::NoEntry),
            Err(error) => Err(self.failure("Could not read", error)),
        }
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_private_files_named_after_their_entry() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("secrets");
        let store = FileStore::open(&dir).unwrap();
        let archive = store.build(ARCHIVE_SERVICE, "archive-00ff", None).unwrap();
        let ai = store
            .build(AI_SERVICE, "ElevaLeads API:shop", None)
            .unwrap();

        assert!(matches!(archive.get_secret(), Err(Error::NoEntry)));
        assert!(matches!(archive.delete_credential(), Err(Error::NoEntry)));
        archive.set_secret(&[7; 32]).unwrap();
        ai.set_password("sk-fixture").unwrap();
        assert_eq!(archive.get_secret().unwrap(), vec![7; 32]);
        assert_eq!(ai.get_password().unwrap(), "sk-fixture");
        assert_eq!(
            fs::read(dir.join("archive-00ff.key")).unwrap(),
            vec![7; 32],
            "the archive key is its 32 raw bytes"
        );
        assert!(dir.join("ai-ElevaLeads%20API%3Ashop").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("archive-00ff.key")), 0o600);
        }
        let names: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(names.len(), 2, "no staging file is left behind");

        archive.set_secret(&[8; 32]).unwrap();
        assert_eq!(archive.get_secret().unwrap(), vec![8; 32]);
        ai.delete_credential().unwrap();
        assert!(matches!(ai.get_secret(), Err(Error::NoEntry)));
    }

    #[cfg(unix)]
    #[test]
    fn an_open_secrets_folder_is_narrowed_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        FileStore::open(directory.path()).unwrap();
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn file_names_cannot_escape_or_collide() {
        assert_eq!(file_name("Gemini"), "Gemini");
        assert_eq!(file_name("../x"), "%2E.%2Fx");
        assert_ne!(file_name("a:b"), file_name("a_b"));
        assert_ne!(file_name("a%3Ab"), file_name("a:b"));
    }

    #[test]
    fn an_archive_key_override_is_64_hex_digits() {
        let key = hex_key(&"0a".repeat(32)).unwrap();
        assert_eq!(*key, [10; 32]);
        for bad in ["", "0a", &"zz".repeat(32), &"0a".repeat(33)] {
            assert!(hex_key(bad).is_err());
        }
    }
}
