use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use crate::auth::KnownPlan;
use crate::auth::PlanType as AuthPlanType;

#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, JsonSchema, TS, Default)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum PlanType {
    #[default]
    Free,
    Go,
    Plus,
    Pro,
    ProLite,
    ProMax,
    Team,
    #[serde(rename = "self_serve_business_prolite")]
    #[ts(rename = "self_serve_business_prolite")]
    SelfServeBusinessProLite,
    #[serde(rename = "self_serve_business_usage_based")]
    #[ts(rename = "self_serve_business_usage_based")]
    SelfServeBusinessUsageBased,
    Business,
    Ent26,
    #[serde(rename = "enterprise_cbp_automation")]
    #[ts(rename = "enterprise_cbp_automation")]
    EnterpriseCbpAutomation,
    #[serde(rename = "enterprise_cbp_usage_based")]
    #[ts(rename = "enterprise_cbp_usage_based")]
    EnterpriseCbpUsageBased,
    Enterprise,
    Edu,
    #[serde(other)]
    Unknown,
}

/// Account state returned by a model provider before it is adapted to an app-facing wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAccount {
    ApiKey,
    Chatgpt {
        email: Option<String>,
        plan_type: PlanType,
    },
    AmazonBedrock {
        credential_source: AmazonBedrockCredentialSource,
    },
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum AmazonBedrockCredentialSource {
    CodexManaged,
    AwsManaged,
}

impl PlanType {
    pub fn is_team_like(self) -> bool {
        matches!(
            self,
            Self::Team | Self::SelfServeBusinessProLite | Self::SelfServeBusinessUsageBased
        )
    }

    pub fn is_business_like(self) -> bool {
        matches!(
            self,
            Self::Business
                | Self::Ent26
                | Self::EnterpriseCbpAutomation
                | Self::EnterpriseCbpUsageBased
        )
    }

    pub fn is_workspace_account(self) -> bool {
        matches!(
            self,
            Self::Team
                | Self::SelfServeBusinessProLite
                | Self::SelfServeBusinessUsageBased
                | Self::Business
                | Self::Ent26
                | Self::EnterpriseCbpAutomation
                | Self::EnterpriseCbpUsageBased
                | Self::Enterprise
                | Self::Edu
        )
    }
}

impl From<AuthPlanType> for PlanType {
    fn from(plan_type: AuthPlanType) -> Self {
        match plan_type {
            AuthPlanType::Known(plan) => plan.into(),
            AuthPlanType::Unknown(_) => Self::Unknown,
        }
    }
}

impl From<KnownPlan> for PlanType {
    fn from(plan: KnownPlan) -> Self {
        match plan {
            KnownPlan::Free => Self::Free,
            KnownPlan::Go => Self::Go,
            KnownPlan::Plus => Self::Plus,
            KnownPlan::Pro => Self::Pro,
            KnownPlan::ProLite => Self::ProLite,
            KnownPlan::ProMax => Self::ProMax,
            KnownPlan::Team => Self::Team,
            KnownPlan::SelfServeBusinessProLite => Self::SelfServeBusinessProLite,
            KnownPlan::SelfServeBusinessUsageBased => Self::SelfServeBusinessUsageBased,
            KnownPlan::Business => Self::Business,
            KnownPlan::Ent26 => Self::Ent26,
            KnownPlan::EnterpriseCbpAutomation => Self::EnterpriseCbpAutomation,
            KnownPlan::EnterpriseCbpUsageBased => Self::EnterpriseCbpUsageBased,
            KnownPlan::Enterprise => Self::Enterprise,
            KnownPlan::Edu => Self::Edu,
        }
    }
}

