use crate::canonical::ContractError;
use crate::canonical::Sha256HexV1;
use crate::canonical::canonical_jcs_of;
use crate::canonical::validate_nfc;
use crate::path::StrictRepositoryPathV1;
use crate::selection::ActionIdV1;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TestRouteIdV1 {
    #[serde(rename = "test-route.rust-nextest.v1")]
    RustNextest,
    #[serde(rename = "test-route.rust-doctest.v1")]
    RustDoctest,
    #[serde(rename = "test-route.python-unittest.v1")]
    PythonUnittest,
    #[serde(rename = "test-route.python-pytest.v1")]
    PythonPytest,
    #[serde(rename = "test-route.javascript-jest.v1")]
    JavascriptJest,
    #[serde(rename = "test-route.argument-comment-lint-native.v1")]
    ArgumentCommentLintNative,
    #[serde(rename = "test-route.windows-sandbox-smoke-native.v1")]
    WindowsSandboxSmokeNative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestRunnerKindV1 {
    RustNextest,
    RustDoctest,
    PythonUnittest,
    PythonPytest,
    JavascriptJest,
    ArgumentCommentLintNative,
    WindowsSandboxSmokeNative,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RunnerSelectorV1 {
    RustNextest {
        cargo_target_context_spec_sha256: Sha256HexV1,
        harness_test_name: String,
        nextest_binary_id: String,
    },
    RustDoctest {
        cargo_target_context_spec_sha256: Sha256HexV1,
        declaration_ordinal: u64,
        harness_test_name: String,
        item_path: String,
        source_path: StrictRepositoryPathV1,
    },
    PythonUnittest {
        parent_test_id: String,
        selection_unit: PythonUnittestSelectionUnitV1,
        subtest_manifest_sha256: crate::canonical::Sha256HexV1,
    },
    PythonPytest {
        node_id: String,
    },
    JavascriptJest {
        config_path: StrictRepositoryPathV1,
        file_path: StrictRepositoryPathV1,
        ancestor_titles: Vec<String>,
        full_title: String,
        line: u64,
        column: u64,
        registration_ordinal: u64,
    },
    ArgumentCommentLintNative {
        case_id: String,
    },
    WindowsSandboxSmokeNative {
        case_id: String,
    },
    NonTestAction {
        action_id: ActionIdV1,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PythonUnittestSelectionUnitV1 {
    #[serde(rename = "parent-with-all-declared-subtests")]
    ParentWithAllDeclaredSubtests,
}

impl RunnerSelectorV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        canonical_jcs_of(self)?;
        match self {
            Self::RustNextest {
                harness_test_name,
                nextest_binary_id,
                ..
            } => {
                for value in [harness_test_name, nextest_binary_id] {
                    require_nonempty(value)?;
                }
                Ok(())
            }
            Self::RustDoctest {
                harness_test_name,
                item_path,
                ..
            } => {
                for value in [harness_test_name, item_path] {
                    require_nonempty(value)?;
                }
                Ok(())
            }
            Self::PythonUnittest { parent_test_id, .. } => require_nonempty(parent_test_id),
            Self::PythonPytest { node_id } => require_nonempty(node_id),
            Self::JavascriptJest {
                ancestor_titles,
                full_title,
                line,
                column,
                registration_ordinal,
                ..
            } => {
                for title in ancestor_titles {
                    require_nonempty(title)?;
                }
                require_nonempty(full_title)?;
                require_positive(*line, "line")?;
                require_positive(*column, "column")?;
                let _ = registration_ordinal;
                Ok(())
            }
            Self::ArgumentCommentLintNative { case_id }
            | Self::WindowsSandboxSmokeNative { case_id } => require_nonempty(case_id),
            Self::NonTestAction { .. } => Ok(()),
        }
    }
}

fn require_nonempty(value: &str) -> Result<(), ContractError> {
    validate_nfc(value)?;
    if value.is_empty() {
        Err(ContractError::InvalidContract(
            "selector field must be nonempty".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn require_positive(value: u64, name: &str) -> Result<(), ContractError> {
    if value == 0 {
        Err(ContractError::InvalidContract(format!(
            "{name} must be positive"
        )))
    } else {
        Ok(())
    }
}
