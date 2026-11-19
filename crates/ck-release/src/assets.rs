//! Checks a payload against the downloaded release files before it is
//! signed: every artifact must match a real file, byte for byte.

use std::fs;
use std::path::Path;

use ck_core::catalog::schema::Platform;

use crate::payload::{self, measure};
use crate::{Error, Result, asset_file_name};

/// Checks every artifact in the payload file `payload` (read with
/// [`payload::read`]) against `<releases>/<name>-<version>/<file>`, the
/// layout `payload build` reads: the file must exist and have the artifact's
/// size and SHA256. Returns how many artifacts matched.
///
/// `<file>` is the last segment of the artifact's URL, which must be the
/// archive name `payload build` gives it ([`asset_file_name`]), so a URL can
/// never point the check outside its release folder.
pub fn verify(payload: &Path, releases: &Path) -> Result<usize> {
    let (_, payload) = payload::read(payload)?;
    let mut count = 0;
    for runtime in &payload.runtimes {
        let folder = releases.join(format!("{}-{}", runtime.name, runtime.version));
        for artifact in &runtime.artifacts {
            let platform = Platform {
                os: artifact.os,
                arch: artifact.arch,
            };
            let what = format!("{} {} for {platform}", runtime.name, runtime.version);
            let file = asset_file_name(&runtime.name, &runtime.version, platform);
            if artifact.url.rsplit('/').next() != Some(file.as_str()) {
                return Err(Error::Invalid(format!(
                    "{what}: the URL {:?} does not end in /{file}",
                    artifact.url
                )));
            }
            let path = folder.join(&file);
            let metadata = fs::symlink_metadata(&path).map_err(|e| Error::io("read", &path, e))?;
            if !metadata.is_file() {
                return Err(Error::Invalid(format!(
                    "{} is not a regular file (folders and links are refused)",
                    path.display()
                )));
            }
            let (sha256, size) = measure(&path)?;
            if size != artifact.size || sha256 != artifact.sha256 {
                return Err(Error::Invalid(format!(
                    "{what}: {} has {size} bytes and SHA256 {sha256}, but the payload says \
                     {} bytes and SHA256 {}",
                    path.display(),
                    artifact.size,
                    artifact.sha256
                )));
            }
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::tests::{PREFIX, flip_first_byte, good_releases, release, spec};
    use crate::test_support::TempDir;

    // T3
    #[test]
    fn every_artifact_must_match_its_release_file_byte_for_byte() {
        let temp = TempDir::new();
        let releases = good_releases(&temp.0);
        let built = payload::build(&releases, &spec(PREFIX), &release()).unwrap();
        let path = temp.0.join("payload-v1.json");
        payload::write(&built, &path).unwrap();
        assert_eq!(verify(&path, &releases).unwrap(), 3);

        let archive = releases
            .join("php-8.5.12-ck1")
            .join("php-8.5.12-ck1-macos-aarch64.tar.gz");
        flip_first_byte(&archive);
        let result = verify(&path, &releases);
        let reason = "but the payload says";
        assert!(
            matches!(&result, Err(Error::Invalid(message)) if message.contains(reason)),
            "{result:?}"
        );

        fs::remove_file(&archive).unwrap();
        let result = verify(&path, &releases);
        assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
    }
}
