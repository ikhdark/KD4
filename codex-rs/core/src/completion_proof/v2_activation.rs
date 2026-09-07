//! Privately authorized selection of a complete Inventory V2 generation.
//! The config replacement is the commit point. A prepared private record accepts
//! either the unchanged predecessor config or its exact successor, so interruption
//! between private persistence and the config replacement never exposes half a bundle.
use super::*;
use codex_validation_contracts::canonical::MustBeNullV1;
use codex_validation_contracts::canonical::proof_hash;
use codex_validation_contracts::inventory_v2::*;
use codex_validation_contracts::selection::ExecutableIdentityV1;

pub(super) const INVENTORY: &str = "frozen-test-inventory-v2.json";
const LEDGER: &str = "test-replacements-v2.json";
const MEMBERS: &[&str] = &[
    INVENTORY,
    LEDGER,
    "frozen-test-inventory-v2-recoveries.json",
    "frozen-test-inventory-v2-recovery-transition-receipts.json",
    "frozen-test-inventory-v2-doctest-recapture.json",
    "frozen-test-inventory-v2-unittest-recapture.json",
    "frozen-test-inventory-v2-unittest-source-exceptions.json",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistedInventoryActivation {
    predecessor_config: String,
    selected_config: String,
    predecessor_bundle_sha256: String,
    generation_path: String,
    member_sha256: BTreeMap<String, String>,
    workspace_head_identity: Option<String>,
    workspace_path_fingerprints: BTreeMap<String, String>,
    approval: PersistedCurrentEvidenceCatalogV1,
}

pub(super) fn is_v2(config: &RepositoryCompletionProofConfig) -> bool {
    Path::new(&config.frozen_inventory_path)
        .file_name()
        .and_then(|name| name.to_str())
        == Some(INVENTORY)
}

fn regular_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(component) = component else {
            return Err("inventory generation paths must stay inside the repository".to_string());
        };
        path.push(component);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(format!(
                        "inventory generation path is a reparse point: {}",
                        path.display()
                    ));
                }
            }
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "inventory generation path is a symlink: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(path)
}

fn read_regular(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let path = regular_path(root, relative)?;
    if !path.is_file() {
        return Err(format!(
            "inventory member is not a regular file: {relative}"
        ));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

struct Bundle {
    raw: BTreeMap<String, Vec<u8>>,
    inventory: FrozenTestInventoryV2,
    ledger: TestReplacementLedgerV2,
}

// This capability has no wire format. The production constructor is below the
// private-state, context, quality and sealed-review checks in activate_inventory_v2.
struct HistoricalAdmissionCapability {
    catalog: FocusedLiveSuccessorCatalogV1,
    approval: FocusedReplacementApprovalReceiptV1,
    proposal: HistoricalReplacementAcceptanceProposalV1,
    sealed_review_sha256: String,
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// The immutable V1 replacement ledger compiled into this runtime. Activation
/// and closure validation both bind to these exact bytes, never to the editable
/// working-tree copy.
fn compiled_predecessor_ledger() -> Result<&'static [u8], String> {
    KD4_TRUSTED_BUNDLE_MEMBERS
        .iter()
        .find(|member| member.relative_path == REPLACEMENT_LEDGER_RELATIVE_PATH)
        .map(|member| member.bytes)
        .ok_or_else(|| "missing immutable ledger".to_string())
}

fn load_bundle(root: &Path, directory: &str) -> Result<Bundle, String> {
    let raw = MEMBERS
        .iter()
        .map(|name| {
            Ok((
                name.to_string(),
                read_regular(root, &format!("{directory}/{name}"))?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let inventory = parse(&raw[INVENTORY])?;
    let ledger = parse(&raw[LEDGER])?;
    let bundle = Bundle {
        raw,
        inventory,
        ledger,
    };
    validate_bundle(&bundle)?;
    Ok(bundle)
}

fn validate_bundle(bundle: &Bundle) -> Result<(), String> {
    let transitions: Vec<codex_validation_contracts::recovery::RecoveryTransitionReceiptV1> =
        parse(&bundle.raw["frozen-test-inventory-v2-recovery-transition-receipts.json"])?;
    // No public or dormant applicability receipt can be promoted by this path.
    let mut key = [0_u8; 32];
    rand::rng().fill_bytes(&mut key);
    let issuer = codex_validation_contracts::applicability::ActiveHostApplicabilityIssuerV1::new(
        Uuid::new_v4().to_string(),
        key,
        Vec::new(),
    )
    .map_err(|e| e.to_string())?;
    let predecessor = compiled_predecessor_ledger()?;
    validate_inventory_ledger_predecessor_closure_with_recaptures(
        &bundle.inventory,
        &bundle.ledger,
        &bundle.raw["frozen-test-inventory-v2-recoveries.json"],
        &transitions,
        &issuer,
        Some(&bundle.raw["frozen-test-inventory-v2-doctest-recapture.json"]),
        Some(&bundle.raw["frozen-test-inventory-v2-unittest-recapture.json"]),
        Some(predecessor),
    )
    .map_err(|e| e.to_string())
}

fn accept_historical(
    bundle: &mut Bundle,
    capability: &HistoricalAdmissionCapability,
) -> Result<(), String> {
    validate_historical_proposal_against_approval(&capability.proposal, &capability.approval)?;
    let predecessor: serde_json::Value = parse(compiled_predecessor_ledger()?)?;
    // Keyed once: the frozen ledger has 15,544 rows and this loop visits 644.
    let rows = predecessor["rows"]
        .as_array()
        .ok_or("missing predecessor rows")?
        .iter()
        .filter_map(|old| Some((old["baseline_id"].as_str()?, old)))
        .collect::<BTreeMap<_, _>>();
    let mut accepted_count = 0;
    for row in &mut bundle.ledger.rows {
        let ReplacementLedgerDispositionV2::Replacement {
            contract,
            stage2_incorrect_behavior_ids,
            ..
        } = &mut row.disposition
        else {
            continue;
        };
        let ReplacementContractDispositionV1::PendingReview {
            legacy_replacement_hint,
            ..
        } = contract
        else {
            return Err("activation requires the unchanged dormant historical ledger".to_string());
        };
        let predecessor_row = row
            .baseline_id
            .as_deref()
            .and_then(|baseline_id| rows.get(baseline_id).copied())
            .ok_or("historical predecessor row is missing")?;
        let mut entries = Vec::new();
        for id in &legacy_replacement_hint.replacement_ids {
            let entry = capability.catalog.resolved_successor_entries.iter().find(|entry| {
                matches!(entry.identity(), ExecutableIdentityV1::Test { test_id, .. } if test_id.as_str() == id)
            }).ok_or_else(|| format!("private current catalog is missing historical successor {id}"))?;
            entries.push(entry.clone());
        }
        let entry = entries
            .first()
            .ok_or("historical mapping is empty")?
            .inventory_entry
            .as_ref();
        let candidate = CandidateReplacementContractV1 {
            candidate_receipt_sha256: capability.approval.receipt_sha256.clone(),
            executable_identity: entry.executable_identity.clone(),
            executable_identity_sha256: entry.executable_identity_sha256.clone(),
            execution_input_contract_sha256: entry.execution_input_contract_sha256.clone(),
            platform_applicability_sha256: entry.platform_applicability_sha256.clone(),
            replacement_id: legacy_replacement_hint.replacement_ids[0].clone(),
            runner_selector: entry.runner_selector.clone(),
            runner_selector_sha256: entry.runner_selector_sha256.clone(),
            test_route_id: entry
                .test_route_id
                .ok_or("historical successor has no test route")?,
            validation_id: entry.validation_id.clone(),
        };
        // The primary selector remains compatible with the V2 contract. These
        // hashes and the preserved edges bind ALL successors, including fan-out.
        let accepted = AcceptedReplacementContractV1 {
            accepted_receipt_sha256: Sha256HexV1::parse(capability.sealed_review_sha256.clone())
                .map_err(|e| e.to_string())?,
            candidate,
            contract_sources_sha256: proof_hash("kd4.historical-successor-contracts.v1", &entries)
                .map_err(|e| e.to_string())?,
            product_behavior_obligation_sha256: proof_hash(
                "kd4.historical-product-obligation.v1",
                &predecessor_row["preserved_behavior"],
            )
            .map_err(|e| e.to_string())?,
            runtime_path_sha256: proof_hash(
                "kd4.historical-product-path.v1",
                &predecessor_row["product_path"],
            )
            .map_err(|e| e.to_string())?,
            resolved_entry_set_sha256: None,
            selection_v1_sha256: None,
            trusted_defect_receipt_sha256s: None,
        };
        *contract = ReplacementContractDispositionV1::Accepted {
            accepted,
            candidate: MustBeNullV1,
            legacy_replacement_hint: legacy_replacement_hint.clone(),
        };
        *stage2_incorrect_behavior_ids = Stage2IncorrectBehaviorIdsV1::Reviewed(Vec::new());
        accepted_count += 1;
    }
    if accepted_count != HistoricalReplacementAcceptanceProposalV1::BASELINE_COUNT as usize {
        return Err("activation did not accept exactly the frozen historical graph".to_string());
    }
    let mut value = serde_json::to_value(&bundle.ledger).map_err(|e| e.to_string())?;
    let object = value.as_object_mut().ok_or("ledger is not an object")?;
    object.remove("semantic_sha256");
    object.remove("self_hash");
    let semantic = proof_hash(TestReplacementLedgerV2::SEMANTIC_HASH_DOMAIN, &value)
        .map_err(|e| e.to_string())?;
    value["semantic_sha256"] = serde_json::json!(semantic);
    let self_hash =
        proof_hash(TestReplacementLedgerV2::SELF_HASH_DOMAIN, &value).map_err(|e| e.to_string())?;
    value["self_hash"] = serde_json::json!(self_hash);
    bundle.ledger = serde_json::from_value(value).map_err(|e| e.to_string())?;
    bundle.ledger.validate().map_err(|e| e.to_string())?;
    bundle.raw.insert(
        LEDGER.to_string(),
        canonical_jcs_of(&bundle.ledger).map_err(|e| e.to_string())?,
    );
    Ok(())
}

impl CompletionProofLedger {
    pub(crate) async fn activate_inventory_v2(&self) -> Result<String, String> {
        if !self.is_terminal_owner() {
            return Err("only the root may activate Inventory V2".to_string());
        }
        let _workspace_operation = self.acquire_workspace_operation().await;
        let _operation = self.operation.lock().await;
        let _file_lock = acquire_private_state_lock(self.persistence.lock_path.clone())
            .await
            .map_err(|e| e.to_string())?;
        self.refresh_persistent_state_from_disk().await?;
        let authority = self.verified_authority().await?;
        if is_v2(&authority.config) {
            return Ok("Inventory V2 is already active; no files changed".to_string());
        }
        if !self.requires_compiled_kd4_authority {
            return Err("Inventory V2 activation requires the compiled KD4 authority".to_string());
        }
        // A prepared authorization survives workspace reconciliation on restart.
        // Recheck its exact original inputs and staged bytes before replaying the
        // single commit; no public receipt or replacement catalog is accepted.
        let prepared = self
            .state
            .lock()
            .await
            .persistent
            .inventory_activation
            .clone();
        if let Some(prepared) = prepared {
            match self.verify_prepared_activation(&prepared).await {
                Ok(config_path) => {
                    // A failed replacement keeps the authenticated preparation for
                    // the next attempt; the predecessor selection stays in force.
                    write_private_state_blocking(&config_path, prepared.selected_config.as_bytes())
                        .map_err(|e| e.to_string())?;
                    return Ok("Inventory V2 activation resumed from its authenticated preparation; unresolved obligations still block final certification".to_string());
                }
                // The preparation's bound inputs drifted before its single commit,
                // so it can never replay. While the configuration is still the exact
                // predecessor nothing was granted; discard the stale record and
                // derive a fresh preparation from current evidence below. Once the
                // selection changed, the record is the only V2 authority and stays.
                Err(error) => {
                    if read_regular(&self.repository_root, CONFIG_RELATIVE_PATH)?
                        != prepared.predecessor_config.as_bytes()
                    {
                        return Err(error);
                    }
                    self.state.lock().await.persistent.inventory_activation = None;
                    self.persist().await?;
                }
            }
        }
        let observation = workspace_observation(&self.repository_root)
            .await
            .ok_or("cannot observe activation workspace")?;
        let fingerprint = observation.fingerprint.clone();
        let catalog = {
            let mut state = self.state.lock().await;
            reconcile_external_workspace_change(
                &self.repository_root,
                &mut state.persistent,
                Some(observation),
            )
            .await;
            state
                .persistent
                .focused_completion
                .check_quality(
                    &self.repository_root,
                    &authority.policy_runner_bundle_sha256,
                )
                .await?;
            let catalog = state
                .persistent
                .current_evidence_catalog
                .as_ref()
                .ok_or("activation requires current private inventory evidence")?;
            let approval = catalog
                .approval_receipt
                .as_ref()
                .ok_or("activation requires a private focused approval")?;
            let review = approval
                .reviewed_proposal
                .as_ref()
                .ok_or("activation requires a sealed independent historical review")?;
            if self.session_lineage_id.as_deref() != Some(catalog.session_lineage_id.as_str())
                || review.session_lineage_id != catalog.session_lineage_id
                || catalog.workspace_fingerprint != fingerprint
                || catalog.mutation_epoch != state.persistent.mutation_epoch
                || catalog.policy_runner_bundle_sha256 != authority.policy_runner_bundle_sha256
            {
                return Err(
                    "activation approval is stale or belongs to another context".to_string()
                );
            }
            let context = focused_replacement_approval_context(
                catalog,
                &approval.attempt_id,
                &approval.policy_id,
                &approval.receipt.focused_validation_id,
            )?;
            approval
                .receipt
                .validate_current_context(&context)
                .map_err(|e| e.to_string())?;
            if sealed_historical_review_hash(&review.sealed_review)? != review.sealed_review_sha256
            {
                return Err(
                    "private historical review does not match its sealed receipt".to_string(),
                );
            }
            catalog.clone()
        };
        let root = self.repository_root.clone();
        let approval = catalog
            .approval_receipt
            .as_ref()
            .ok_or("missing checked approval")?;
        let review = approval
            .reviewed_proposal
            .as_ref()
            .ok_or("missing checked review")?;
        let capability = HistoricalAdmissionCapability {
            catalog: catalog.catalog.clone(),
            approval: approval.receipt.clone(),
            proposal: review.proposal.clone(),
            sealed_review_sha256: review.sealed_review_sha256.clone(),
        };
        let bundle = tokio::task::spawn_blocking(move || {
            let mut bundle = load_bundle(&root, ".codex/validation")?;
            accept_historical(&mut bundle, &capability)?;
            validate_bundle(&bundle)?;
            Ok::<_, String>(bundle)
        })
        .await
        .map_err(|e| e.to_string())??;
        // Bind the record to the compiled predecessor, and require the working
        // tree to match it right now; a racing edit must not be baked in.
        let compiled_config = KD4_TRUSTED_BUNDLE_MEMBERS
            .iter()
            .find(|member| member.relative_path == CONFIG_RELATIVE_PATH)
            .ok_or("missing compiled configuration")?
            .bytes;
        if read_regular(&self.repository_root, CONFIG_RELATIVE_PATH)? != compiled_config {
            return Err("configuration differs from the compiled KD4 policy".to_string());
        }
        let predecessor_config =
            String::from_utf8(compiled_config.to_vec()).map_err(|e| e.to_string())?;
        let member_sha256 = bundle
            .raw
            .iter()
            .map(|(name, bytes)| (name.clone(), format!("{:x}", Sha256::digest(bytes))))
            .collect::<BTreeMap<_, _>>();
        let generation_hash = proof_hash("kd4.inventory-v2-generation.v1", &member_sha256)
            .map_err(|e| e.to_string())?;
        let generation_path = format!(".codex/validation/inventory-generations/{generation_hash}");
        let mut selected: toml_edit::DocumentMut = predecessor_config
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| e.to_string())?;
        selected["frozen_inventory"] = toml_edit::value(format!("{generation_path}/{INVENTORY}"));
        selected["replacement_ledger"] = toml_edit::value(format!("{generation_path}/{LEDGER}"));
        let selected_config = selected.to_string();
        stage_generation(&self.repository_root, &generation_path, &bundle.raw)?;
        // Staging is still invisible to policy readers. Ignore only this exact
        // newly staged generation when checking for concurrent workspace edits.
        let after = workspace_observation(&self.repository_root)
            .await
            .ok_or("cannot recheck activation workspace")?;
        if read_regular(&self.repository_root, CONFIG_RELATIVE_PATH)?
            != predecessor_config.as_bytes()
        {
            return Err("configuration changed during activation preparation".to_string());
        }
        // Recheck all inputs covered by the catalog, excluding only our output.
        let current_paths = after.path_fingerprints.clone();
        let (baseline_paths, baseline_head) = {
            let state = self.state.lock().await;
            (
                state.persistent.last_observed_path_fingerprints.clone(),
                state.persistent.last_observed_head_identity.clone(),
            )
        };
        let staged_prefix = format!("{generation_path}/");
        let unchanged = after.head_identity == baseline_head
            && current_paths
                .iter()
                .filter(|(p, _)| !p.starts_with(&staged_prefix))
                .eq(baseline_paths
                    .iter()
                    .filter(|(p, _)| !p.starts_with(&staged_prefix)));
        if !unchanged {
            return Err("workspace changed during activation staging".to_string());
        }
        let activation = PersistedInventoryActivation {
            predecessor_config,
            selected_config: selected_config.clone(),
            predecessor_bundle_sha256: authority.policy_runner_bundle_sha256,
            generation_path,
            member_sha256,
            workspace_head_identity: after.head_identity,
            workspace_path_fingerprints: baseline_paths,
            approval: catalog,
        };
        self.state.lock().await.persistent.inventory_activation = Some(activation.clone());
        // Authorize BOTH exact configurations durably before the sole commit point.
        // Failure here preserves the predecessor config; a replay never grants V2.
        self.persist().await?;
        self.commit_prepared_activation(&activation).await?;
        // The staged generation stays on disk; the config now selects it.
        Ok(format!(
            "Inventory V2 activated with {} reviewed historical mappings; unresolved obligations still block final certification",
            HistoricalReplacementAcceptanceProposalV1::BASELINE_COUNT
        ))
    }

    async fn commit_prepared_activation(
        &self,
        record: &PersistedInventoryActivation,
    ) -> Result<(), String> {
        let config_path = self.verify_prepared_activation(record).await?;
        write_private_state_blocking(&config_path, record.selected_config.as_bytes())
            .map_err(|e| e.to_string())
    }

    /// Rechecks every input the preparation was bound to and returns the
    /// configuration path whose replacement is the single commit point.
    async fn verify_prepared_activation(
        &self,
        record: &PersistedInventoryActivation,
    ) -> Result<PathBuf, String> {
        let authority = load_completion_proof_authority(
            &self.repository_root,
            self.requires_compiled_kd4_authority,
        )
        .await?;
        if authority.policy_runner_bundle_sha256 != record.predecessor_bundle_sha256 {
            return Err("prepared activation policy inputs changed".to_string());
        }
        verify_generation(&self.repository_root, record)?;
        let observation = workspace_observation(&self.repository_root)
            .await
            .ok_or("cannot observe prepared activation inputs")?;
        let outputs = record
            .member_sha256
            .keys()
            .map(|name| format!("{}/{name}", record.generation_path))
            .collect::<BTreeSet<_>>();
        if observation.head_identity != record.workspace_head_identity
            || !observation
                .path_fingerprints
                .iter()
                .filter(|(path, _)| !outputs.contains(*path))
                .eq(record
                    .workspace_path_fingerprints
                    .iter()
                    .filter(|(path, _)| !outputs.contains(*path)))
            || read_regular(&self.repository_root, CONFIG_RELATIVE_PATH)?
                != record.predecessor_config.as_bytes()
        {
            return Err(
                "workspace changed since the authenticated activation preparation".to_string(),
            );
        }
        regular_path(&self.repository_root, CONFIG_RELATIVE_PATH)
    }
}

fn stage_generation(
    root: &Path,
    relative: &str,
    members: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf, String> {
    let path = regular_path(root, relative)?;
    if path.exists() {
        for (name, bytes) in members {
            if read_regular(root, &format!("{relative}/{name}"))? != *bytes {
                return Err("existing inventory generation has different bytes".to_string());
            }
        }
        return Ok(path);
    }
    let parent = path.parent().ok_or("generation has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let staging = tempfile::Builder::new()
        .prefix(".inventory-stage-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    for (name, bytes) in members {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.path().join(name))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(staging.path(), &path).map_err(|e| e.to_string())?;
    Ok(path)
}

pub(super) async fn selected_authority(
    root: &Path,
    record: &PersistedInventoryActivation,
    compiled: bool,
) -> Result<Option<CompletionProofAuthority>, String> {
    let config_bytes = read_regular(root, CONFIG_RELATIVE_PATH)?;
    if config_bytes == record.predecessor_config.as_bytes() {
        return Ok(None);
    }
    if config_bytes != record.selected_config.as_bytes() {
        return Err("configuration differs from the privately authorized V2 selection".to_string());
    }
    let config = parse_repository_config_bytes(&config_bytes, &root.join(CONFIG_RELATIVE_PATH))?;
    if !compiled {
        return Err("Inventory V2 activation requires the compiled KD4 authority".to_string());
    }
    // Every authority check re-reads roughly 10 MB of bundle members and hashes
    // the ~38 MB generation; keep that off the async runtime.
    let predecessor = {
        let root = root.to_path_buf();
        let record = record.clone();
        tokio::task::spawn_blocking(move || {
            for member in KD4_TRUSTED_BUNDLE_MEMBERS {
                if member.relative_path == CONFIG_RELATIVE_PATH {
                    if member.bytes != record.predecessor_config.as_bytes()
                        && member.bytes != record.selected_config.as_bytes()
                    {
                        return Err("compiled predecessor configuration changed".to_string());
                    }
                } else if read_regular(&root, member.relative_path)? != member.bytes {
                    return Err(format!(
                        "trusted KD4 bundle member changed: {}",
                        member.relative_path
                    ));
                }
            }
            verify_generation(&root, &record)?;
            compiled_kd4_authority()
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let bundle_sha = proof_hash(
        "kd4.activated-inventory-policy.v1",
        &serde_json::json!({
        "compiled_policy": predecessor.policy_runner_bundle_sha256, "config": record.selected_config,
            "members": record.member_sha256,
        }),
    )
    .map_err(|e| e.to_string())?;
    let mut trusted_bundle_hashes = predecessor.trusted_bundle_hashes;
    trusted_bundle_hashes.insert(
        CONFIG_RELATIVE_PATH.to_owned(),
        format!("{:x}", Sha256::digest(&config_bytes)),
    );
    trusted_bundle_hashes.extend(
        record
            .member_sha256
            .iter()
            .map(|(name, hash)| (format!("{}/{name}", record.generation_path), hash.clone())),
    );
    Ok(Some(CompletionProofAuthority {
        config,
        policy_runner_bundle_sha256: bundle_sha.to_string(),
        trusted_bundle_hashes,
        trusted_runner_entrypoints: predecessor.trusted_runner_entrypoints,
    }))
}

fn verify_generation(root: &Path, record: &PersistedInventoryActivation) -> Result<(), String> {
    if record
        .member_sha256
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != MEMBERS.iter().copied().collect::<BTreeSet<_>>()
    {
        return Err("prepared activation does not contain the complete V2 generation".to_string());
    }
    for (name, digest) in &record.member_sha256 {
        if format!(
            "{:x}",
            Sha256::digest(read_regular(
                root,
                &format!("{}/{name}", record.generation_path)
            )?)
        ) != *digest
        {
            return Err(format!("activated V2 generation member changed: {name}"));
        }
    }
    Ok(())
}

pub(super) fn inventory_hash(
    root: &Path,
    config: &RepositoryCompletionProofConfig,
) -> Result<String, String> {
    let directory = Path::new(&config.frozen_inventory_path)
        .parent()
        .and_then(|p| p.to_str())
        .ok_or("V2 inventory has no directory")?;
    if config.replacement_ledger_path != format!("{directory}/{LEDGER}") {
        return Err("V2 config must select inventory and ledger from one generation".to_string());
    }
    let _bundle = load_bundle(root, directory)?;
    Ok(config.frozen_inventory_hash.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_keyring_store::tests::MockKeyringStore;

    fn source_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    // Synthetic capability data tests the projection after admission. This is
    // never written to a live home and is not evidence of an actual review.
    fn projection_capability() -> HistoricalAdmissionCapability {
        let root = source_root();
        let output = std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
            .args(["-c", r#"
import json
from pathlib import Path
from scripts import replacement_admission as a
root = Path.cwd()
plan = a.compile_historical_replacement_review_plan(root)
vectors = json.loads((root / 'scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json').read_bytes())
receipt = vectors['valid_vectors'][0]['receipt']
reviews = [dict(review_scope_id=s['review_scope_id'], review_scope_sha256=s['review_scope_sha256'], disposition=a.HISTORICAL_SCOPE_REVIEW_DISPOSITION) for s in plan['review_scopes']]
reviews.sort(key=lambda review: review['review_scope_id'])
print(json.dumps(a.build_historical_replacement_acceptance_proposal_v1(plan, receipt, reviews)))
"#]).current_dir(&root).output().expect("start proposal fixture compiler");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let proposal = parse(&output.stdout).expect("parse proposal fixture");
        let receipts: serde_json::Value = parse(include_bytes!(
            "../../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
        ))
        .expect("receipt fixtures");
        let approval = serde_json::from_value(receipts["valid_vectors"][0]["receipt"].clone())
            .expect("fixture receipt");
        let vectors: serde_json::Value = parse(include_bytes!(
            "../../../../scripts/fixtures/focused_live_successor_catalog_v1_vectors.json"
        ))
        .expect("catalog fixtures");
        let mut catalog: FocusedLiveSuccessorCatalogV1 =
            serde_json::from_value(vectors["valid_vectors"][0]["catalog"].clone())
                .expect("fixture catalog");
        let template = catalog.resolved_successor_entries[0].clone();
        let predecessor: serde_json::Value = parse(include_bytes!(
            "../../../../.codex/validation/test-replacements-v1.json"
        ))
        .expect("immutable ledger");
        let ids = predecessor["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .flat_map(|row| row["replacement_ids"].as_array().into_iter().flatten())
            .filter_map(|id| id.as_str())
            .collect::<BTreeSet<_>>();
        catalog.resolved_successor_entries = ids
            .into_iter()
            .map(|id| {
                let mut resolved = template.clone();
                let entry = resolved.inventory_entry.as_mut();
                let ExecutableIdentityV1::Test { test_id, .. } = &mut entry.executable_identity
                else {
                    panic!("fixture is a test");
                };
                *test_id = codex_validation_contracts::selection::TestIdV1::parse(id.to_string())
                    .expect("test ID");
                entry.executable_identity_sha256 =
                    proof_hash("kd4.executable-identity.v1", &entry.executable_identity)
                        .expect("identity hash");
                resolved.inventory_entry_semantic_sha256 =
                    entry.semantic_sha256().expect("entry hash");
                resolved
            })
            .collect();
        HistoricalAdmissionCapability {
            catalog,
            approval,
            proposal,
            sealed_review_sha256: "7".repeat(64),
        }
    }

    #[test]
    fn historical_projection_preserves_every_edge_and_unresolved_child() {
        let root = source_root();
        let mut bundle = load_bundle(&root, ".codex/validation").expect("dormant bundle");
        let before = bundle.ledger.rows.clone();
        let mut capability = projection_capability();
        accept_historical(&mut bundle, &capability).expect("project admitted history");
        validate_bundle(&bundle).expect("accepted bundle closes over the same predecessors");
        let mut accepted = 0;
        let mut edges = 0;
        for (old, new) in before.iter().zip(&bundle.ledger.rows) {
            match (&old.disposition, &new.disposition) {
                (
                    ReplacementLedgerDispositionV2::Replacement {
                        contract:
                            ReplacementContractDispositionV1::PendingReview {
                                legacy_replacement_hint: old_hint,
                                ..
                            },
                        edge_ids: old_edges,
                        ..
                    },
                    ReplacementLedgerDispositionV2::Replacement {
                        contract:
                            ReplacementContractDispositionV1::Accepted {
                                legacy_replacement_hint: new_hint,
                                ..
                            },
                        edge_ids: new_edges,
                        stage2_incorrect_behavior_ids: Stage2IncorrectBehaviorIdsV1::Reviewed(ids),
                    },
                ) => {
                    assert_eq!(old_hint, new_hint);
                    assert_eq!(old_edges, new_edges);
                    assert!(ids.is_empty());
                    accepted += 1;
                    edges += new_edges.len();
                }
                _ => assert_eq!(old, new, "nonhistorical obligations must remain unchanged"),
            }
        }
        assert_eq!(
            (accepted, edges, bundle.ledger.rows.len()),
            (644, 685, 16776)
        );
        // This frozen successor is never a row's primary candidate. Checking
        // only primary selectors must therefore fail the unchanged test.
        let secondary = "rust-nextest::codex-core::core_cli_workspace$suite::completion_proof_gate::renamed_non_documentation_destination_requires_canonical_proof_through_real_session_path";
        assert!(before.iter().all(|row| {
            match &row.disposition {
                ReplacementLedgerDispositionV2::Replacement {
                    contract:
                        ReplacementContractDispositionV1::PendingReview {
                            legacy_replacement_hint,
                            ..
                        },
                    ..
                } => {
                    legacy_replacement_hint
                        .replacement_ids
                        .first()
                        .map(String::as_str)
                        != Some(secondary)
                }
                _ => true,
            }
        }));
        capability.catalog.resolved_successor_entries.retain(|entry| !matches!(entry.identity(), ExecutableIdentityV1::Test { test_id, .. } if test_id.as_str() == secondary));
        let mut incomplete = load_bundle(&root, ".codex/validation").expect("fresh dormant bundle");
        assert!(
            accept_historical(&mut incomplete, &capability)
                .unwrap_err()
                .contains("missing historical successor")
        );
    }

    #[tokio::test]
    async fn prepared_activation_replays_after_authenticated_resume_and_rejects_drift() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let home = tempfile::tempdir().expect("temporary private home");
        let root = repository.path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .expect("initialize fixture")
                .success()
        );
        for member in KD4_TRUSTED_BUNDLE_MEMBERS {
            let path = root.join(member.relative_path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("bundle parent");
            std::fs::write(path, member.bytes).expect("compiled fixture bundle");
        }
        let trust: Arc<dyn KeyringStore> = Arc::new(MockKeyringStore::default());
        let load = || {
            CompletionProofLedger::load_or_new_with_context_and_trust_store(
                home.path().to_path_buf(),
                root,
                CompletionProofSessionAuthority::root_terminal_owner(
                    CompletionProofRuntimeRegistry::new(),
                    root,
                ),
                Some("fixture-activation-lineage".to_string()),
                trust.clone(),
            )
        };
        let ledger = load().await;
        let authority = ledger
            .verified_authority()
            .await
            .expect("compiled authority");
        let observation = workspace_observation(root)
            .await
            .expect("preparation input snapshot");
        let capability = projection_capability();
        let mut bundle = load_bundle(&source_root(), ".codex/validation").expect("V2 fixture");
        accept_historical(&mut bundle, &capability).expect("project fixture admissions");
        let generation_path = ".codex/validation/inventory-generations/resume-fixture";
        stage_generation(root, generation_path, &bundle.raw).expect("complete staged generation");
        let predecessor_config = String::from_utf8(
            read_regular(root, CONFIG_RELATIVE_PATH).expect("predecessor config"),
        )
        .expect("UTF8 config");
        let mut selected = predecessor_config
            .parse::<toml_edit::DocumentMut>()
            .expect("config TOML");
        selected["frozen_inventory"] = toml_edit::value(format!("{generation_path}/{INVENTORY}"));
        selected["replacement_ledger"] = toml_edit::value(format!("{generation_path}/{LEDGER}"));
        // Seed only the already-authorized preparation boundary in this isolated
        // MAC/keyring fixture. This does not test or provide live review authority.
        let prepared = PersistedInventoryActivation {
            predecessor_config: predecessor_config.clone(),
            selected_config: selected.to_string(),
            predecessor_bundle_sha256: authority.policy_runner_bundle_sha256,
            generation_path: generation_path.to_string(),
            member_sha256: bundle
                .raw
                .iter()
                .map(|(name, bytes)| (name.clone(), format!("{:x}", Sha256::digest(bytes))))
                .collect(),
            workspace_head_identity: observation.head_identity,
            workspace_path_fingerprints: observation.path_fingerprints,
            approval: PersistedCurrentEvidenceCatalogV1 {
                schema_version: 1,
                catalog: capability.catalog,
                catalog_sha256: "1".repeat(64),
                channel_frame_sha256: "2".repeat(64),
                attempt_id: "fixture-attempt".to_string(),
                invocation_nonce: "fixture-nonce".to_string(),
                exact_command: "just completion-focused inventory.current-evidence".to_string(),
                repository_root: root.display().to_string(),
                workspace_fingerprint: observation.fingerprint,
                mutation_epoch: 0,
                policy_runner_bundle_sha256: "3".repeat(64),
                runner_process_id: 1,
                runner_executable_path: PathBuf::from("fixture-python"),
                runner_entrypoint_path: PathBuf::from("scripts/completion_proof.py"),
                session_lineage_id: "fixture-activation-lineage".to_string(),
                recorded_at_unix_ms: 1,
                approval_receipt: None,
            },
        };
        ledger.state.lock().await.persistent.inventory_activation = Some(prepared.clone());
        ledger.persist().await.expect("authenticate preparation");
        drop(ledger);

        // Load really performs ordinary workspace reconciliation. The generated
        // files can invalidate the catalog without destroying the preparation.
        let resumed = load().await;
        assert!(
            resumed
                .state
                .lock()
                .await
                .persistent
                .current_evidence_catalog
                .is_none()
        );
        // Drift before the single commit can never replay. While the predecessor
        // configuration is still selected the stale record is discarded and a
        // fresh preparation is attempted, which this fixture cannot satisfy.
        std::fs::write(root.join("concurrent-source.rs"), "changed").expect("external edit");
        let drift = resumed.activate_inventory_v2().await.unwrap_err();
        assert!(
            !drift.contains("workspace changed since the authenticated activation preparation"),
            "stale preparation must be discarded, not replayed forever: {drift}"
        );
        assert!(
            resumed
                .state
                .lock()
                .await
                .persistent
                .inventory_activation
                .is_none(),
            "drifted preparation was retained"
        );
        assert_eq!(
            read_regular(root, CONFIG_RELATIVE_PATH).unwrap(),
            predecessor_config.as_bytes()
        );
        std::fs::remove_file(root.join("concurrent-source.rs")).expect("restore fixture inputs");
        resumed.state.lock().await.persistent.inventory_activation = Some(prepared.clone());
        resumed
            .persist()
            .await
            .expect("re-seed preparation after drift");
        let member = root.join(generation_path).join(INVENTORY);
        std::fs::write(&member, b"corrupt").expect("change staged member");
        let tampered = resumed.activate_inventory_v2().await.unwrap_err();
        assert!(
            !tampered.contains("generation member changed"),
            "tampered staging must discard the preparation: {tampered}"
        );
        assert!(
            resumed
                .state
                .lock()
                .await
                .persistent
                .inventory_activation
                .is_none()
        );
        std::fs::write(&member, &bundle.raw[INVENTORY]).expect("restore staged fixture member");
        resumed.state.lock().await.persistent.inventory_activation = Some(prepared.clone());
        resumed
            .persist()
            .await
            .expect("re-seed preparation after tampering");
        #[cfg(windows)]
        {
            // A failed replacement is not drift: the verified preparation survives.
            let config = root.join(CONFIG_RELATIVE_PATH);
            let permissions = std::fs::metadata(&config).unwrap().permissions();
            let mut readonly = permissions.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&config, readonly).unwrap();
            let attempt = resumed.activate_inventory_v2().await;
            std::fs::set_permissions(&config, permissions).unwrap();
            assert!(attempt.is_err());
            assert!(
                resumed
                    .state
                    .lock()
                    .await
                    .persistent
                    .inventory_activation
                    .is_some(),
                "a failed config write must keep the authenticated preparation"
            );
            assert_eq!(
                read_regular(root, CONFIG_RELATIVE_PATH).unwrap(),
                predecessor_config.as_bytes()
            );
        }
        drop(resumed);
        let resumed = load().await;
        assert!(
            resumed
                .activate_inventory_v2()
                .await
                .expect("replay exact prepared commit")
                .contains("resumed")
        );
        assert_eq!(
            read_regular(root, CONFIG_RELATIVE_PATH).unwrap(),
            prepared.selected_config.as_bytes()
        );
        let selected_authority = resumed
            .verified_authority()
            .await
            .expect("selected authority");
        assert!(is_v2(&selected_authority.config));
        assert_eq!(
            inventory_hash(root, &selected_authority.config).expect("Core V2 reader"),
            KD4_FROZEN_INVENTORY_HASH
        );
        let policy_hash = selected_authority.policy_runner_bundle_sha256;
        drop(resumed);
        let resumed = load().await;
        assert_eq!(
            resumed
                .verified_authority()
                .await
                .expect("authority after restart")
                .policy_runner_bundle_sha256,
            policy_hash
        );
        assert!(
            resumed
                .activate_inventory_v2()
                .await
                .expect("idempotent activation")
                .contains("already active")
        );
        std::fs::write(&member, b"late tampering").expect("tamper selected generation");
        assert!(
            resumed
                .verified_authority()
                .await
                .unwrap_err()
                .contains("generation member changed")
        );
    }

    #[test]
    fn staged_generation_failure_and_interruption_preserve_selected_bundle() {
        let root = tempfile::tempdir().expect("temporary repository");
        let config = root.path().join("config.toml");
        let old = b"inventory='old/inventory.json'\nledger='old/ledger.json'\n";
        std::fs::write(&config, old).expect("old config");
        let failure = BTreeMap::from([
            ("a.json".to_string(), b"staged".to_vec()),
            ("missing-parent/b.json".to_string(), b"failure".to_vec()),
        ]);
        assert!(stage_generation(root.path(), "generations/failed", &failure).is_err());
        assert_eq!(std::fs::read(&config).expect("config after failure"), old);
        assert!(!root.path().join("generations/failed").exists());
        assert_eq!(
            std::fs::read_dir(root.path().join("generations"))
                .expect("staging parent")
                .count(),
            0
        );
        let members = BTreeMap::from([
            ("inventory.json".to_string(), b"complete inventory".to_vec()),
            ("ledger.json".to_string(), b"complete ledger".to_vec()),
        ]);
        stage_generation(root.path(), "generations/next", &members).expect("stage next generation");
        // Represents interruption after staging and before the single commit.
        assert_eq!(
            std::fs::read(&config).expect("old selection after interruption"),
            old
        );
        for (name, bytes) in &members {
            assert_eq!(
                std::fs::read(root.path().join("generations/next").join(name))
                    .expect("complete staged member"),
                *bytes
            );
        }
        let selected =
            b"inventory='generations/next/inventory.json'\nledger='generations/next/ledger.json'\n";
        #[cfg(windows)]
        {
            let permissions = std::fs::metadata(&config)
                .expect("config metadata")
                .permissions();
            let mut readonly = permissions.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&config, readonly)
                .expect("simulate unavailable config replacement");
            let result = write_private_state_blocking(&config, selected);
            std::fs::set_permissions(&config, permissions).expect("restore fixture permissions");
            assert!(result.is_err());
            assert_eq!(
                std::fs::read(&config).expect("config after failed replacement"),
                old
            );
        }
        write_private_state_blocking(&config, selected).expect("atomic selection");
        assert_eq!(
            std::fs::read(&config).expect("new complete selection"),
            selected
        );
        let conflicting = BTreeMap::from([("inventory.json".to_string(), b"corrupt".to_vec())]);
        assert!(stage_generation(root.path(), "generations/next", &conflicting).is_err());
        assert_eq!(
            std::fs::read(root.path().join("generations/next/inventory.json"))
                .expect("preserved inventory"),
            b"complete inventory"
        );
    }
}
