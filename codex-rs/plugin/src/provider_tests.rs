use super::PluginResourceLocator;
use super::ResolvedPlugin;
use super::ResolvedPluginError;
use super::ResolvedPluginLocation;
use crate::manifest::PluginManifest;
use crate::manifest::PluginManifestHooks;
use crate::manifest::PluginManifestInterface;
use crate::manifest::PluginManifestMcpServers;
use crate::manifest::PluginManifestPaths;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;

fn absolute(path: impl AsRef<std::path::Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path_checked(path.as_ref()).expect("absolute test path")
}

fn path_uri(path: &AbsolutePathBuf) -> PathUri {
    PathUri::from_abs_path(path)
}

fn resource(environment_id: &str, path: &AbsolutePathBuf) -> PluginResourceLocator {
    PluginResourceLocator::Environment {
        environment_id: environment_id.to_string(),
        path: path_uri(path),
    }
}

#[test]
fn environment_descriptor_binds_every_manifest_resource() {
    let root = absolute(std::env::current_dir().expect("cwd").join("plugin-root"));
    let root_uri = path_uri(&root);
    let manifest_path = root.join(".codex-plugin/plugin.json");
    let skills = root.join("skills");
    let mcp_servers = root.join(".mcp.json");
    let apps = root.join(".app.json");
    let hooks = root.join("hooks/hooks.json");
    let composer_icon = root.join("assets/composer.svg");
    let logo = root.join("assets/logo.svg");
    let logo_dark = root.join("assets/logo-dark.svg");
    let screenshot = root.join("assets/screenshot.png");
    let manifest = PluginManifest {
        name: "demo".to_string(),
        version: None,
        description: None,
        keywords: Vec::new(),
        paths: PluginManifestPaths {
            skills: vec![path_uri(&skills)],
            mcp_servers: Some(PluginManifestMcpServers::Path(path_uri(&mcp_servers))),
            apps: Some(path_uri(&apps)),
            hooks: Some(PluginManifestHooks::Paths(vec![path_uri(&hooks)])),
        },
        interface: Some(PluginManifestInterface {
            composer_icon: Some(path_uri(&composer_icon)),
            logo: Some(path_uri(&logo)),
            logo_dark: Some(path_uri(&logo_dark)),
            screenshots: vec![path_uri(&screenshot)],
            ..PluginManifestInterface::default()
        }),
        tool_exposure: None,
    };

    let plugin = ResolvedPlugin::from_environment(
        "selected-demo".to_string(),
        "executor-1".to_string(),
        root_uri,
        path_uri(&manifest_path),
        manifest,
    )
    .expect("valid descriptor");

    assert_eq!(plugin.selected_root_id(), "selected-demo");
    assert_eq!(
        plugin.location(),
        &ResolvedPluginLocation::Environment {
            environment_id: "executor-1".to_string(),
            root: path_uri(&root),
        }
    );
    assert_eq!(
        plugin.manifest_path(),
        &resource("executor-1", &manifest_path)
    );
    assert_eq!(
        plugin.manifest(),
        &PluginManifest {
            name: "demo".to_string(),
            version: None,
            description: None,
            keywords: Vec::new(),
            paths: PluginManifestPaths {
                skills: vec![resource("executor-1", &skills)],
                mcp_servers: Some(PluginManifestMcpServers::Path(resource(
                    "executor-1",
                    &mcp_servers,
                ))),
                apps: Some(resource("executor-1", &apps)),
                hooks: Some(PluginManifestHooks::Paths(vec![resource(
                    "executor-1",
                    &hooks
                )])),
            },
            interface: Some(PluginManifestInterface {
                composer_icon: Some(resource("executor-1", &composer_icon)),
                logo: Some(resource("executor-1", &logo)),
                logo_dark: Some(resource("executor-1", &logo_dark)),
                screenshots: vec![resource("executor-1", &screenshot)],
                ..PluginManifestInterface::default()
            }),
            tool_exposure: None,
        }
    );
}

#[test]
fn environment_descriptor_rejects_resources_outside_package_root() {
    let cwd = std::env::current_dir().expect("cwd");
    let root = absolute(cwd.join("plugin-root"));
    let outside = absolute(cwd.join("plugin-root-other/.mcp.json"));
    for field in ["manifest", "skills", "mcp", "apps", "hooks", "composer", "logo", "logo_dark", "screenshots"] {
        let mut manifest_path = path_uri(&root.join(".codex-plugin/plugin.json"));
        let mut manifest = PluginManifest {
            name: "demo".to_string(),
            version: None,
            description: None,
            keywords: Vec::new(),
            paths: PluginManifestPaths {
                skills: vec![path_uri(&root.join("skills"))],
                mcp_servers: None,
                apps: None,
                hooks: None,
            },
            interface: Some(PluginManifestInterface::default()),
            tool_exposure: None,
        };
        let outside_uri = path_uri(&outside);
        let interface = manifest.interface.as_mut().unwrap();
        match field {
            "manifest" => manifest_path = outside_uri,
            "skills" => manifest.paths.skills.push(outside_uri),
            "mcp" => manifest.paths.mcp_servers = Some(PluginManifestMcpServers::Path(outside_uri)),
            "apps" => manifest.paths.apps = Some(outside_uri),
            "hooks" => manifest.paths.hooks = Some(PluginManifestHooks::Paths(vec![outside_uri])),
            "composer" => interface.composer_icon = Some(outside_uri),
            "logo" => interface.logo = Some(outside_uri),
            "logo_dark" => interface.logo_dark = Some(outside_uri),
            "screenshots" => interface.screenshots.push(outside_uri),
            _ => unreachable!(),
        }
        let err = ResolvedPlugin::from_environment(
            "selected-demo".to_string(),
            "executor-1".to_string(),
            path_uri(&root),
            manifest_path,
            manifest,
        )
        .expect_err("every resource must stay inside the package root");

        assert_eq!(
            err,
            ResolvedPluginError::ResourceOutsideRoot {
                root: path_uri(&root),
                path: path_uri(&outside),
            },
            "{field}"
        );
    }
}
