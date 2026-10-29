use crate::auth_types::{AuthError, CodexSource, Credentials, Provider};
use rand::random;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    },
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
};
use zeroize::{Zeroize, Zeroizing};

pub struct Vault {
    root: PathBuf,
}
#[derive(serde::Deserialize, serde::Serialize)]
struct Preferences {
    codex_source: CodexSource,
}
struct ProtectedBuffer(CRYPT_INTEGER_BLOB);
impl ProtectedBuffer {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
    }
}
impl Drop for ProtectedBuffer {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            unsafe {
                std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                LocalFree(self.0.pbData.cast());
            }
        }
    }
}
fn crypt(bytes: &mut [u8], decrypt: bool) -> Result<ProtectedBuffer, AuthError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len())
            .map_err(|_| AuthError::new("Credential data is too large"))?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = ProtectedBuffer(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    });
    // User-scoped DPAPI. Never CRYPTPROTECT_LOCAL_MACHINE; never permit a credential UI prompt.
    let ok = unsafe {
        if decrypt {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        } else {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
    };
    if ok == 0 || output.0.pbData.is_null() {
        return Err(AuthError::new(
            "Windows DPAPI cannot protect or decrypt this application's credentials",
        ));
    }
    Ok(output)
}
impl Vault {
    pub fn open() -> Result<Self, AuthError> {
        let root = match env::var_os("QUOTA_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                env::var_os("LOCALAPPDATA")
                    .ok_or_else(|| AuthError::new("LOCALAPPDATA is not available"))?,
            )
            .join("LLMQuotaMonitor"),
        };
        Self::at(root)
    }
    fn at(root: PathBuf) -> Result<Self, AuthError> {
        fs::create_dir_all(root.join("auth"))
            .map_err(|_| AuthError::new("Cannot create the application's credential directory"))?;
        Ok(Self { root })
    }
    pub fn acquire_instance(&self) -> Result<File, AuthError> {
        OpenOptions::new().create(true).truncate(false).read(true).write(true).share_mode(0).open(self.root.join("instance.lock"))
            .map_err(|_|AuthError::new("Quota Monitor is already running for this data directory; close its tray icon or headless instance first"))
    }
    fn path(&self, provider: Provider) -> PathBuf {
        self.root
            .join("auth")
            .join(format!("{}.dpapi", provider.key()))
    }
    pub fn load(&self, provider: Provider) -> Result<Option<Credentials>, AuthError> {
        let path = self.path(provider);
        let metadata = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AuthError::new("Cannot inspect saved authorization")),
        };
        if metadata.len() > 64 * 1024 {
            return Err(AuthError::new("Saved authorization file is too large"));
        }
        let mut encrypted = Zeroizing::new(
            fs::read(path).map_err(|_| AuthError::new("Cannot read saved authorization"))?,
        );
        let plain = crypt(&mut encrypted, true)?;
        let credentials: Credentials = serde_json::from_slice(plain.bytes())
            .map_err(|_| AuthError::new("Saved authorization is invalid"))?;
        if credentials.access_token.is_empty()
            || credentials.refresh_token.is_empty()
            || credentials.expires_at <= 0
        {
            return Err(AuthError::new("Saved authorization has invalid fields"));
        }
        Ok(Some(credentials))
    }
    pub fn save(&self, provider: Provider, credentials: &Credentials) -> Result<(), AuthError> {
        let mut plain = Zeroizing::new(
            serde_json::to_vec(credentials)
                .map_err(|_| AuthError::new("Cannot encode authorization"))?,
        );
        if plain.len() > 60 * 1024 {
            return Err(AuthError::new("Authorization is too large to store"));
        }
        let protected = crypt(&mut plain, false)?;
        self.commit(&self.path(provider), protected.bytes())
    }
    pub fn codex_source(&self) -> Result<CodexSource, AuthError> {
        let path = self.root.join("settings.json");
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CodexSource::default())
            }
            Err(_) => {
                return Err(AuthError::new(
                    "Cannot inspect the saved Codex source setting",
                ))
            }
        };
        if metadata.len() > 4096 {
            return Err(AuthError::new("Saved Codex source setting is too large"));
        }
        let bytes = fs::read(path)
            .map_err(|_| AuthError::new("Cannot read the saved Codex source setting"))?;
        let settings: Preferences = serde_json::from_slice(&bytes)
            .map_err(|_| AuthError::new("Saved Codex source setting is invalid"))?;
        Ok(settings.codex_source)
    }
    pub fn save_codex_source(&self, source: CodexSource) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(&Preferences {
            codex_source: source,
        })
        .map_err(|_| AuthError::new("Cannot encode the Codex source setting"))?;
        self.commit(&self.root.join("settings.json"), &bytes)
    }
    fn commit(&self, destination: &Path, bytes: &[u8]) -> Result<(), AuthError> {
        let temporary = destination.with_extension(format!("{:016x}.tmp", random::<u64>()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|_| AuthError::new("Cannot create the application's data file"))?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| AuthError::new("Cannot write the application's data file"))?;
            drop(file);
            let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let to: Vec<u16> = destination
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            if unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(AuthError::new(
                    "Cannot commit application data; token refresh and source switching are paused",
                ));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
    pub fn remove(&self, provider: Provider) -> Result<(), AuthError> {
        match fs::remove_file(self.path(provider)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AuthError::new("Cannot remove saved authorization")),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpapi_roundtrip_rotation_tamper_and_instance_exclusion() {
        let root = env::temp_dir().join(format!("quota-vault-test-{:016x}", random::<u64>()));
        let vault = Vault::at(root.clone()).unwrap();
        let lock = vault.acquire_instance().unwrap();
        assert!(vault.acquire_instance().is_err());
        assert_eq!(vault.codex_source().unwrap(), CodexSource::CodexLb);
        vault.save_codex_source(CodexSource::OAuth).unwrap();
        assert_eq!(
            Vault::at(root.clone()).unwrap().codex_source().unwrap(),
            CodexSource::OAuth
        );
        vault.save_codex_source(CodexSource::CodexLb).unwrap();
        assert_eq!(vault.codex_source().unwrap(), CodexSource::CodexLb);
        assert!(vault.load(Provider::Claude).unwrap().is_none());
        let credentials = Credentials {
            access_token: "access-never-plaintext".into(),
            refresh_token: "refresh-one".into(),
            expires_at: 12345,
            device_id: None,
        };
        vault.save(Provider::Claude, &credentials).unwrap();
        let encrypted = fs::read(vault.path(Provider::Claude)).unwrap();
        assert!(!encrypted
            .windows(credentials.access_token.len())
            .any(|part| part == credentials.access_token.as_bytes()));
        assert_eq!(
            vault.load(Provider::Claude).unwrap().unwrap().access_token,
            credentials.access_token
        );
        let rotated = Credentials {
            access_token: "new-access".into(),
            refresh_token: "refresh-two".into(),
            expires_at: 23456,
            device_id: None,
        };
        vault.save(Provider::Claude, &rotated).unwrap();
        assert_eq!(
            vault.load(Provider::Claude).unwrap().unwrap().refresh_token,
            "refresh-two"
        );
        let mut damaged = fs::read(vault.path(Provider::Claude)).unwrap();
        let last = damaged.len() - 1;
        damaged[last] ^= 0x80;
        fs::write(vault.path(Provider::Claude), damaged).unwrap();
        assert!(vault.load(Provider::Claude).is_err());
        vault.remove(Provider::Claude).unwrap();
        assert!(vault.load(Provider::Claude).unwrap().is_none());
        drop(lock);
        drop(vault.acquire_instance().unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
