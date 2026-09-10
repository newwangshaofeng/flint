use remote::RemotePlatform;
use std::{collections::HashSet, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentArtifactFormat {
    Raw,
    TarGz {
        executable_path: &'static str,
    },
    TarGzBundle {
        root_path: &'static str,
        executable_path: &'static str,
    },
    ZipBundle {
        root_path: &'static str,
        executable_path: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSourceVerification {
    Sha256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentVersionMatcher {
    Codex { version: &'static str },
    Claude { version: &'static str },
    Pi { version: &'static str },
    OpenCode { version: &'static str },
}

impl AgentVersionMatcher {
    pub fn matches(self, output: &str) -> bool {
        let mut fields = output.split_whitespace();
        match self {
            Self::Codex { version } => {
                fields.next().is_some_and(|agent| {
                    agent.eq_ignore_ascii_case("codex") || agent.eq_ignore_ascii_case("codex-cli")
                }) && fields.next() == Some(version)
            }
            Self::Claude { version } => {
                let fields = output.split_whitespace().collect::<Vec<_>>();
                matches!(
                    fields.as_slice(),
                    [found_version, "(Claude", "Code)"] if *found_version == version
                ) || matches!(
                    fields.as_slice(),
                    ["Claude", "Code", found_version] if *found_version == version
                )
            }
            Self::Pi { version } => output.trim() == version,
            Self::OpenCode { version } => output.trim() == version,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentRelease {
    pub version: &'static str,
    pub target: RemotePlatform,
    pub source_url: &'static str,
    pub source_sha256: &'static str,
    pub source_verification: AgentSourceVerification,
    pub executable_sha256: &'static str,
    pub artifact: AgentArtifactFormat,
    pub executable_name: &'static str,
    pub version_matcher: AgentVersionMatcher,
    pub self_update_environment: &'static [(&'static str, &'static str)],
}

pub struct AgentReleaseCatalog<'a> {
    agent_id: &'static str,
    official_source_prefixes: &'static [&'static str],
    releases: &'a [AgentRelease],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentSelfUpdatePolicy {
    pub environment: &'static [(&'static str, &'static str)],
    pub arguments: &'static [&'static str],
}

macro_rules! claude_release {
    ($os:expr, $arch:expr, $libc:expr, $platform:literal, $name:literal, $digest:literal) => {
        AgentRelease {
            version: "2.1.267",
            target: RemotePlatform {
                os: $os,
                arch: $arch,
                libc: $libc,
            },
            source_url: concat!(
                "https://downloads.claude.ai/claude-code-releases/2.1.267/",
                $platform,
                "/",
                $name
            ),
            source_sha256: $digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $digest,
            artifact: AgentArtifactFormat::Raw,
            executable_name: $name,
            version_matcher: AgentVersionMatcher::Claude { version: "2.1.267" },
            self_update_environment: &[("DISABLE_UPDATES", "1")],
        }
    };
}

pub const CLAUDE_RELEASES: &[AgentRelease] = &[
    claude_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::Aarch64,
        None,
        "darwin-arm64",
        "claude",
        "a681f3008f0050029aeebcab3af51bb6a55ddeb625a3af3141a4416d43cd2558"
    ),
    claude_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::X86_64,
        None,
        "darwin-x64",
        "claude",
        "071988cb2e5a4378d8543d78e0ff5f8ed1ecc5e113271774a0582ee74fb0ef79"
    ),
    claude_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::Aarch64,
        Some(remote::RemoteLibc::Glibc),
        "linux-arm64",
        "claude",
        "226a4e009574044a18bf5495f127806b2a1bfcbf25b3c01608705fafee95fefb"
    ),
    claude_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::X86_64,
        Some(remote::RemoteLibc::Glibc),
        "linux-x64",
        "claude",
        "0399c793ff571d5946ef923d80b4f330d05ac4b6842a6b0775468f5d389403c0"
    ),
    claude_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::Aarch64,
        Some(remote::RemoteLibc::Musl),
        "linux-arm64-musl",
        "claude",
        "2aa337b3610742cdc9ed5dc5123d0ac41094849235182a5e6f2b7956da7ccaa5"
    ),
    claude_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::X86_64,
        Some(remote::RemoteLibc::Musl),
        "linux-x64-musl",
        "claude",
        "3c336039170511e6592be624097e95eb9e85be0ecf96f78b331907497512c401"
    ),
    claude_release!(
        remote::RemoteOs::Windows,
        remote::RemoteArch::X86_64,
        None,
        "win32-x64",
        "claude.exe",
        "23dde2a47cf1d7d9c4a2d96d21fa80ea9bfc872dfde0ee06e9982d2908603350"
    ),
    claude_release!(
        remote::RemoteOs::Windows,
        remote::RemoteArch::Aarch64,
        None,
        "win32-arm64",
        "claude.exe",
        "0dc306259e3036af4255f66b77451d3f7297bcd376bfae20f7b42e2fa1473607"
    ),
];

macro_rules! codex_archive_release {
    ($os:expr, $arch:expr, $libc:expr, $target:literal, $source_digest:literal, $executable_digest:literal) => {
        AgentRelease {
            version: "0.154.0",
            target: RemotePlatform {
                os: $os,
                arch: $arch,
                libc: $libc,
            },
            source_url: concat!(
                "https://github.com/openai/codex/releases/download/rust-v0.154.0/codex-",
                $target,
                ".tar.gz"
            ),
            source_sha256: $source_digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $executable_digest,
            artifact: AgentArtifactFormat::TarGz {
                executable_path: concat!("codex-", $target),
            },
            executable_name: "codex",
            version_matcher: AgentVersionMatcher::Codex { version: "0.154.0" },
            self_update_environment: &[],
        }
    };
}

macro_rules! codex_windows_release {
    ($arch:expr, $target:literal, $digest:literal) => {
        AgentRelease {
            version: "0.154.0",
            target: RemotePlatform {
                os: remote::RemoteOs::Windows,
                arch: $arch,
                libc: None,
            },
            source_url: concat!(
                "https://github.com/openai/codex/releases/download/rust-v0.154.0/codex-",
                $target,
                ".exe"
            ),
            source_sha256: $digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $digest,
            artifact: AgentArtifactFormat::Raw,
            executable_name: "codex.exe",
            version_matcher: AgentVersionMatcher::Codex { version: "0.154.0" },
            self_update_environment: &[],
        }
    };
}

pub const CODEX_RELEASES: &[AgentRelease] = &[
    codex_archive_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::Aarch64,
        None,
        "aarch64-apple-darwin",
        "344310a0a591c1b192e04feff304321a69907c9498baaac331ca7e16ebcef9d7",
        "4f85982624b3898c8991cb80c0981b2aa71070e3537046c9a95950318a95afcc"
    ),
    codex_archive_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::X86_64,
        None,
        "x86_64-apple-darwin",
        "1219c837d8f813b493a424c125c0038b5d9ca16279bc6d3fe6ce037a3e18a6e7",
        "b0e26f09819c4b27f621853800c29f95ac5d526ad9b88ab641de4db86835718d"
    ),
    codex_archive_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::Aarch64,
        Some(remote::RemoteLibc::Glibc),
        "aarch64-unknown-linux-musl",
        "583b48df32804213bdcd338c2e5adb06b34340821fa757a726cc0a524fa33c27",
        "9b7c1c7abdc26fc3c4f47c77656a8e9121def5483dbae830ef1ee561758448a9"
    ),
    codex_archive_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::Aarch64,
        Some(remote::RemoteLibc::Musl),
        "aarch64-unknown-linux-musl",
        "583b48df32804213bdcd338c2e5adb06b34340821fa757a726cc0a524fa33c27",
        "9b7c1c7abdc26fc3c4f47c77656a8e9121def5483dbae830ef1ee561758448a9"
    ),
    codex_archive_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::X86_64,
        Some(remote::RemoteLibc::Glibc),
        "x86_64-unknown-linux-musl",
        "d7e18b2597ae8f242f5f31ee9e90deef48dbc9edd634d9868fb6435d08c07f02",
        "3188814c35471432d4123203e0eb38e5bddc60226e3d7ddf0e59e649ea140022"
    ),
    codex_archive_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::X86_64,
        Some(remote::RemoteLibc::Musl),
        "x86_64-unknown-linux-musl",
        "d7e18b2597ae8f242f5f31ee9e90deef48dbc9edd634d9868fb6435d08c07f02",
        "3188814c35471432d4123203e0eb38e5bddc60226e3d7ddf0e59e649ea140022"
    ),
    codex_windows_release!(
        remote::RemoteArch::X86_64,
        "x86_64-pc-windows-msvc",
        "be96b992178b1e467c225800da0d65f2c86d5eba1ef0b14632f65db381cbdfde"
    ),
    codex_windows_release!(
        remote::RemoteArch::Aarch64,
        "aarch64-pc-windows-msvc",
        "dc6d744d747a50f8caf7f08817e0ccc9b09781f6269dec3609e2f9fbe036233d"
    ),
];

macro_rules! pi_tar_release {
    ($os:expr, $arch:expr, $libc:expr, $asset:literal, $source_digest:literal, $executable_digest:literal) => {
        AgentRelease {
            version: "0.81.1",
            target: RemotePlatform {
                os: $os,
                arch: $arch,
                libc: $libc,
            },
            source_url: concat!(
                "https://github.com/earendil-works/pi/releases/download/v0.81.1/",
                $asset
            ),
            source_sha256: $source_digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $executable_digest,
            artifact: AgentArtifactFormat::TarGzBundle {
                root_path: "pi",
                executable_path: "pi/pi",
            },
            executable_name: "pi",
            version_matcher: AgentVersionMatcher::Pi { version: "0.81.1" },
            self_update_environment: &[("PI_SKIP_VERSION_CHECK", "1"), ("PI_TELEMETRY", "0")],
        }
    };
}

macro_rules! pi_windows_release {
    ($arch:expr, $asset:literal, $source_digest:literal, $executable_digest:literal) => {
        AgentRelease {
            version: "0.81.1",
            target: RemotePlatform {
                os: remote::RemoteOs::Windows,
                arch: $arch,
                libc: None,
            },
            source_url: concat!(
                "https://github.com/earendil-works/pi/releases/download/v0.81.1/",
                $asset
            ),
            source_sha256: $source_digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $executable_digest,
            artifact: AgentArtifactFormat::ZipBundle {
                root_path: "",
                executable_path: "pi.exe",
            },
            executable_name: "pi.exe",
            version_matcher: AgentVersionMatcher::Pi { version: "0.81.1" },
            self_update_environment: &[("PI_SKIP_VERSION_CHECK", "1"), ("PI_TELEMETRY", "0")],
        }
    };
}

pub const PI_RELEASES: &[AgentRelease] = &[
    pi_tar_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::Aarch64,
        None,
        "pi-darwin-arm64.tar.gz",
        "a24834019ec02ee5a475ff1c5a5e9f838974191ba6adc4348f6e6475a7c7667b",
        "271b7a506398e4ece04c664c7723705d4fa874c98e7a62d7b289e1fa582cf3c9"
    ),
    pi_tar_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::X86_64,
        None,
        "pi-darwin-x64.tar.gz",
        "ecaed0ef0fcaeff2e475294fc34b2d7de4700434ab9df23cdb0fffd9cfadf5b8",
        "330434ba869a9582557be97ce5423afbdcc8553e16a9301cfdd9740ddc87c69b"
    ),
    pi_tar_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::Aarch64,
        Some(remote::RemoteLibc::Glibc),
        "pi-linux-arm64.tar.gz",
        "c049e132c85466224d57d19f7924909b0c0fdbc9bed8e091ddc361830704b392",
        "5d098361cdcbb3abad53988d692bcc6c2619bfd8875e8256903f448c54e82a5c"
    ),
    pi_tar_release!(
        remote::RemoteOs::Linux,
        remote::RemoteArch::X86_64,
        Some(remote::RemoteLibc::Glibc),
        "pi-linux-x64.tar.gz",
        "1f6e23d9ec0668a13cea9c786e3d54c1fc679b8e22e7f6bfade0349f4807cbf2",
        "e69d579772ed139458a144f165e2e0f05d50a1a86bf7557bf02eef1f75691505"
    ),
    pi_windows_release!(
        remote::RemoteArch::Aarch64,
        "pi-windows-arm64.zip",
        "875dfb42e2ad20e81430365cce48c5ddcab560c3b9ee474d2d5c7ff6345269eb",
        "7d3fbae1d56b5f95607b0162f65c1e6793333d4ff2df01a7a89ef30ea44c9611"
    ),
    pi_windows_release!(
        remote::RemoteArch::X86_64,
        "pi-windows-x64.zip",
        "6c46cca1fa94234982e56dc60a453d4bc57dc45efc2e16f97bbc6eace7a7de60",
        "1421d967cc6497c0585f057414a9a7da8a69b339fc001dc49461c419acec9d85"
    ),
];

macro_rules! opencode_tar_release {
    ($arch:expr, $libc:expr, $asset:literal, $source_digest:literal, $executable_digest:literal) => {
        AgentRelease {
            version: "1.18.10",
            target: RemotePlatform {
                os: remote::RemoteOs::Linux,
                arch: $arch,
                libc: Some($libc),
            },
            source_url: concat!(
                "https://github.com/anomalyco/opencode/releases/download/v1.18.10/",
                $asset
            ),
            source_sha256: $source_digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $executable_digest,
            artifact: AgentArtifactFormat::TarGz {
                executable_path: "opencode",
            },
            executable_name: "opencode",
            version_matcher: AgentVersionMatcher::OpenCode { version: "1.18.10" },
            self_update_environment: &[("OPENCODE_DISABLE_AUTOUPDATE", "1")],
        }
    };
}

macro_rules! opencode_zip_release {
    ($os:expr, $arch:expr, $asset:literal, $executable:literal, $source_digest:literal, $executable_digest:literal) => {
        AgentRelease {
            version: "1.18.10",
            target: RemotePlatform {
                os: $os,
                arch: $arch,
                libc: None,
            },
            source_url: concat!(
                "https://github.com/anomalyco/opencode/releases/download/v1.18.10/",
                $asset
            ),
            source_sha256: $source_digest,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: $executable_digest,
            artifact: AgentArtifactFormat::ZipBundle {
                root_path: "",
                executable_path: $executable,
            },
            executable_name: $executable,
            version_matcher: AgentVersionMatcher::OpenCode { version: "1.18.10" },
            self_update_environment: &[("OPENCODE_DISABLE_AUTOUPDATE", "1")],
        }
    };
}

pub const OPENCODE_RELEASES: &[AgentRelease] = &[
    opencode_zip_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::Aarch64,
        "opencode-darwin-arm64.zip",
        "opencode",
        "641fe2e65e42db76c2d32db5f85573c3682a8c72f82d01568a922a8feccc4658",
        "7f488b3f084de90a638785b3116d5e05449a584694aa81c9a757da49139857c1"
    ),
    opencode_zip_release!(
        remote::RemoteOs::MacOs,
        remote::RemoteArch::X86_64,
        "opencode-darwin-x64-baseline.zip",
        "opencode",
        "db723500eda1d36f726748c0ba972f40a5029c3feec73f39478074ecc53a6096",
        "1149cc71d4bac2317d93566a8fc602c254ec12120b979c345601742600bf9ebb"
    ),
    opencode_tar_release!(
        remote::RemoteArch::Aarch64,
        remote::RemoteLibc::Glibc,
        "opencode-linux-arm64.tar.gz",
        "41ae3041e91b894e4c0dc06a73a9a2796254bf390ffb99626a43af5e2912d170",
        "5b6411fcdc22f96f966d328c61c22a709a333c2c3a3de9c8188d43c709487ea0"
    ),
    opencode_tar_release!(
        remote::RemoteArch::Aarch64,
        remote::RemoteLibc::Musl,
        "opencode-linux-arm64-musl.tar.gz",
        "cc5f2670b4ba833cccc4b6c532adcffba8731bbe21c697c6b0309c359ac38366",
        "e911eb46fbb7a90855243cc8470641794d35f4d73e39b3dd9f005a6a8c4bb0bd"
    ),
    opencode_tar_release!(
        remote::RemoteArch::X86_64,
        remote::RemoteLibc::Glibc,
        "opencode-linux-x64-baseline.tar.gz",
        "e2841f720dee95855524cd8c8aad45438e9e7e13f6f043e85c246f2907bc2cf9",
        "2735f786be499db50c823d961fb8627dfb74f920e2320686b67e6c5c81c66f16"
    ),
    opencode_tar_release!(
        remote::RemoteArch::X86_64,
        remote::RemoteLibc::Musl,
        "opencode-linux-x64-baseline-musl.tar.gz",
        "2497531ff8ae90232d40c4b2ebee967434673ff6b921d1b9096b398996e4aa03",
        "4e67327e2c4c0c47bdfb27e0ee9e4f45e3c5b1039ab4cbde73e4850921118876"
    ),
    opencode_zip_release!(
        remote::RemoteOs::Windows,
        remote::RemoteArch::Aarch64,
        "opencode-windows-arm64.zip",
        "opencode.exe",
        "0ef987b43df4ff427f55b805f6d44231313ef098296fa154d2043837656e092f",
        "24f1b3475262a19c5eb97b989aee60623756043f028c0a472fd0739c2d075c4f"
    ),
    opencode_zip_release!(
        remote::RemoteOs::Windows,
        remote::RemoteArch::X86_64,
        "opencode-windows-x64-baseline.zip",
        "opencode.exe",
        "d92a23aa6981d573d7e0f920cea82ace553a7d3d4a42da08c5ecaf00ed5b0c4a",
        "ba4c44cd79640031f809a6bcb6de95bcff936caf407ef4da7bbec699fb517846"
    ),
];

impl<'a> AgentReleaseCatalog<'a> {
    pub fn new(
        agent_id: &'static str,
        official_source_prefixes: &'static [&'static str],
        releases: &'a [AgentRelease],
    ) -> Self {
        Self {
            agent_id,
            official_source_prefixes,
            releases,
        }
    }

    pub fn release_for(&self, target: RemotePlatform) -> Option<&'a AgentRelease> {
        self.releases
            .iter()
            .find(|release| release.target == target)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut releases = HashSet::new();
        for release in self.releases {
            if !is_sha256(release.source_sha256) {
                anyhow::bail!(
                    "{} {} has an invalid source SHA-256",
                    self.agent_id,
                    release.version
                );
            }
            if !is_sha256(release.executable_sha256) {
                anyhow::bail!(
                    "{} {} has an invalid executable SHA-256",
                    self.agent_id,
                    release.version
                );
            }
            if let Some(path) = release.artifact.executable_path()
                && !is_safe_archive_path(path)
            {
                anyhow::bail!(
                    "{} {} has an invalid archive executable path",
                    self.agent_id,
                    release.version
                );
            }
            if let AgentArtifactFormat::TarGzBundle {
                root_path,
                executable_path,
            }
            | AgentArtifactFormat::ZipBundle {
                root_path,
                executable_path,
            } = release.artifact
            {
                if (!root_path.is_empty() && !is_safe_archive_path(root_path))
                    || Path::new(executable_path)
                        .strip_prefix(root_path)
                        .ok()
                        .filter(|path| *path == Path::new(release.executable_name))
                        .is_none()
                {
                    anyhow::bail!(
                        "{} {} has an invalid bundle layout",
                        self.agent_id,
                        release.version
                    );
                }
            }
            if !source_is_official(release.source_url, self.official_source_prefixes) {
                anyhow::bail!(
                    "{} {} does not use an official HTTPS source",
                    self.agent_id,
                    release.version
                );
            }
            if !releases.insert((release.version, release.target)) {
                anyhow::bail!(
                    "duplicate {} {} release for {:?}",
                    self.agent_id,
                    release.version,
                    release.target
                );
            }
        }
        Ok(())
    }
}

pub fn source_is_official(source: &str, official_source_prefixes: &[&str]) -> bool {
    let Ok(source) = url::Url::parse(source) else {
        return false;
    };
    if source.scheme() != "https" {
        return false;
    }
    official_source_prefixes.iter().any(|prefix| {
        url::Url::parse(prefix).is_ok_and(|prefix| source.as_str().starts_with(prefix.as_str()))
    })
}

impl AgentArtifactFormat {
    pub(crate) fn executable_path(self) -> Option<&'static str> {
        match self {
            Self::Raw => None,
            Self::TarGz { executable_path }
            | Self::TarGzBundle {
                executable_path, ..
            }
            | Self::ZipBundle {
                executable_path, ..
            } => Some(executable_path),
        }
    }
}

fn is_safe_archive_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\\') {
        return false;
    }
    let path = std::path::Path::new(path);
    path.components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_kind_registry;
    use remote::{RemoteArch, RemoteLibc, RemoteOs, RemotePlatform};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn linux_glibc_target() -> RemotePlatform {
        RemotePlatform {
            os: RemoteOs::Linux,
            arch: RemoteArch::X86_64,
            libc: Some(RemoteLibc::Glibc),
        }
    }

    fn fixture_release(target: RemotePlatform) -> AgentRelease {
        AgentRelease {
            version: "1.2.3",
            target,
            source_url: "https://github.com/openai/codex/releases/download/v1.2.3/codex",
            source_sha256: DIGEST,
            source_verification: AgentSourceVerification::Sha256,
            executable_sha256: DIGEST,
            artifact: AgentArtifactFormat::Raw,
            executable_name: "codex",
            version_matcher: AgentVersionMatcher::Codex { version: "1.2.3" },
            self_update_environment: &[("CODEX_DISABLE_UPDATE", "1")],
        }
    }

    #[test]
    fn supported_target_resolves_to_one_pinned_release() {
        let target = linux_glibc_target();
        let releases = [fixture_release(target)];
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            &releases,
        );

        let release = catalog
            .release_for(target)
            .expect("supported target should have a release");

        assert_eq!(release.version, "1.2.3");
        assert_eq!(release.source_verification, AgentSourceVerification::Sha256);
    }

    #[test]
    fn duplicate_version_and_target_is_rejected() {
        let target = linux_glibc_target();
        let releases = [fixture_release(target), fixture_release(target)];
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            &releases,
        );

        let error = catalog
            .validate()
            .expect_err("duplicate release should be rejected");

        assert!(error.to_string().contains("duplicate codex 1.2.3"));
    }

    #[test]
    fn non_https_source_is_rejected() {
        let mut release = fixture_release(linux_glibc_target());
        release.source_url = "http://github.com/openai/codex/releases/download/v1.2.3/codex";
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        let error = catalog
            .validate()
            .expect_err("non-HTTPS source should be rejected");

        assert!(error.to_string().contains("official HTTPS source"));
    }

    #[test]
    fn source_path_cannot_escape_the_official_release_prefix() {
        let mut release = fixture_release(linux_glibc_target());
        release.source_url =
            "https://github.com/openai/codex/releases/download/../../attacker/artifact";
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        let error = catalog
            .validate()
            .expect_err("normalized source outside the official prefix should be rejected");

        assert!(error.to_string().contains("official HTTPS source"));
    }

    #[test]
    fn malformed_source_sha256_is_rejected() {
        let mut release = fixture_release(linux_glibc_target());
        release.source_sha256 = "not-a-sha256";
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        let error = catalog
            .validate()
            .expect_err("malformed source digest should be rejected");

        assert!(error.to_string().contains("source SHA-256"));
    }

    #[test]
    fn malformed_executable_sha256_is_rejected() {
        let mut release = fixture_release(linux_glibc_target());
        release.executable_sha256 = "";
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        let error = catalog
            .validate()
            .expect_err("missing executable digest should be rejected");

        assert!(error.to_string().contains("executable SHA-256"));
    }

    #[test]
    fn archive_executable_path_cannot_escape_the_archive() {
        let mut release = fixture_release(linux_glibc_target());
        release.artifact = AgentArtifactFormat::TarGz {
            executable_path: "../codex",
        };
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        let error = catalog
            .validate()
            .expect_err("archive traversal path should be rejected");

        assert!(error.to_string().contains("archive executable path"));
    }

    #[test]
    fn release_may_use_kind_level_argument_update_suppression() {
        let mut release = fixture_release(linux_glibc_target());
        release.self_update_environment = &[];
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &["https://github.com/openai/codex/releases/download/"],
            std::slice::from_ref(&release),
        );

        catalog
            .validate()
            .expect("kind-level launch arguments may suppress updates");
    }

    #[test]
    fn codex_version_matcher_accepts_formatting_but_rejects_wrong_identity_or_version() {
        let matcher = AgentVersionMatcher::Codex { version: "1.2.3" };

        assert!(matcher.matches("codex-cli 1.2.3\n"));
        assert!(matcher.matches("Codex 1.2.3"));
        assert!(!matcher.matches("codex 1.2.4"));
        assert!(!matcher.matches("claude 1.2.3"));
    }

    #[test]
    fn claude_version_matcher_accepts_known_formats_but_rejects_wrong_identity_or_version() {
        let matcher = AgentVersionMatcher::Claude { version: "2.3.4" };

        assert!(matcher.matches("2.3.4 (Claude Code)\n"));
        assert!(matcher.matches("Claude Code 2.3.4"));
        assert!(!matcher.matches("2.3.5 (Claude Code)"));
        assert!(!matcher.matches("2.3.4 (Codex)"));
    }

    #[test]
    fn pi_version_matcher_accepts_exact_version_only() {
        let matcher = AgentVersionMatcher::Pi { version: "0.81.1" };

        assert!(matcher.matches("0.81.1\n"));
        assert!(!matcher.matches("0.81.0"));
        assert!(!matcher.matches("pi 0.81.1"));
    }

    #[test]
    fn opencode_version_matcher_accepts_exact_version_only() {
        let matcher = AgentVersionMatcher::OpenCode { version: "1.18.10" };

        assert!(matcher.matches("1.18.10\n"));
        assert!(!matcher.matches("1.18.9"));
        assert!(!matcher.matches("opencode 1.18.10"));
    }

    #[test]
    fn every_registered_agent_suppresses_self_updates_per_process() {
        for kind in agent_kind_registry() {
            let policy = kind.self_update_policy();
            assert!(
                !policy.environment.is_empty() || !policy.arguments.is_empty(),
                "{} should define self-update suppression",
                kind.id
            );
        }
    }

    #[test]
    fn pinned_claude_release_catalog_is_valid_and_covers_supported_targets() {
        let catalog = AgentReleaseCatalog::new(
            "claude",
            &["https://downloads.claude.ai/claude-code-releases/"],
            CLAUDE_RELEASES,
        );

        catalog
            .validate()
            .expect("pinned Claude release catalog should be valid");
        assert_eq!(CLAUDE_RELEASES.len(), 8);
        assert!(catalog.release_for(linux_glibc_target()).is_some());
    }

    #[test]
    fn pinned_codex_release_catalog_is_valid_and_covers_supported_targets() {
        let catalog = AgentReleaseCatalog::new(
            "codex",
            &[
                "https://github.com/openai/codex/releases/download/",
                "https://release-assets.githubusercontent.com/",
            ],
            CODEX_RELEASES,
        );

        catalog
            .validate()
            .expect("pinned Codex release catalog should be valid");
        assert_eq!(CODEX_RELEASES.len(), 8);
        assert!(catalog.release_for(linux_glibc_target()).is_some());
    }

    #[test]
    fn pinned_pi_release_catalog_is_valid_and_covers_published_targets() {
        let catalog = AgentReleaseCatalog::new(
            "pi",
            &[
                "https://github.com/earendil-works/pi/releases/download/",
                "https://release-assets.githubusercontent.com/",
            ],
            PI_RELEASES,
        );

        catalog
            .validate()
            .expect("pinned Pi release catalog should be valid");
        assert_eq!(PI_RELEASES.len(), 6);
        assert!(catalog.release_for(linux_glibc_target()).is_some());
        assert!(
            catalog
                .release_for(RemotePlatform {
                    os: RemoteOs::Linux,
                    arch: RemoteArch::X86_64,
                    libc: Some(RemoteLibc::Musl),
                })
                .is_none()
        );
    }

    #[test]
    fn pinned_opencode_release_catalog_is_valid_and_covers_supported_targets() {
        let catalog = AgentReleaseCatalog::new(
            "opencode",
            &[
                "https://github.com/anomalyco/opencode/releases/download/",
                "https://release-assets.githubusercontent.com/",
            ],
            OPENCODE_RELEASES,
        );

        catalog
            .validate()
            .expect("pinned OpenCode release catalog should be valid");
        assert_eq!(OPENCODE_RELEASES.len(), 8);
        assert!(catalog.release_for(linux_glibc_target()).is_some());
        assert!(
            catalog
                .release_for(RemotePlatform {
                    os: RemoteOs::Linux,
                    arch: RemoteArch::X86_64,
                    libc: Some(RemoteLibc::Musl),
                })
                .is_some()
        );
    }
}
