use super::*;

const REMOTE_CONTROL_APP_SERVER_CLIENT_NAME_NONE: &str = "";

/// Persisted remote-control server enrollment, including the lookup key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteControlEnrollmentRecord {
    pub websocket_url: String,
    pub account_id: String,
    pub app_server_client_name: Option<String>,
    pub server_id: String,
    pub environment_id: String,
    pub server_name: String,
    pub remote_control_enabled: Option<bool>,
}

fn remote_control_app_server_client_name_key(app_server_client_name: Option<&str>) -> &str {
    app_server_client_name.unwrap_or(REMOTE_CONTROL_APP_SERVER_CLIENT_NAME_NONE)
}

fn app_server_client_name_from_key(app_server_client_name: String) -> Option<String> {
    if app_server_client_name.is_empty() {
        None
    } else {
        Some(app_server_client_name)
    }
}

impl StateRuntime {
    pub async fn get_remote_control_enrollment(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<Option<RemoteControlEnrollmentRecord>> {
        let row = sqlx::query(
            r#"
SELECT websocket_url, account_id, app_server_client_name, server_id, environment_id, server_name,
    remote_control_enabled
FROM remote_control_enrollments
WHERE websocket_url = ? AND account_id = ? AND app_server_client_name = ?
            "#,
        )
        .bind(websocket_url)
        .bind(account_id)
        .bind(remote_control_app_server_client_name_key(
            app_server_client_name,
        ))
        .fetch_optional(self.pool.as_ref())
        .await?;

        row.map(|row| {
            let app_server_client_name: String = row.try_get("app_server_client_name")?;
            Ok(RemoteControlEnrollmentRecord {
                websocket_url: row.try_get("websocket_url")?,
                account_id: row.try_get("account_id")?,
                app_server_client_name: app_server_client_name_from_key(app_server_client_name),
                server_id: row.try_get("server_id")?,
                environment_id: row.try_get("environment_id")?,
                server_name: row.try_get("server_name")?,
                remote_control_enabled: row.try_get("remote_control_enabled")?,
            })
        })
        .transpose()
    }

    pub async fn upsert_remote_control_enrollment(
        &self,
        enrollment: &RemoteControlEnrollmentRecord,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO remote_control_enrollments (
    websocket_url,
    account_id,
    app_server_client_name,
    server_id,
    environment_id,
    server_name,
    remote_control_enabled,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(websocket_url, account_id, app_server_client_name) DO UPDATE SET
    server_id = excluded.server_id,
    environment_id = excluded.environment_id,
    server_name = excluded.server_name,
    updated_at = excluded.updated_at
            "#,
        )
        .bind(&enrollment.websocket_url)
        .bind(&enrollment.account_id)
        .bind(remote_control_app_server_client_name_key(
            enrollment.app_server_client_name.as_deref(),
        ))
        .bind(&enrollment.server_id)
        .bind(&enrollment.environment_id)
        .bind(&enrollment.server_name)
        .bind(enrollment.remote_control_enabled)
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    pub async fn set_remote_control_enabled(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
        remote_control_enabled: bool,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            r#"
UPDATE remote_control_enrollments
SET remote_control_enabled = ?, updated_at = ?
WHERE websocket_url = ? AND account_id = ? AND app_server_client_name = ?
            "#,
        )
        .bind(remote_control_enabled)
        .bind(Utc::now().timestamp())
        .bind(websocket_url)
        .bind(account_id)
        .bind(remote_control_app_server_client_name_key(
            app_server_client_name,
        ))
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_remote_control_enrollment(
        &self,
        websocket_url: &str,
        account_id: &str,
        app_server_client_name: Option<&str>,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            r#"
DELETE FROM remote_control_enrollments
WHERE websocket_url = ? AND account_id = ? AND app_server_client_name = ?
            "#,
        )
        .bind(websocket_url)
        .bind(account_id)
        .bind(remote_control_app_server_client_name_key(
            app_server_client_name,
        ))
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected())
    }
}

