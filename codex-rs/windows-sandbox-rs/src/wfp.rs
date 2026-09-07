mod filter_specs;

use crate::to_wide;
use anyhow::Result;
use std::ffi::OsStr;
use std::mem::zeroed;
use std::ptr::null;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::FWP_E_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::FWP_E_FILTER_NOT_FOUND;
use windows_sys::Win32::Foundation::FWP_E_NOT_FOUND;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_BLOCK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTION_PERMIT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_ACTRL_MATCH_FILTER;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_BYTE_BLOB;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_FLAG_IS_LOOPBACK;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_CONDITION_VALUE0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_EMPTY;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_EQUAL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_FLAGS_ALL_SET;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_MATCH_FLAGS_NONE_SET;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_SECURITY_DESCRIPTOR_TYPE;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT8;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT16;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_UINT32;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_ACTION0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_ALE_USER_ID;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_FLAGS;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_PROTOCOL;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_CONDITION_IP_REMOTE_PORT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_DISPLAY_DATA0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_CONDITION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_FILTER0_0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_PROVIDER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SESSION0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER_FLAG_PERSISTENT;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_SUBLAYER0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineClose0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmEngineOpen0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmFilterDeleteByKey0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmProviderAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmSubLayerAdd0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionAbort0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionBegin0;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FwpmTransactionCommit0;
use windows_sys::Win32::Networking::WinSock::IPPROTO_TCP;
use windows_sys::Win32::Networking::WinSock::IPPROTO_UDP;
use windows_sys::Win32::Security::Authorization::BuildExplicitAccessWithNameW;
use windows_sys::Win32::Security::Authorization::BuildSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::core::GUID;

use filter_specs::ConditionSpec;
use filter_specs::FILTER_SPECS;
use filter_specs::FilterSpec;

const SESSION_NAME: &str = "Codex Windows Sandbox WFP";
const PROVIDER_NAME: &str = "Codex Windows Sandbox WFP";
const PROVIDER_DESCRIPTION: &str = "Persistent WFP provider for Codex Windows sandbox filters";
const SUBLAYER_NAME: &str = "Codex Windows Sandbox WFP";
const SUBLAYER_DESCRIPTION: &str = "Persistent WFP sublayer for Codex Windows sandbox filters";

#[derive(Clone, Copy)]
struct LoopbackFilterSpec {
    key: GUID,
    name: &'static str,
    description: &'static str,
    layer_key: GUID,
    protocol: u8,
}

const LOOPBACK_BLOCK_WEIGHT: u8 = 8;
const LOOPBACK_PROXY_PERMIT_WEIGHT: u8 = 9;
const MAX_PROXY_PORTS: usize = 10;
const LOOPBACK_PROXY_FILTER_KEY_BASE: u128 = 0xa72e1d8b_2cc4_4faa_8000_000000000000;

const LOOPBACK_FILTER_SPECS: &[LoopbackFilterSpec] = &[
    LoopbackFilterSpec {
        key: GUID::from_u128(0x23a8c889_d0f8_4e7e_9e5e_267189459fb2),
        name: "codex_wfp_loopback_tcp_v4",
        description: "Block sandbox-account loopback TCP except configured proxy ports v4",
        layer_key: windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        protocol: IPPROTO_TCP as u8,
    },
    LoopbackFilterSpec {
        key: GUID::from_u128(0x59480d35_d5bc_49d6_a425_440f4f30f66e),
        name: "codex_wfp_loopback_tcp_v6",
        description: "Block sandbox-account loopback TCP except configured proxy ports v6",
        layer_key: windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        protocol: IPPROTO_TCP as u8,
    },
    LoopbackFilterSpec {
        key: GUID::from_u128(0xbfc10c2b_c9a2_42ae_8726_b9c00bcaf9c1),
        name: "codex_wfp_loopback_udp_v4",
        description: "Block sandbox-account loopback UDP v4",
        layer_key: windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        protocol: IPPROTO_UDP as u8,
    },
    LoopbackFilterSpec {
        key: GUID::from_u128(0xe5f42f10_b350_4856_9e04_f08925524b84),
        name: "codex_wfp_loopback_udp_v6",
        description: "Block sandbox-account loopback UDP v6",
        layer_key: windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        protocol: IPPROTO_UDP as u8,
    },
];

// WFP identifies persistent providers, sublayers, and filters by stable GUIDs.
// These values are Codex-owned identities; do not regenerate them unless we
// intentionally want to orphan old objects and create a new WFP namespace.
const PROVIDER_KEY: GUID = GUID::from_u128(0x2e31d31c_3948_4753_9117_e5d1a6496f41);
const SUBLAYER_KEY: GUID = GUID::from_u128(0xe65054fd_4d32_4c7c_95ef_621f0cf6431a);

/// Installs the persistent Codex WFP filters for `account`.
///
/// This is intended to run from the already-elevated setup helper. The loopback
/// filters installed here are a required part of offline network isolation.
pub fn install_wfp_filters_for_account(
    account: &str,
    proxy_ports: &[u16],
    allow_local_binding: bool,
) -> Result<usize> {
    let engine = Engine::open()?;
    let mut transaction = engine.begin_transaction()?;
    ensure_provider(engine.handle)?;
    ensure_sublayer(engine.handle)?;

    let user_condition = UserMatchCondition::for_account(account)?;
    let proxy_ports = normalized_proxy_ports(proxy_ports)?;
    let mut installed_filter_count = 0;
    for spec in FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key)?;
        add_filter(engine.handle, spec, &user_condition)?;
        installed_filter_count += 1;
    }
    for spec in LOOPBACK_FILTER_SPECS {
        delete_filter_if_present(engine.handle, &spec.key)?;
    }
    for family in 0..2 {
        for slot in 0..MAX_PROXY_PORTS {
            delete_filter_if_present(engine.handle, &loopback_proxy_filter_key(family, slot))?;
        }
    }

    if !allow_local_binding {
        for spec in LOOPBACK_FILTER_SPECS {
            let conditions = loopback_conditions(spec.protocol, None);
            add_filter_parts(
                engine.handle,
                spec.key,
                spec.name,
                spec.description,
                spec.layer_key,
                &conditions,
                &user_condition,
                FWP_ACTION_BLOCK,
                LOOPBACK_BLOCK_WEIGHT,
            )?;
            installed_filter_count += 1;
        }
        for (slot, port) in proxy_ports.into_iter().enumerate() {
            for (family, layer_key) in [
                windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWPM_LAYER_ALE_AUTH_CONNECT_V6,
            ]
            .into_iter()
            .enumerate()
            {
                let name = format!("codex_wfp_loopback_proxy_v{}_slot_{slot}", family + 4);
                let description = format!(
                    "Permit sandbox-account loopback proxy TCP port {port} v{}",
                    family + 4
                );
                let conditions = loopback_conditions(IPPROTO_TCP as u8, Some(port));
                add_filter_parts(
                    engine.handle,
                    loopback_proxy_filter_key(family, slot),
                    &name,
                    &description,
                    layer_key,
                    &conditions,
                    &user_condition,
                    FWP_ACTION_PERMIT,
                    LOOPBACK_PROXY_PERMIT_WEIGHT,
                )?;
                installed_filter_count += 1;
            }
        }
    }

    transaction.commit()?;
    Ok(installed_filter_count)
}

/// Owns an open WFP engine handle and closes it on drop.
struct Engine {
    handle: HANDLE,
}

impl Engine {
    fn open() -> Result<Self> {
        let session_name = to_wide(OsStr::new(SESSION_NAME));
        let mut session: FWPM_SESSION0 = unsafe { zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 {
            name: session_name.as_ptr() as *mut _,
            description: null_mut(),
        };
        session.txnWaitTimeoutInMSec = INFINITE;

        let mut handle = HANDLE::default();
        let result = unsafe {
            FwpmEngineOpen0(
                null(),
                RPC_C_AUTHN_DEFAULT as u32,
                null(),
                &session,
                &mut handle,
            )
        };
        ensure_success(result, "FwpmEngineOpen0")?;
        Ok(Self { handle })
    }

    fn begin_transaction(&self) -> Result<Transaction<'_>> {
        let result = unsafe { FwpmTransactionBegin0(self.handle, 0) };
        ensure_success(result, "FwpmTransactionBegin0")?;
        Ok(Transaction {
            engine: self,
            committed: false,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.handle);
        }
    }
}

/// Aborts an open WFP transaction unless it was explicitly committed.
struct Transaction<'a> {
    engine: &'a Engine,
    committed: bool,
}

impl Transaction<'_> {
    fn commit(&mut self) -> Result<()> {
        let result = unsafe { FwpmTransactionCommit0(self.engine.handle) };
        ensure_success(result, "FwpmTransactionCommit0")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                FwpmTransactionAbort0(self.engine.handle);
            }
        }
    }
}

/// Builds the ALE_USER_ID condition blob that scopes filters to one account.
struct UserMatchCondition {
    security_descriptor: PSECURITY_DESCRIPTOR,
    blob: FWP_BYTE_BLOB,
}

impl UserMatchCondition {
    fn for_account(account: &str) -> Result<Self> {
        let account_w = to_wide(OsStr::new(account));
        let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
        unsafe {
            BuildExplicitAccessWithNameW(
                &mut access,
                account_w.as_ptr(),
                FWP_ACTRL_MATCH_FILTER,
                GRANT_ACCESS,
                0,
            );
        }

        let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let mut security_descriptor_len = 0;
        let result = unsafe {
            BuildSecurityDescriptorW(
                null(),
                null(),
                1,
                &access,
                0,
                null(),
                null_mut(),
                &mut security_descriptor_len,
                &mut security_descriptor,
            )
        };
        ensure_success(result, "BuildSecurityDescriptorW")?;

        Ok(Self {
            security_descriptor,
            blob: FWP_BYTE_BLOB {
                size: security_descriptor_len,
                data: security_descriptor as *mut u8,
            },
        })
    }
}

impl Drop for UserMatchCondition {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                LocalFree(self.security_descriptor as HLOCAL);
            }
        }
    }
}

/// Ensures the persistent Codex WFP provider exists.
fn ensure_provider(engine: HANDLE) -> Result<()> {
    let provider_name = to_wide(OsStr::new(PROVIDER_NAME));
    let provider_description = to_wide(OsStr::new(PROVIDER_DESCRIPTION));
    let provider = FWPM_PROVIDER0 {
        providerKey: PROVIDER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: provider_name.as_ptr() as *mut _,
            description: provider_description.as_ptr() as *mut _,
        },
        flags: FWPM_PROVIDER_FLAG_PERSISTENT,
        providerData: empty_blob(),
        serviceName: null_mut(),
    };

    let result = unsafe { FwpmProviderAdd0(engine, &provider, null_mut()) };
    ensure_success_or(result, "FwpmProviderAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

/// Ensures the persistent Codex sublayer exists under the Codex provider.
fn ensure_sublayer(engine: HANDLE) -> Result<()> {
    let sublayer_name = to_wide(OsStr::new(SUBLAYER_NAME));
    let sublayer_description = to_wide(OsStr::new(SUBLAYER_DESCRIPTION));
    let provider_key = PROVIDER_KEY;
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER_KEY,
        displayData: FWPM_DISPLAY_DATA0 {
            name: sublayer_name.as_ptr() as *mut _,
            description: sublayer_description.as_ptr() as *mut _,
        },
        flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        weight: 0x8000,
    };

    let result = unsafe { FwpmSubLayerAdd0(engine, &sublayer, null_mut()) };
    ensure_success_or(result, "FwpmSubLayerAdd0", &[FWP_E_ALREADY_EXISTS as u32])
}

/// Adds one blocking WFP filter from the static filter spec list.
fn add_filter(
    engine: HANDLE,
    spec: &FilterSpec,
    user_condition: &UserMatchCondition,
) -> Result<()> {
    add_filter_parts(
        engine,
        spec.key,
        spec.name,
        spec.description,
        spec.layer_key,
        spec.conditions,
        user_condition,
        FWP_ACTION_BLOCK,
        0,
    )
}

fn add_filter_parts(
    engine: HANDLE,
    key: GUID,
    name: &str,
    description: &str,
    layer_key: GUID,
    conditions: &[ConditionSpec],
    user_condition: &UserMatchCondition,
    action: u32,
    weight: u8,
) -> Result<()> {
    let filter_name = to_wide(OsStr::new(name));
    let filter_description = to_wide(OsStr::new(description));
    let mut filter_conditions = build_conditions(conditions, user_condition);
    let provider_key = PROVIDER_KEY;
    let filter = FWPM_FILTER0 {
        filterKey: key,
        displayData: FWPM_DISPLAY_DATA0 {
            name: filter_name.as_ptr() as *mut _,
            description: filter_description.as_ptr() as *mut _,
        },
        flags: FWPM_FILTER_FLAG_PERSISTENT,
        providerKey: &provider_key as *const _ as *mut _,
        providerData: empty_blob(),
        layerKey: layer_key,
        subLayerKey: SUBLAYER_KEY,
        weight: if weight == 0 {
            empty_value()
        } else {
            uint8_value(weight)
        },
        numFilterConditions: filter_conditions.len() as u32,
        filterCondition: filter_conditions.as_mut_ptr(),
        action: FWPM_ACTION0 {
            r#type: action,
            Anonymous: FWPM_ACTION0_0 {
                filterType: zero_guid(),
            },
        },
        Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
        reserved: null_mut(),
        filterId: 0,
        effectiveWeight: empty_value(),
    };

    let mut filter_id = 0_u64;
    let result = unsafe { FwpmFilterAdd0(engine, &filter, null_mut(), &mut filter_id) };
    ensure_success(result, &format!("FwpmFilterAdd0({name})"))
}

fn loopback_conditions(protocol: u8, remote_port: Option<u16>) -> Vec<ConditionSpec> {
    let mut conditions = vec![
        ConditionSpec::User,
        ConditionSpec::Loopback,
        ConditionSpec::Protocol(protocol),
    ];
    if let Some(remote_port) = remote_port {
        conditions.push(ConditionSpec::RemotePort(remote_port));
    }
    conditions
}

fn normalized_proxy_ports(proxy_ports: &[u16]) -> Result<Vec<u16>> {
    let mut proxy_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    proxy_ports.sort_unstable();
    proxy_ports.dedup();
    if proxy_ports.len() > MAX_PROXY_PORTS {
        return Err(anyhow::anyhow!(
            "too many loopback proxy ports for WFP filter slots: {} > {MAX_PROXY_PORTS}",
            proxy_ports.len()
        ));
    }
    Ok(proxy_ports)
}

fn loopback_proxy_filter_key(family: usize, slot: usize) -> GUID {
    GUID::from_u128(LOOPBACK_PROXY_FILTER_KEY_BASE + (family * MAX_PROXY_PORTS + slot) as u128)
}

/// Converts our compact condition specs into WFP filter conditions.
fn build_conditions(
    specs: &[ConditionSpec],
    user_condition: &UserMatchCondition,
) -> Vec<FWPM_FILTER_CONDITION0> {
    specs
        .iter()
        .map(|spec| match spec {
            ConditionSpec::User => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_ALE_USER_ID,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        sd: &user_condition.blob as *const _ as *mut _,
                    },
                },
            },
            ConditionSpec::Loopback => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_FLAGS,
                matchType: FWP_MATCH_FLAGS_ALL_SET,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        uint32: FWP_CONDITION_FLAG_IS_LOOPBACK,
                    },
                },
            },
            ConditionSpec::NotLoopback => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_FLAGS,
                matchType: FWP_MATCH_FLAGS_NONE_SET,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        uint32: FWP_CONDITION_FLAG_IS_LOOPBACK,
                    },
                },
            },
            ConditionSpec::Protocol(protocol) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint8: *protocol },
                },
            },
            ConditionSpec::RemotePort(port) => FWPM_FILTER_CONDITION0 {
                fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                matchType: FWP_MATCH_EQUAL,
                conditionValue: FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_CONDITION_VALUE0_0 { uint16: *port },
                },
            },
        })
        .collect()
}

/// Deletes an old copy of a filter before re-adding it.
fn delete_filter_if_present(engine: HANDLE, key: &GUID) -> Result<()> {
    let result = unsafe { FwpmFilterDeleteByKey0(engine, key) };
    ensure_success_or(
        result,
        "FwpmFilterDeleteByKey0",
        &[FWP_E_FILTER_NOT_FOUND as u32, FWP_E_NOT_FOUND as u32],
    )
}

fn ensure_success(result: u32, operation: &str) -> Result<()> {
    ensure_success_or(result, operation, &[])
}

fn ensure_success_or(result: u32, operation: &str, allowed: &[u32]) -> Result<()> {
    if result == 0 || allowed.contains(&result) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{operation} failed: {}",
            format_error_code(result)
        ))
    }
}

fn format_error_code(result: u32) -> String {
    format!("0x{result:08X}")
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB {
        size: 0,
        data: null_mut(),
    }
}

fn empty_value() -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_EMPTY,
        Anonymous: unsafe { zeroed() },
    }
}

fn uint8_value(value: u8) -> FWP_VALUE0 {
    FWP_VALUE0 {
        r#type: FWP_UINT8,
        Anonymous: windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_VALUE0_0 {
            uint8: value,
        },
    }
}

fn zero_guid() -> GUID {
    GUID::from_u128(0)
}

#[cfg(test)]
mod tests {
    use super::ConditionSpec;
    use super::FILTER_SPECS;
    use super::IPPROTO_TCP;
    use super::LOOPBACK_FILTER_SPECS;
    use super::LOOPBACK_PROXY_FILTER_KEY_BASE;
    use super::MAX_PROXY_PORTS;
    use super::loopback_conditions;
    use super::loopback_proxy_filter_key;
    use super::normalized_proxy_ports;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeSet;

    #[test]
    fn filter_keys_are_unique() {
        let keys = FILTER_SPECS
            .iter()
            .map(|spec| spec.key)
            .chain(LOOPBACK_FILTER_SPECS.iter().map(|spec| spec.key))
            .chain((0..2).flat_map(|family| {
                (0..MAX_PROXY_PORTS).map(move |slot| loopback_proxy_filter_key(family, slot))
            }))
            .map(|spec| (spec.data1, spec.data2, spec.data3, spec.data4))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys.len(),
            FILTER_SPECS.len() + LOOPBACK_FILTER_SPECS.len() + 2 * MAX_PROXY_PORTS
        );
    }

    #[test]
    fn filter_names_are_unique() {
        let names = FILTER_SPECS
            .iter()
            .map(|spec| spec.name)
            .chain(LOOPBACK_FILTER_SPECS.iter().map(|spec| spec.name))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names.len(),
            FILTER_SPECS.len() + LOOPBACK_FILTER_SPECS.len()
        );
    }

    #[test]
    fn loopback_proxy_filter_matches_one_exact_port() {
        assert_eq!(
            loopback_conditions(IPPROTO_TCP as u8, Some(8080)),
            vec![
                ConditionSpec::User,
                ConditionSpec::Loopback,
                ConditionSpec::Protocol(IPPROTO_TCP as u8),
                ConditionSpec::RemotePort(8080),
            ]
        );
    }

    #[test]
    fn proxy_ports_are_normalized_within_fixed_filter_slots() {
        assert_eq!(
            normalized_proxy_ports(&[8080, 0, 3128, 8080]).expect("valid proxy ports"),
            vec![3128, 8080]
        );
        let first = loopback_proxy_filter_key(0, 0);
        let last = loopback_proxy_filter_key(1, MAX_PROXY_PORTS - 1);
        assert_eq!(
            (first.data1, first.data2, first.data3, first.data4),
            (0xa72e1d8b, 0x2cc4, 0x4faa, [0x80, 0, 0, 0, 0, 0, 0, 0])
        );
        let expected_last = windows_sys::core::GUID::from_u128(
            LOOPBACK_PROXY_FILTER_KEY_BASE + (2 * MAX_PROXY_PORTS - 1) as u128,
        );
        assert_eq!(
            (last.data1, last.data2, last.data3, last.data4),
            (
                expected_last.data1,
                expected_last.data2,
                expected_last.data3,
                expected_last.data4,
            )
        );
    }

    #[test]
    fn static_ale_port_blocks_exclude_loopback_proxy_ports() {
        for spec in FILTER_SPECS.iter().filter(|spec| {
            spec.conditions
                .iter()
                .any(|condition| matches!(condition, ConditionSpec::RemotePort(_)))
        }) {
            assert!(spec.conditions.contains(&ConditionSpec::NotLoopback));
        }
    }
}
