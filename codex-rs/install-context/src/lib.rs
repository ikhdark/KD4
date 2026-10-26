use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use codex_utils_absolute_path::AbsolutePathBuf;

const BIN_DIRNAME: &str = "bin";
const PACKAGE_METADATA_FILENAME: &str = "codex-package.json";
const PATH_DIRNAME: &str = "codex-path";
const RELEASES_DIRNAME: &str = "releases";
const RESOURCES_DIRNAME: &str = "codex-resources";
const STANDALONE_PACKAGES_DIRNAME: &str = "standalone";
static INSTALL_CONTEXT: OnceLock<InstallContext> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexPackageLayout {
    /// The package root that contains the metadata file and layout directories.
    pub package_dir: AbsolutePathBuf,
    /// Directory containing the Codex entrypoint executable.
    pub bin_dir: AbsolutePathBuf,
    /// Directory containing managed helper binaries and data files, when present.
    pub resources_dir: Option<AbsolutePathBuf>,
    /// Folder that should be prepended to the PATH, when present.
    pub path_dir: Option<AbsolutePathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallContext {
    pub method: InstallMethod,
    pub package_layout: Option<CodexPackageLayout>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallMethod {
    Standalone {
        /// The managed standalone release directory. Legacy installs use paths
        /// such as `%CODEX_HOME%\packages\standalone\releases\0.111.0-x86_64-pc-windows-msvc`.
        /// Package-layout installs use the package root that contains `bin/`,
        /// `codex-resources/`, and `codex-path/`.
        release_dir: AbsolutePathBuf,
        /// The bundled resource directory for managed dependencies.
        resources_dir: Option<AbsolutePathBuf>,
    },
    /// A Codex binary launched through the npm-managed `codex.js` shim.
    Npm,
    /// A Codex binary launched through the bun-managed `codex.js` shim.
    Bun,
    /// A Codex binary launched through the pnpm-managed `codex.js` shim.
    Pnpm,
    /// Any other execution environment.
    ///
    /// This commonly covers `cargo run`, app-bundled Codex binaries, custom
    /// internal launchers, and tests that execute Codex from an arbitrary path.
    Other,
}

/// Latest release of this fork. Update checks, release notes, and the standalone
/// installer must all use this channel: upstream releases and npm packages would
/// replace the fork with a different build.
pub const LATEST_RELEASE_URL: &str = "https://github.com/ikhdark/KD4/releases/latest";
/// GitHub API endpoint for [`LATEST_RELEASE_URL`].
pub const LATEST_RELEASE_API_URL: &str = "https://api.github.com/repos/ikhdark/KD4/releases/latest";

/// Update action appropriate for a detected Codex installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Rerun the fork's standalone Windows installer.
    StandaloneWindows,
}

impl UpdateAction {
    /// Package-manager installs come from fork release tarballs that are not
    /// published to a registry, so no package-manager command can update them.
    pub fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Standalone { .. } => Some(Self::StandaloneWindows),
            InstallMethod::Npm
            | InstallMethod::Bun
            | InstallMethod::Pnpm
            | InstallMethod::Other => None,
        }
    }

    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::StandaloneWindows => (
                "powershell",
                &[
                    "-ExecutionPolicy",
                    "Bypass",
                    "-c",
                    "$env:CODEX_NON_INTERACTIVE=1; irm https://raw.githubusercontent.com/ikhdark/KD4/main/scripts/install/install.ps1 | iex",
                ],
            ),
        }
    }

    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().copied()))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

/// Compare two stable, three-component release versions.
pub fn is_newer_version(latest: &str, current: &str) -> Option<bool> {
    match (
        parse_release_version(latest),
        parse_release_version(current),
    ) {
        (Some(latest), Some(current)) => Some(latest > current),
        _ => None,
    }
}

/// Extract the version component from a Codex Rust release tag.
pub fn version_from_release_tag(tag: &str) -> Option<&str> {
    tag.strip_prefix("rust-v")
}

/// Return whether a version identifies an unversioned source build.
pub fn is_source_build_version(version: &str) -> bool {
    parse_release_version(version) == Some((0, 0, 0))
}

fn parse_release_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

impl InstallContext {
    pub fn from_exe(current_exe: Option<&Path>, method_override: Option<InstallMethod>) -> Self {
        let codex_home = codex_utils_home_dir::find_codex_home().ok();
        Self::from_exe_with_codex_home(current_exe, method_override, codex_home.as_deref())
    }

    fn from_exe_with_codex_home(
        current_exe: Option<&Path>,
        method_override: Option<InstallMethod>,
        codex_home: Option<&Path>,
    ) -> Self {
        let package_layout = current_exe.and_then(CodexPackageLayout::from_exe);
        let method = if let Some(method) = method_override {
            method
        } else if let Some(exe_path) = current_exe {
            install_method_from_exe(exe_path, codex_home, package_layout.as_ref())
        } else {
            InstallMethod::Other
        };

        Self {
            method,
            package_layout,
        }
    }

    pub fn current() -> &'static Self {
        INSTALL_CONTEXT.get_or_init(|| {
            let current_exe = std::env::current_exe().ok();
            let method_override = if std::env::var_os("CODEX_MANAGED_BY_PNPM").is_some() {
                Some(InstallMethod::Pnpm)
            } else if std::env::var_os("CODEX_MANAGED_BY_NPM").is_some() {
                Some(InstallMethod::Npm)
            } else if std::env::var_os("CODEX_MANAGED_BY_BUN").is_some() {
                Some(InstallMethod::Bun)
            } else {
                None
            };
            Self::from_exe(current_exe.as_deref(), method_override)
        })
    }

    pub fn rg_command(&self) -> PathBuf {
        if let Some(package_layout) = &self.package_layout
            && let Some(path_dir) = &package_layout.path_dir
        {
            let bundled_rg = path_dir.join(default_rg_command());
            if bundled_rg.is_file() {
                return bundled_rg.into_path_buf();
            }
        }

        if let InstallMethod::Standalone {
            resources_dir: Some(resources_dir),
            ..
        } = &self.method
        {
            let bundled_rg = resources_dir.join(default_rg_command());
            if bundled_rg.is_file() {
                return bundled_rg.into_path_buf();
            }
        }

        default_rg_command()
    }

    pub fn bundled_resource(&self, file_name: impl AsRef<Path>) -> Option<AbsolutePathBuf> {
        if let Some(package_layout) = &self.package_layout
            && let Some(resources_dir) = &package_layout.resources_dir
        {
            let resource = resources_dir.join(file_name.as_ref());
            if resource.is_file() {
                return Some(resource);
            }
        }

        if let InstallMethod::Standalone {
            resources_dir: Some(resources_dir),
            ..
        } = &self.method
        {
            let resource = resources_dir.join(file_name);
            if resource.is_file() {
                return Some(resource);
            }
        }

        None
    }
}

impl CodexPackageLayout {
    fn from_exe(exe_path: &Path) -> Option<Self> {
        let canonical_exe = canonical_absolute_path(exe_path)?;
        let exe_dir = canonical_exe.parent()?;
        match exe_dir.file_name() {
            Some(name) if name == OsStr::new(BIN_DIRNAME) => Self::from_package_bin_dir(exe_dir),
            Some(_) | None => None,
        }
    }

    fn from_package_bin_dir(bin_dir: AbsolutePathBuf) -> Option<Self> {
        let package_dir = bin_dir.parent()?;
        if !package_dir.join(PACKAGE_METADATA_FILENAME).is_file() {
            return None;
        }

        Some(Self {
            resources_dir: existing_dir(package_dir.join(RESOURCES_DIRNAME)),
            path_dir: existing_dir(package_dir.join(PATH_DIRNAME)),
            package_dir,
            bin_dir,
        })
    }
}

fn install_method_from_exe(
    exe_path: &Path,
    codex_home: Option<&Path>,
    package_layout: Option<&CodexPackageLayout>,
) -> InstallMethod {
    if let Some(standalone_method) = standalone_install_method(exe_path, codex_home, package_layout)
    {
        return standalone_method;
    }

    InstallMethod::Other
}

fn standalone_install_method(
    exe_path: &Path,
    codex_home: Option<&Path>,
    package_layout: Option<&CodexPackageLayout>,
) -> Option<InstallMethod> {
    let canonical_codex_home = canonical_absolute_path(codex_home?)?;
    let release_dir = if let Some(package_layout) = package_layout {
        package_layout.package_dir.clone()
    } else {
        canonical_absolute_path(exe_path)?.parent()?
    };
    let releases_root = canonical_codex_home
        .join("packages")
        .join(STANDALONE_PACKAGES_DIRNAME)
        .join(RELEASES_DIRNAME);
    // Only immediate release children belong to the installer. A nested helper
    // or an unrelated executable in the releases root is not an update target.
    if release_dir.parent().as_ref() != Some(&releases_root) {
        return None;
    }

    let resources_dir = release_dir.join(RESOURCES_DIRNAME);
    Some(InstallMethod::Standalone {
        release_dir,
        resources_dir: resources_dir.is_dir().then_some(resources_dir),
    })
}

fn canonical_absolute_path(path: &Path) -> Option<AbsolutePathBuf> {
    let canonical_path = std::fs::canonicalize(path).ok()?;
    AbsolutePathBuf::from_absolute_path(canonical_path).ok()
}

fn existing_dir(path: AbsolutePathBuf) -> Option<AbsolutePathBuf> {
    path.is_dir().then_some(path)
}

fn default_rg_command() -> PathBuf {
    PathBuf::from("rg.exe")
}

