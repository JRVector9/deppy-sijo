//! Verify bundled ELF helpers before a generation-scoped, consented install.
use sha2::{Digest, Sha256};
use ssh_transport::{HelperDigest, HelperTarget, HelperVersion, RemoteCommand};
use std::path::Path;

const HELPER_BYTES_MAX: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundledHelperManifest {
    pub version: String,
    /// `linux-aarch64` or `linux-x86_64`; filenames are fixed by this target.
    pub target: String,
    pub sha256: String,
    pub bytes: u64,
}

pub(crate) struct VerifiedHelperArtifact {
    bytes: Vec<u8>,
    version: HelperVersion,
    digest: HelperDigest,
    target: HelperTarget,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HelperInstallConsent {
    pub connection_generation: u64,
}

pub(crate) struct HelperInstallPlan<'a> {
    pub remote_command: RemoteCommand,
    pub bytes: &'a [u8],
    pub ready_marker: String,
}

impl VerifiedHelperArtifact {
    pub fn verify(
        bytes: Vec<u8>,
        manifest: &BundledHelperManifest,
        expected_version: &str,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            manifest.version == expected_version,
            "ssh_helper_version_mismatch"
        );
        let version = HelperVersion::parse(&manifest.version)?;
        let digest = HelperDigest::parse(&manifest.sha256)?;
        anyhow::ensure!(
            bytes.len() >= 64
                && bytes.len() <= HELPER_BYTES_MAX
                && bytes.len() as u64 == manifest.bytes,
            "ssh_helper_size_mismatch"
        );
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        anyhow::ensure!(actual == manifest.sha256, "ssh_helper_digest_mismatch");
        let (target, machine) = match manifest.target.as_str() {
            "linux-aarch64" => (HelperTarget::LinuxArm64, 183u16),
            "linux-x86_64" => (HelperTarget::LinuxX86_64, 62u16),
            _ => anyhow::bail!("ssh_helper_target_unsupported"),
        };
        anyhow::ensure!(
            &bytes[..7] == b"\x7fELF\x02\x01\x01"
                && matches!(bytes[7], 0 | 3)
                && matches!(u16::from_le_bytes([bytes[16], bytes[17]]), 2 | 3)
                && u16::from_le_bytes([bytes[18], bytes[19]]) == machine,
            "ssh_helper_elf_architecture_mismatch"
        );
        Ok(Self {
            bytes,
            version,
            digest,
            target,
        })
    }

    /// Resources are copied into and covered by the macOS app bundle signature.
    /// The manifest is bounded data; it cannot choose an executable path.
    pub fn load(
        resources: &Path,
        target: HelperTarget,
        expected_version: &str,
    ) -> anyhow::Result<Self> {
        use std::io::Read;
        let root = resources.join("remote-host");
        anyhow::ensure!(
            std::fs::symlink_metadata(&root)?.is_dir(),
            "ssh_helper_directory_not_bundled"
        );
        let manifest_path = root.join("manifest.json");
        anyhow::ensure!(
            std::fs::symlink_metadata(&manifest_path)?.is_file(),
            "ssh_helper_manifest_not_file"
        );
        let mut encoded = Vec::new();
        std::fs::File::open(&manifest_path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut encoded)?;
        anyhow::ensure!(encoded.len() <= 64 * 1024, "ssh_helper_manifest_too_large");
        let entries: Vec<BundledHelperManifest> = serde_json::from_slice(&encoded)?;
        anyhow::ensure!(entries.len() <= 2, "ssh_helper_manifest_invalid");
        let label = match target {
            HelperTarget::LinuxArm64 => "linux-aarch64",
            HelperTarget::LinuxX86_64 => "linux-x86_64",
        };
        let mut selected = entries.iter().filter(|entry| entry.target == label);
        let manifest = selected
            .next()
            .ok_or_else(|| anyhow::anyhow!("ssh_helper_not_bundled"))?;
        anyhow::ensure!(selected.next().is_none(), "ssh_helper_manifest_duplicate");
        let path = root.join(label).join("deppy-remote-host");
        anyhow::ensure!(
            std::fs::symlink_metadata(root.join(label))?.is_dir(),
            "ssh_helper_directory_not_bundled"
        );
        anyhow::ensure!(
            std::fs::symlink_metadata(&path)?.is_file()
                && path.canonicalize()?.starts_with(root.canonicalize()?),
            "ssh_helper_outside_bundle"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(HELPER_BYTES_MAX as u64 + 1)
            .read_to_end(&mut bytes)?;
        Self::verify(bytes, manifest, expected_version)
    }
}

pub(crate) fn prepare_helper_install<'a>(
    artifact: &'a VerifiedHelperArtifact,
    consent: Option<HelperInstallConsent>,
    generation: u64,
) -> anyhow::Result<HelperInstallPlan<'a>> {
    anyhow::ensure!(
        generation > 0
            && consent.is_some_and(|consent| consent.connection_generation == generation),
        "ssh_helper_install_requires_current_consent"
    );
    Ok(HelperInstallPlan {
        remote_command: RemoteCommand::InstallHelper {
            version: artifact.version.clone(),
            digest: artifact.digest.clone(),
            target: artifact.target,
        },
        bytes: &artifact.bytes,
        ready_marker: format!("DEPPY_HELPER_READY {}", artifact.version.as_str()),
    })
}

#[derive(Debug)]
pub(crate) struct ServerProbe {
    pub target: HelperTarget,
    pub account: String,
    pub home: String,
}

pub(crate) fn parse_server_probe(bytes: &[u8]) -> anyhow::Result<ServerProbe> {
    anyhow::ensure!(bytes.len() <= 16 * 1024, "ssh_probe_too_large");
    let text = std::str::from_utf8(bytes)?;
    let lines: Vec<_> = text.lines().collect();
    anyhow::ensure!(
        lines.len() == 4 && lines[0] == "Linux",
        "ssh_probe_platform_unsupported"
    );
    let target = match lines[1] {
        "aarch64" => HelperTarget::LinuxArm64,
        "x86_64" => HelperTarget::LinuxX86_64,
        _ => anyhow::bail!("ssh_probe_architecture_unsupported"),
    };
    anyhow::ensure!(
        !lines[2].is_empty()
            && lines[2].len() <= 256
            && !lines[2].chars().any(char::is_control)
            && !lines[2].contains('/'),
        "ssh_probe_account_invalid"
    );
    deppy_core::workspace_target::RemotePath::new(lines[3])?;
    Ok(ServerProbe {
        target,
        account: lines[2].into(),
        home: lines[3].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn artifact() -> (Vec<u8>, BundledHelperManifest) {
        let mut bytes = vec![0; 128];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        let manifest = BundledHelperManifest {
            version: "0.13.0".into(),
            target: "linux-aarch64".into(),
            sha256: Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            bytes: bytes.len() as u64,
        };
        (bytes, manifest)
    }
    #[test]
    fn remote_bootstrap_rejects_digest_architecture_size_and_version_before_install() {
        let (mut bytes, manifest) = artifact();
        bytes[100] = 1;
        assert!(
            VerifiedHelperArtifact::verify(bytes, &manifest, "0.13.0").is_err(),
            "tampered bundled helper"
        );
        let (bytes, mut manifest) = artifact();
        manifest.target = "linux-x86_64".into();
        assert!(
            VerifiedHelperArtifact::verify(bytes, &manifest, "0.13.0").is_err(),
            "manifest cannot mislabel ELF architecture"
        );
        let (bytes, mut manifest) = artifact();
        manifest.bytes += 1;
        assert!(VerifiedHelperArtifact::verify(bytes, &manifest, "0.13.0").is_err());
        let (bytes, manifest) = artifact();
        assert!(VerifiedHelperArtifact::verify(bytes, &manifest, "0.14.0").is_err());
    }
    #[test]
    fn remote_bootstrap_install_needs_current_generation_explicit_consent() {
        let (bytes, manifest) = artifact();
        let artifact = VerifiedHelperArtifact::verify(bytes, &manifest, "0.13.0").unwrap();
        assert!(
            prepare_helper_install(&artifact, None, 2).is_err(),
            "no implicit install consent"
        );
        assert!(
            prepare_helper_install(
                &artifact,
                Some(HelperInstallConsent {
                    connection_generation: 1
                }),
                2
            )
            .is_err(),
            "stale dialog approval"
        );
        let plan = prepare_helper_install(
            &artifact,
            Some(HelperInstallConsent {
                connection_generation: 2,
            }),
            2,
        )
        .unwrap();
        assert_eq!(plan.ready_marker, "DEPPY_HELPER_READY 0.13.0");
        assert!(!plan.bytes.is_empty());
    }

    #[test]
    fn remote_bootstrap_probe_requires_exact_linux_identity() {
        let probe = parse_server_probe(b"Linux\naarch64\nubuntu\n/home/ubuntu\n").unwrap();
        assert_eq!(probe.target, HelperTarget::LinuxArm64);
        assert_eq!(probe.account, "ubuntu");
        assert_eq!(probe.home, "/home/ubuntu");
        assert!(parse_server_probe(b"Darwin\narm64\nuser\n/Users/user\n").is_err());
        assert!(parse_server_probe(b"Linux\nx86_64\nuser\nrelative\n").is_err());
        assert!(parse_server_probe(b"Linux\nx86_64\nuser\n/home/user\nextra\n").is_err());
        assert!(parse_server_probe(b"Linux\nx86_64\nuser\rname\n/home/user\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn remote_bootstrap_rejects_symlinked_bundle_helper_directory() {
        let fixture =
            std::env::temp_dir().join(format!("deppy-helper-bundle-{}", uuid::Uuid::new_v4()));
        let resources = fixture.join("Resources");
        let outside = fixture.join("outside");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(outside.join("linux-aarch64")).unwrap();
        let (bytes, manifest) = artifact();
        std::fs::write(
            outside.join("manifest.json"),
            serde_json::to_vec(&vec![manifest]).unwrap(),
        )
        .unwrap();
        std::fs::write(outside.join("linux-aarch64/deppy-remote-host"), bytes).unwrap();
        std::os::unix::fs::symlink(&outside, resources.join("remote-host")).unwrap();
        let result = VerifiedHelperArtifact::load(&resources, HelperTarget::LinuxArm64, "0.13.0");
        std::fs::remove_dir_all(fixture).unwrap();
        assert!(
            result.is_err(),
            "a bundle cannot select helper bytes through an outside directory symlink"
        );
    }
}
