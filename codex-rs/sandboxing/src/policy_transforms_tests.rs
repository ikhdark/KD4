use super::effective_file_system_sandbox_policy;
use super::intersect_permission_profiles;
use super::intersect_uri_permission_profiles;
use super::merge_file_system_policy_with_additional_permissions;
use super::normalize_additional_permissions;
use super::should_require_platform_sandbox;
use codex_protocol::models::AdditionalPermissionProfile as PermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::NetworkPermissions;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::permissions::ReadDenyMatcher;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use dunce::canonicalize;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn intersect_uri_permission_profiles_materializes_grants_and_denies_for_reuse() {
    let cwd = PathUri::parse("file:///home/remote/project").expect("request cwd");
    let later_cwd = PathUri::parse("file:///home/remote/later").expect("later cwd");
    let permissions = UriAdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(Some("private".into())),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
    };
    let granted = intersect_uri_permission_profiles(permissions.clone(), permissions, &cwd);
    assert_eq!(
        granted.file_system.as_ref().unwrap().entries,
        vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: cwd.clone() },
                access: FileSystemAccessMode::Write
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: cwd.join("private").unwrap()
                },
                access: FileSystemAccessMode::Deny
            },
        ]
    );
    for (base, has_write) in [(&cwd, true), (&later_cwd, false)] {
        let request = UriAdditionalPermissionProfile {
            network: None,
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec![base.clone()]),
            )),
        };
        let reused = intersect_uri_permission_profiles(request, granted.clone(), base);
        assert_eq!(
            reused.file_system.as_ref().is_some_and(|fs| fs
                .entries
                .iter()
                .any(|entry| entry.access == FileSystemAccessMode::Write)),
            has_write
        );
    }
}

#[test]
fn intersect_uri_permission_profiles_resolves_project_roots_in_foreign_cwd() {
    let cwd = PathUri::parse("file:///home/remote/project").expect("foreign cwd URI");
    let child = cwd.join("generated").expect("child URI");
    let requested = UriAdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
    };
    let granted = UriAdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![child.clone()]),
        )),
    };

    assert_eq!(
        intersect_uri_permission_profiles(requested, granted, &cwd),
        UriAdditionalPermissionProfile {
            network: None,
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![child]),
            )),
        }
    );
}

#[test]
fn full_access_restricted_policy_skips_platform_sandbox_when_network_is_enabled() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    }]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Enabled,
            /*has_managed_network_requirements*/ false
        ),
        false
    );
}

#[test]
fn root_write_policy_with_carveouts_still_uses_platform_sandbox() {
    let blocked = AbsolutePathBuf::resolve_path_against_base(
        "blocked",
        std::env::current_dir().expect("current dir"),
    );
    let policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: blocked },
            access: FileSystemAccessMode::Deny,
        },
    ]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Enabled,
            /*has_managed_network_requirements*/ false
        ),
        true
    );
}

#[test]
fn full_access_restricted_policy_still_uses_platform_sandbox_for_restricted_network() {
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    }]);

    assert_eq!(
        should_require_platform_sandbox(
            &policy,
            NetworkSandboxPolicy::Restricted,
            /*has_managed_network_requirements*/ false
        ),
        true
    );
}

#[test]
fn normalize_additional_permissions_preserves_network() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let permissions = normalize_additional_permissions(PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(vec![path.clone()]),
        )),
    })
    .expect("permissions");

    assert_eq!(
        permissions.network,
        Some(NetworkPermissions {
            enabled: Some(true),
        })
    );
    assert_eq!(
        permissions.file_system,
        Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(vec![path]),
        ))
    );
}

#[test]
fn normalize_additional_permissions_rejects_glob_read_grants() {
    let err = normalize_additional_permissions(PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: "**/*.env".to_string(),
                },
                access: FileSystemAccessMode::Read,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    })
    .expect_err("read glob permissions are unsupported");

    assert_eq!(
        err,
        "glob file system permissions only support deny-read entries"
    );
}

#[test]
fn normalize_additional_permissions_preserves_deny_globs() {
    let permissions = normalize_additional_permissions(PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: "**/*.env".to_string(),
                },
                access: FileSystemAccessMode::Deny,
            }],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    })
    .expect("deny glob permissions are supported");

    assert_eq!(
        permissions,
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: "**/*.env".to_string(),
                    },
                    access: FileSystemAccessMode::Deny,
                }],
                glob_scan_max_depth: std::num::NonZeroUsize::new(2),
            }),
            ..Default::default()
        }
    );
}

#[test]
fn normalize_additional_permissions_drops_empty_nested_profiles() {
    let permissions = normalize_additional_permissions(PermissionProfile {
        network: Some(NetworkPermissions { enabled: None }),
        file_system: Some(FileSystemPermissions::default()),
    })
    .expect("permissions");

    assert_eq!(permissions, PermissionProfile::default());
}

#[test]
fn intersect_permission_profiles_preserves_explicit_empty_requested_reads() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![path]),
        )),
        ..Default::default()
    };
    let granted = requested.clone();

    assert_eq!(
        intersect_permission_profiles(requested.clone(), granted, temp_dir.path()),
        requested
    );
}

#[test]
fn intersect_permission_profiles_drops_ungranted_nonempty_path_requests() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path]),
            /*write*/ None,
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, PermissionProfile::default(), temp_dir.path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_explicit_empty_reads_without_grant() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let path = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![path]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, PermissionProfile::default(), temp_dir.path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_accepts_child_path_granted_for_requested_cwd() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let child = cwd.join("child");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![child]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted.clone(), cwd.as_path()),
        granted
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_grant_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let later_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("later-cwd"))
        .expect("absolute later cwd");
    let cwd_write_permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    let intersected = intersect_permission_profiles(
        cwd_write_permissions.clone(),
        cwd_write_permissions,
        request_cwd.as_path(),
    );

    assert_eq!(
        intersected,
        PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![request_cwd]),
            )),
            ..Default::default()
        }
    );
    assert_eq!(
        intersect_permission_profiles(
            PermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_roots(
                    /*read*/ None,
                    Some(vec![later_cwd.join("child")]),
                )),
                ..Default::default()
            },
            intersected,
            later_cwd.as_path(),
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_deduplicates_materialized_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd =
        AbsolutePathBuf::from_absolute_path(temp_dir.path().join("cwd")).expect("absolute cwd");
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path: cwd.clone() },
                    access: FileSystemAccessMode::Write,
                },
            ],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(permissions.clone(), permissions, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![cwd]),
            )),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_materializes_cwd_deny_entries() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(permissions.clone(), permissions, request_cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Write,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Path { path: request_cwd },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_drops_deny_entries_without_filesystem_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let secret = cwd.join("secret");
    let requested = PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path: secret },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
    };
    let granted = PermissionProfile {
        network: Some(NetworkPermissions {
            enabled: Some(true),
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted.clone(), cwd.as_path()),
        granted
    );
}

#[test]
fn intersect_permission_profiles_rejects_concrete_grants_matched_by_requested_deny_globs() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let env_file = cwd.join(".env");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: "**/*.env".to_string(),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![env_file]),
        )),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_keeps_brace_deny_globs_that_constrain_grants() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let secrets = cwd.join("secrets");
    // The alternation precedes the first wildcard, so the deny's literal prefix is `cwd`.
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: secrets.clone(),
                    },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: cwd
                            .join("{secrets,keys}")
                            .join("*.pem")
                            .to_string_lossy()
                            .into_owned(),
                    },
                    access: FileSystemAccessMode::Deny,
                },
            ],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    let intersected =
        intersect_permission_profiles(permissions.clone(), permissions.clone(), cwd.as_path());

    assert_eq!(intersected, permissions);
    let policy =
        FileSystemSandboxPolicy::restricted(intersected.file_system.unwrap_or_default().entries);
    assert!(
        ReadDenyMatcher::new(&policy, cwd.as_path())
            .is_some_and(|matcher| matcher.is_read_denied(secrets.join("id.pem").as_path()))
    );
}

#[test]
fn intersect_permission_profiles_materializes_relative_deny_globs_for_reuse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let request_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("request-cwd"))
        .expect("absolute request cwd");
    let later_cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("later-cwd"))
        .expect("absolute later cwd");
    let cwd_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![cwd_write, deny_env_files],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };

    let intersected =
        intersect_permission_profiles(permissions.clone(), permissions, request_cwd.as_path());

    assert_eq!(
        intersected,
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Path {
                            path: request_cwd.clone(),
                        },
                        access: FileSystemAccessMode::Write,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: request_cwd.join("**/*.env").to_string_lossy().into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: std::num::NonZeroUsize::new(2),
            }),
            ..Default::default()
        }
    );
    assert_eq!(
        intersect_permission_profiles(
            PermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_roots(
                    /*read*/ None,
                    Some(vec![later_cwd.join("token.env")]),
                )),
                ..Default::default()
            },
            intersected,
            later_cwd.as_path(),
        ),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_drops_broader_cwd_grant_for_requested_child_path() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let child = cwd.join("child");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            /*read*/ None,
            Some(vec![child]),
        )),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                },
                access: FileSystemAccessMode::Write,
            }],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile::default()
    );
}

#[test]
fn intersect_permission_profiles_uses_granted_bounded_glob_scan_depth() {
    let cwd = std::env::current_dir().expect("current dir");
    let root_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files],
            glob_scan_max_depth: std::num::NonZeroUsize::new(4),
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    root_write,
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: AbsolutePathBuf::resolve_path_against_base(
                                "**/*.env",
                                cwd.as_path()
                            )
                            .to_string_lossy()
                            .into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: std::num::NonZeroUsize::new(4),
            }),
            ..Default::default()
        }
    );
}

#[test]
fn intersect_permission_profiles_uses_granted_unbounded_glob_scan_depth() {
    let cwd = std::env::current_dir().expect("current dir");
    let root_write = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        },
        access: FileSystemAccessMode::Write,
    };
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        }),
        ..Default::default()
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![root_write.clone(), deny_env_files],
            glob_scan_max_depth: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path()),
        PermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    root_write,
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: AbsolutePathBuf::resolve_path_against_base(
                                "**/*.env",
                                cwd.as_path()
                            )
                            .to_string_lossy()
                            .into_owned(),
                        },
                        access: FileSystemAccessMode::Deny,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        }
    );
}

#[test]
fn merge_file_system_policy_with_additional_permissions_preserves_unreadable_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: denied_path.clone(),
                },
                access: FileSystemAccessMode::Deny,
            },
        ]),
        &FileSystemPermissions::from_read_write_roots(
            Some(vec![allowed_path.clone()]),
            Some(Vec::new()),
        ),
    );

    assert!(!merged_policy.can_read_path_with_cwd(denied_path.as_path(), cwd.as_path()));
    assert!(merged_policy.can_read_path_with_cwd(allowed_path.as_path(), cwd.as_path()));
    assert!(!merged_policy.can_write_path_with_cwd(allowed_path.as_path(), cwd.as_path()));
    assert_eq!(
        merged_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        }),
        true
    );
    assert_eq!(
        merged_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: allowed_path },
            access: FileSystemAccessMode::Read,
        }),
        true
    );
}

#[test]
fn merge_file_system_policy_with_additional_permissions_carries_bounded_glob_scan_depth() {
    let deny_env_files = FileSystemSandboxEntry {
        path: FileSystemPath::GlobPattern {
            pattern: "**/*.env".to_string(),
        },
        access: FileSystemAccessMode::Deny,
    };
    let merged_policy = merge_file_system_policy_with_additional_permissions(
        &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Write,
        }]),
        &FileSystemPermissions {
            entries: vec![deny_env_files.clone()],
            glob_scan_max_depth: std::num::NonZeroUsize::new(2),
        },
    );

    assert_eq!(merged_policy, {
        let mut policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Write,
            },
            deny_env_files,
        ]);
        policy.glob_scan_max_depth = Some(2);
        policy
    });
}

#[test]
fn effective_file_system_sandbox_policy_returns_base_policy_without_additional_permissions() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        },
    ]);

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, /*additional_permissions*/ None);

    assert_eq!(effective_policy, base_policy);
}

#[test]
fn effective_file_system_sandbox_policy_merges_additional_write_roots() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(
        canonicalize(temp_dir.path()).expect("canonicalize temp dir"),
    )
    .expect("absolute temp dir");
    let allowed_path = cwd.join("allowed");
    let denied_path = cwd.join("denied");
    let base_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: denied_path.clone(),
            },
            access: FileSystemAccessMode::Deny,
        },
    ]);
    let additional_permissions = PermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![allowed_path.clone()]),
        )),
        ..Default::default()
    };

    let effective_policy =
        effective_file_system_sandbox_policy(&base_policy, Some(&additional_permissions));

    assert!(!effective_policy.can_read_path_with_cwd(denied_path.as_path(), cwd.as_path()));
    assert!(effective_policy.can_write_path_with_cwd(allowed_path.as_path(), cwd.as_path()));
    assert_eq!(
        effective_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: denied_path },
            access: FileSystemAccessMode::Deny,
        }),
        true
    );
    assert_eq!(
        effective_policy.entries.contains(&FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: allowed_path },
            access: FileSystemAccessMode::Write,
        }),
        true
    );
}

#[test]
fn intersections_preserve_read_carveouts_and_reject_writes_inside_them() {
    let dir = TempDir::new().unwrap();
    let cwd = AbsolutePathBuf::from_absolute_path(canonicalize(dir.path()).unwrap()).unwrap();
    let protected = cwd.join("protected");
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path: cwd.clone() },
                    access: FileSystemAccessMode::Write,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: protected.clone(),
                    },
                    access: FileSystemAccessMode::Read,
                },
            ],
            glob_scan_max_depth: None,
        }),
        network: None,
    };
    for grant_path in [cwd.clone(), protected.join("child")] {
        let grant = PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec![grant_path.clone()]),
            )),
            network: None,
        };
        let native = intersect_permission_profiles(requested.clone(), grant.clone(), cwd.as_path());
        let uri = intersect_uri_permission_profiles(
            requested.clone().into(),
            grant.into(),
            &PathUri::from_abs_path(&cwd),
        );
        let uri_native: PermissionProfile = uri.try_into().expect("native URI result");
        for result in [native, uri_native] {
            let policy =
                FileSystemSandboxPolicy::restricted(result.file_system.unwrap_or_default().entries);
            assert_eq!(
                policy.can_write_path_with_cwd(cwd.join("allowed").as_path(), cwd.as_path()),
                grant_path == cwd
            );
            assert!(
                !policy.can_write_path_with_cwd(protected.join("child").as_path(), cwd.as_path())
            );
            assert_eq!(
                policy.can_read_path_with_cwd(protected.join("child").as_path(), cwd.as_path()),
                grant_path == cwd
            );
        }
    }
}

#[test]
fn uri_intersection_accepts_symbolic_grant_for_concrete_request() {
    for (cwd, requested_root) in [
        ("file:///project", "file:///project"),
        ("file:///C:/project", "file:///C:/project"),
        ("file:///project", "file:///project/"),
        ("file:///C:/project", "file:///C:/project/"),
        ("file:///project", "file:///pro%6Aect"),
    ] {
        let cwd = PathUri::parse(cwd).unwrap();
        let requested = UriAdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec![PathUri::parse(requested_root).unwrap()]),
            )),
            network: None,
        };
        let granted = UriAdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(None),
                    },
                    access: FileSystemAccessMode::Write,
                }],
                glob_scan_max_depth: None,
            }),
            network: None,
        };
        assert_eq!(
            intersect_uri_permission_profiles(requested, granted, &cwd),
            UriAdditionalPermissionProfile {
                file_system: Some(FileSystemPermissions::from_read_write_roots(
                    None,
                    Some(vec![cwd.clone()]),
                )),
                network: None,
            }
        );
    }
}

#[test]
fn uri_intersection_rejects_grant_when_deny_glob_cannot_be_bound() {
    let cwd = PathUri::parse("file:///C:/project").unwrap();
    let requested = UriAdditionalPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            None,
            Some(vec![cwd.clone()]),
        )),
        network: None,
    };
    let mut granted = requested.clone();
    granted
        .file_system
        .as_mut()
        .unwrap()
        .entries
        .push(FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                // Drive-relative patterns have no stable meaning without per-drive cwd state.
                pattern: "D:*.env".into(),
            },
            access: FileSystemAccessMode::Deny,
        });
    assert_eq!(
        intersect_uri_permission_profiles(requested, granted, &cwd).file_system,
        None
    );
}

#[test]
fn uri_intersection_binds_granted_deny_globs_using_target_convention() {
    for (cwd, expected) in [
        ("file:///project-a", "/project-a/**/*.env"),
        ("file:///C:/project-a", r"C:\project-a\**\*.env"),
    ] {
        let cwd = PathUri::parse(cwd).unwrap();
        let requested = UriAdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                None,
                Some(vec![cwd.clone()]),
            )),
            network: None,
        };
        let mut grant = requested.clone();
        grant
            .file_system
            .as_mut()
            .unwrap()
            .entries
            .push(FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: "**/*.env".into(),
                },
                access: FileSystemAccessMode::Deny,
            });
        let stored = intersect_uri_permission_profiles(requested.clone(), grant, &cwd);
        let fs = stored.file_system.as_ref().unwrap();
        assert_eq!(
            fs.entries[0],
            requested.file_system.as_ref().unwrap().entries[0]
        );
        assert_eq!(
            fs.entries[1],
            FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: expected.into()
                },
                access: FileSystemAccessMode::Deny,
            }
        );
        let later = cwd.join("../project-b").unwrap();
        assert_eq!(
            intersect_uri_permission_profiles(requested, stored.clone(), &later),
            stored
        );
    }
}

#[test]
fn unresolved_grants_keep_deny_constraints() {
    let dir = TempDir::new().unwrap();
    let cwd = AbsolutePathBuf::from_absolute_path(dir.path()).unwrap();
    let allow = FileSystemSandboxEntry {
        path: FileSystemPath::Special {
            value: FileSystemSpecialPath::Minimal,
        },
        access: FileSystemAccessMode::Read,
    };
    let deny = FileSystemSandboxEntry {
        path: FileSystemPath::Path {
            path: cwd.join("blocked"),
        },
        access: FileSystemAccessMode::Deny,
    };
    let requested = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![allow.clone(), deny.clone()],
            glob_scan_max_depth: None,
        }),
        network: None,
    };
    let granted = PermissionProfile {
        file_system: Some(FileSystemPermissions {
            entries: vec![allow.clone()],
            glob_scan_max_depth: None,
        }),
        network: None,
    };
    assert_eq!(
        intersect_permission_profiles(requested, granted, cwd.as_path())
            .file_system
            .unwrap()
            .entries,
        vec![allow, deny]
    );
}
