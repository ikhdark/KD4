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

/// Update action appropriate for a detected Codex installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update a global npm installation.
    NpmGlobalLatest,
    /// Update a global Bun installation.
    BunGlobalLatest,
    /// Update a global pnpm installation.
    PnpmGlobalLatest,
    /// Rerun the standalone Windows installer.
    StandaloneWindows,
}

impl UpdateAction {
    pub fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm => Some(Self::NpmGlobalLatest),
            InstallMethod::Bun => Some(Self::BunGlobalLatest),
            InstallMethod::Pnpm => Some(Self::PnpmGlobalLatest),
            InstallMethod::Standalone { .. } => Some(Self::StandaloneWindows),
            InstallMethod::Other => None,
        }
    }

    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::NpmGlobalLatest => ("npm", &["install", "-g", "@openai/codex"]),
            Self::BunGlobalLatest => ("bun", &["install", "-g", "@openai/codex"]),
            Self::PnpmGlobalLatest => ("pnpm", &["add", "-g", "@openai/codex"]),
            Self::StandaloneWindows => (
                "powershell",
                &[
                    "-ExecutionPolicy",
                    "Bypass",
                    "-c",
                    "$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex",
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
    if !release_dir.starts_with(releases_root.as_path()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;

    const TEST_RESOURCE_NAME: &str = "codex-test-helper";

    #[test]
    fn release_version_policy_is_shared() {
        assert_eq!(version_from_release_tag("rust-v1.5.0"), Some("1.5.0"));
        assert_eq!(version_from_release_tag("v1.5.0"), None);
        assert_eq!(is_newer_version("1.2.4", "1.2.3"), Some(true));
        assert_eq!(is_newer_version("1.2.3", "1.2.4"), Some(false));
        assert_eq!(is_newer_version("1.2.3-beta.1", "1.2.2"), None);
        assert_eq!(is_newer_version(" 1.2.3 ", "1.2.2"), Some(true));
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn update_actions_come_from_install_context() {
        let standalone_release_dir =
            AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join("standalone-release"))
                .expect("temp dir path should be absolute");
        for (method, expected) in [
            (InstallMethod::Npm, Some(UpdateAction::NpmGlobalLatest)),
            (InstallMethod::Bun, Some(UpdateAction::BunGlobalLatest)),
            (InstallMethod::Pnpm, Some(UpdateAction::PnpmGlobalLatest)),
            (
                InstallMethod::Standalone {
                    release_dir: standalone_release_dir,
                    resources_dir: None,
                },
                Some(UpdateAction::StandaloneWindows),
            ),
            (InstallMethod::Other, None),
        ] {
            assert_eq!(
                UpdateAction::from_install_context(&InstallContext {
                    method,
                    package_layout: None,
                }),
                expected
            );
        }
        assert_eq!(
            UpdateAction::NpmGlobalLatest.command_args(),
            ("npm", &["install", "-g", "@openai/codex"][..])
        );
        assert_eq!(
            UpdateAction::StandaloneWindows.command_args(),
            (
                "powershell",
                &[
                    "-ExecutionPolicy",
                    "Bypass",
                    "-c",
                    "$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex"
                ][..],
            )
        );
    }

    #[test]
    fn detects_standalone_install_from_release_layout() -> std::io::Result<()> {
        let codex_home = tempfile::tempdir()?;
        let release_dir = codex_home
            .path()
            .join("packages/standalone/releases/1.2.3-x86_64-pc-windows-msvc");
        let resources_dir = release_dir.join(RESOURCES_DIRNAME);
        fs::create_dir_all(&resources_dir)?;
        let exe_path = release_dir.join("codex.exe");
        fs::write(&exe_path, "")?;
        fs::write(resources_dir.join(default_rg_command()), "")?;
        fs::write(resources_dir.join(TEST_RESOURCE_NAME), "")?;
        let canonical_release_dir =
            AbsolutePathBuf::from_absolute_path(release_dir.canonicalize()?)?;
        let canonical_resources_dir =
            AbsolutePathBuf::from_absolute_path(resources_dir.canonicalize()?)?;

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ Some(codex_home.path()),
        );
        assert_eq!(
            context,
            InstallContext {
                method: InstallMethod::Standalone {
                    release_dir: canonical_release_dir,
                    resources_dir: Some(canonical_resources_dir.clone()),
                },
                package_layout: None,
            }
        );
        assert_eq!(
            context.bundled_resource(TEST_RESOURCE_NAME),
            Some(canonical_resources_dir.join(TEST_RESOURCE_NAME))
        );
        Ok(())
    }

    #[test]
    fn standalone_rg_falls_back_when_resources_are_missing() -> std::io::Result<()> {
        let codex_home = tempfile::tempdir()?;
        let release_dir = codex_home
            .path()
            .join("packages/standalone/releases/1.2.3-x86_64-pc-windows-msvc");
        fs::create_dir_all(&release_dir)?;
        let exe_path = release_dir.join("codex.exe");
        fs::write(&exe_path, "")?;

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ Some(codex_home.path()),
        );
        assert_eq!(context.rg_command(), default_rg_command());
        Ok(())
    }

    #[test]
    fn detects_package_layout_independently_from_install_method() -> std::io::Result<()> {
        let package_dir = tempfile::tempdir()?;
        let bin_dir = package_dir.path().join(BIN_DIRNAME);
        let resources_dir = package_dir.path().join(RESOURCES_DIRNAME);
        let path_dir = package_dir.path().join(PATH_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&resources_dir)?;
        fs::create_dir_all(&path_dir)?;
        fs::write(package_dir.path().join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join("codex.exe");
        fs::write(&exe_path, "")?;
        fs::write(resources_dir.join(TEST_RESOURCE_NAME), "")?;
        fs::write(path_dir.join(default_rg_command()), "")?;
        let canonical_package_dir =
            AbsolutePathBuf::from_absolute_path(package_dir.path().canonicalize()?)?;
        let canonical_bin_dir = AbsolutePathBuf::from_absolute_path(bin_dir.canonicalize()?)?;
        let canonical_resources_dir =
            AbsolutePathBuf::from_absolute_path(resources_dir.canonicalize()?)?;
        let canonical_path_dir = AbsolutePathBuf::from_absolute_path(path_dir.canonicalize()?)?;
        let package_layout = CodexPackageLayout {
            package_dir: canonical_package_dir,
            bin_dir: canonical_bin_dir,
            resources_dir: Some(canonical_resources_dir.clone()),
            path_dir: Some(canonical_path_dir.clone()),
        };

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ None,
        );
        assert_eq!(
            context,
            InstallContext {
                method: InstallMethod::Other,
                package_layout: Some(package_layout),
            }
        );
        assert_eq!(
            context.rg_command(),
            canonical_path_dir
                .join(default_rg_command())
                .into_path_buf()
        );
        assert_eq!(
            context.bundled_resource(TEST_RESOURCE_NAME),
            Some(canonical_resources_dir.join(TEST_RESOURCE_NAME))
        );
        Ok(())
    }

    #[test]
    fn standalone_package_layout_keeps_standalone_install_method() -> std::io::Result<()> {
        let codex_home = tempfile::tempdir()?;
        let package_dir = codex_home
            .path()
            .join("packages/standalone/releases/1.2.3-x86_64-pc-windows-msvc");
        let bin_dir = package_dir.join(BIN_DIRNAME);
        let resources_dir = package_dir.join(RESOURCES_DIRNAME);
        let path_dir = package_dir.join(PATH_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&resources_dir)?;
        fs::create_dir_all(&path_dir)?;
        fs::write(package_dir.join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join("codex.exe");
        fs::write(&exe_path, "")?;
        fs::write(resources_dir.join(TEST_RESOURCE_NAME), "")?;
        fs::write(path_dir.join(default_rg_command()), "")?;
        let canonical_package_dir =
            AbsolutePathBuf::from_absolute_path(package_dir.canonicalize()?)?;
        let canonical_bin_dir = AbsolutePathBuf::from_absolute_path(bin_dir.canonicalize()?)?;
        let canonical_resources_dir =
            AbsolutePathBuf::from_absolute_path(resources_dir.canonicalize()?)?;
        let canonical_path_dir = AbsolutePathBuf::from_absolute_path(path_dir.canonicalize()?)?;

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ Some(codex_home.path()),
        );
        assert_eq!(
            context,
            InstallContext {
                method: InstallMethod::Standalone {
                    release_dir: canonical_package_dir.clone(),
                    resources_dir: Some(canonical_resources_dir.clone()),
                },
                package_layout: Some(CodexPackageLayout {
                    package_dir: canonical_package_dir,
                    bin_dir: canonical_bin_dir,
                    resources_dir: Some(canonical_resources_dir.clone()),
                    path_dir: Some(canonical_path_dir.clone()),
                }),
            }
        );
        assert_eq!(
            context.rg_command(),
            canonical_path_dir
                .join(default_rg_command())
                .into_path_buf()
        );
        assert_eq!(
            context.bundled_resource(TEST_RESOURCE_NAME),
            Some(canonical_resources_dir.join(TEST_RESOURCE_NAME))
        );
        Ok(())
    }

    #[test]
    fn npm_managed_package_keeps_package_layout() -> std::io::Result<()> {
        let package_dir = tempfile::tempdir()?;
        let bin_dir = package_dir.path().join(BIN_DIRNAME);
        let path_dir = package_dir.path().join(PATH_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&path_dir)?;
        fs::write(package_dir.path().join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join("codex.exe");
        fs::write(&exe_path, "")?;
        fs::write(path_dir.join(default_rg_command()), "")?;
        let canonical_path_dir = AbsolutePathBuf::from_absolute_path(path_dir.canonicalize()?)?;

        let context = InstallContext::from_exe(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ Some(InstallMethod::Npm),
        );
        assert_eq!(context.method, InstallMethod::Npm);
        assert!(context.package_layout.is_some());
        assert_eq!(
            context.rg_command(),
            canonical_path_dir
                .join(default_rg_command())
                .into_path_buf()
        );
        Ok(())
    }

    #[test]
    fn standalone_package_rg_falls_back_when_codex_path_is_missing() -> std::io::Result<()> {
        let package_dir = tempfile::tempdir()?;
        let bin_dir = package_dir.path().join(BIN_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::write(package_dir.path().join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join("codex.exe");
        fs::write(&exe_path, "")?;

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ None,
        );
        assert_eq!(context.rg_command(), default_rg_command());
        Ok(())
    }

    #[test]
    fn bundled_file_lookups_ignore_directories() -> std::io::Result<()> {
        let package_dir = tempfile::tempdir()?;
        let bin_dir = package_dir.path().join(BIN_DIRNAME);
        let resources_dir = package_dir.path().join(RESOURCES_DIRNAME);
        let path_dir = package_dir.path().join(PATH_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(resources_dir.join(TEST_RESOURCE_NAME))?;
        fs::create_dir_all(path_dir.join(default_rg_command()))?;
        fs::write(package_dir.path().join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join("codex.exe");
        fs::write(&exe_path, "")?;

        let context = InstallContext::from_exe_with_codex_home(
            /*current_exe*/ Some(&exe_path),
            /*method_override*/ None,
            /*codex_home*/ None,
        );
        assert_eq!(context.rg_command(), default_rg_command());
        assert_eq!(context.bundled_resource(TEST_RESOURCE_NAME), None);
        Ok(())
    }

    #[test]
    fn package_manager_method_overrides_take_precedence() {
        let pnpm_context = InstallContext::from_exe(
            /*current_exe*/ Some(Path::new(r"C:\Codex\codex.exe")),
            /*method_override*/ Some(InstallMethod::Pnpm),
        );
        assert_eq!(
            pnpm_context,
            InstallContext {
                method: InstallMethod::Pnpm,
                package_layout: None,
            }
        );

        let npm_context = InstallContext::from_exe(
            /*current_exe*/ Some(Path::new(r"C:\Codex\codex.exe")),
            /*method_override*/ Some(InstallMethod::Npm),
        );
        assert_eq!(
            npm_context,
            InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }
        );

        let bun_context = InstallContext::from_exe(
            /*current_exe*/ Some(Path::new(r"C:\Codex\codex.exe")),
            /*method_override*/ Some(InstallMethod::Bun),
        );
        assert_eq!(
            bun_context,
            InstallContext {
                method: InstallMethod::Bun,
                package_layout: None,
            }
        );
    }
}
