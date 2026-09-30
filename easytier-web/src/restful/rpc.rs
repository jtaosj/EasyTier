use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use axum_login::AuthUser as _;
use easytier::proto::{
    api::{
        config::PatchConfigRequest,
        instance::{
            GenerateCredentialRequest, RevokeCredentialRequest, UpsertCredentialRequest,
            instance_identifier,
        },
        manage::{
            DeleteNetworkInstanceRequest, RetainNetworkInstanceRequest, RunNetworkInstanceRequest,
        },
    },
    rpc_types::controller::BaseController,
};

use crate::{
    central_network::service::CentralNetworkService,
    db::{CentralOwnedRuntimeConfig, UserIdInDb},
};

use super::{AppState, HttpHandleError, other_error};

#[derive(Debug, serde::Deserialize)]
pub struct ProxyRpcRequest {
    pub service_name: String,
    pub method_name: String,
    pub payload: serde_json::Value,
    pub scope: Option<String>,
}

macro_rules! match_service {
    ($factory:ty, $method_name:expr, $payload:expr, $session:expr) => {{
        let client = $session.scoped_client::<$factory>();
        client
            .json_call_method(BaseController::default(), &$method_name, $payload)
            .await
    }};
}

async fn handle_proxy_rpc_by_session(
    session: &crate::client_manager::session::Session,
    req: ProxyRpcRequest,
) -> Result<Json<serde_json::Value>, HttpHandleError> {
    let ProxyRpcRequest {
        service_name,
        method_name,
        payload,
        scope,
    } = req;

    let mutates_runtime_config = proxy_rpc_mutates_runtime_config(&service_name, &method_name);
    if mutates_runtime_config {
        session
            .invalidate_runtime_config_for_direct_mutation()
            .await;
    }

    let resp = match service_name.as_str() {
        "api.manage.WebClientService" => match_service!(
            easytier::proto::api::manage::WebClientServiceClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.PeerManageRpcService" => match_service!(
            easytier::proto::api::instance::PeerManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.PeerCenterManageRpcService" => match_service!(
            easytier::proto::peer_rpc::PeerCenterRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.ConnectorManageRpcService" => match_service!(
            easytier::proto::api::instance::ConnectorManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.MappedListenerManageRpcService" => match_service!(
            easytier::proto::api::instance::MappedListenerManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.VpnPortalRpcService" => match_service!(
            easytier::proto::api::instance::VpnPortalRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.TcpProxyRpcService" => {
            let client = if let Some(ref domain) = scope {
                session.scoped_client_with_domain::<
                    easytier::proto::api::instance::TcpProxyRpcClientFactory<BaseController>,
                >(domain.clone())
            } else {
                session.scoped_client::<
                    easytier::proto::api::instance::TcpProxyRpcClientFactory<BaseController>,
                >()
            };
            client
                .json_call_method(BaseController::default(), &method_name, payload)
                .await
        }
        "api.instance.AclManageRpcService" => match_service!(
            easytier::proto::api::instance::AclManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.PortForwardManageRpcService" => match_service!(
            easytier::proto::api::instance::PortForwardManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.StatsRpcService" => match_service!(
            easytier::proto::api::instance::StatsRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.instance.CredentialManageRpcService" => match_service!(
            easytier::proto::api::instance::CredentialManageRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.logger.LoggerRpcService" => match_service!(
            easytier::proto::api::logger::LoggerRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        "api.config.ConfigRpcService" => match_service!(
            easytier::proto::api::config::ConfigRpcClientFactory<BaseController>,
            method_name,
            payload,
            session
        ),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                other_error(format!("Unknown service: {}", service_name)).into(),
            ));
        }
    };

    if mutates_runtime_config {
        session
            .invalidate_runtime_config_for_direct_mutation()
            .await;
    }

    match resp {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            other_error(format!("RPC Error: {:?}", e)).into(),
        )),
    }
}

fn ensure_credential_mutation_allowed(
    req: &ProxyRpcRequest,
    central: &[CentralOwnedRuntimeConfig],
) -> Result<(), HttpHandleError> {
    if central.is_empty() {
        return Ok(());
    }
    let instance = match req.method_name.as_str() {
        "generate_credential" | "GenerateCredential" => {
            serde_json::from_value::<GenerateCredentialRequest>(req.payload.clone())
                .map(|request| request.instance)
        }
        "revoke_credential" | "RevokeCredential" => {
            serde_json::from_value::<RevokeCredentialRequest>(req.payload.clone())
                .map(|request| request.instance)
        }
        "upsert_credential" | "UpsertCredential" => {
            serde_json::from_value::<UpsertCredentialRequest>(req.payload.clone())
                .map(|request| request.instance)
        }
        _ => return Ok(()),
    }
    .map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            other_error(format!("Invalid RPC payload: {error}")).into(),
        )
    })?;
    let conflicts = match instance
        .as_ref()
        .and_then(|instance| instance.selector.as_ref())
    {
        Some(instance_identifier::Selector::Id(id)) => {
            let id = uuid::Uuid::from(*id);
            central.iter().any(|config| config.instance_id == id)
        }
        Some(instance_identifier::Selector::InstanceSelector(selector)) => selector
            .name
            .as_deref()
            .is_none_or(|name| central.iter().any(|config| config.network_name == name)),
        None => true,
    };
    if conflicts {
        return Err(super::network::central_ownership_conflict());
    }
    Ok(())
}

fn ensure_runtime_mutation_allowed(
    req: &ProxyRpcRequest,
    central: &[CentralOwnedRuntimeConfig],
) -> Result<(), HttpHandleError> {
    if central.is_empty() {
        return Ok(());
    }
    if req.service_name == "api.instance.CredentialManageRpcService" {
        return ensure_credential_mutation_allowed(req, central);
    }
    fn parse<T: serde::de::DeserializeOwned>(req: &ProxyRpcRequest) -> Result<T, HttpHandleError> {
        serde_json::from_value(req.payload.clone()).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                other_error(format!("Invalid RPC payload: {error}")).into(),
            )
        })
    }
    let conflicts = match (req.service_name.as_str(), req.method_name.as_str()) {
        ("api.manage.WebClientService", "run_network_instance" | "RunNetworkInstance") => {
            let request: RunNetworkInstanceRequest = parse(req)?;
            central.iter().any(|config| {
                request
                    .inst_id
                    .is_some_and(|id| uuid::Uuid::from(id) == config.instance_id)
                    || request.config.as_ref().is_some_and(|requested| {
                        requested
                            .instance_id
                            .as_deref()
                            .and_then(|id| uuid::Uuid::parse_str(id).ok())
                            == Some(config.instance_id)
                            || requested.network_name.as_deref()
                                == Some(config.network_name.as_str())
                    })
            })
        }
        ("api.manage.WebClientService", "retain_network_instance" | "RetainNetworkInstance") => {
            let request: RetainNetworkInstanceRequest = parse(req)?;
            central.iter().any(|config| {
                !request
                    .inst_ids
                    .iter()
                    .any(|id| uuid::Uuid::from(*id) == config.instance_id)
            })
        }
        ("api.manage.WebClientService", "delete_network_instance" | "DeleteNetworkInstance") => {
            let request: DeleteNetworkInstanceRequest = parse(req)?;
            central.iter().any(|config| {
                request
                    .inst_ids
                    .iter()
                    .any(|id| uuid::Uuid::from(*id) == config.instance_id)
            })
        }
        ("api.config.ConfigRpcService", "patch_config" | "PatchConfig") => {
            let request: PatchConfigRequest = parse(req)?;
            match request
                .instance
                .as_ref()
                .and_then(|instance| instance.selector.as_ref())
            {
                Some(instance_identifier::Selector::Id(id)) => central
                    .iter()
                    .any(|config| config.instance_id == uuid::Uuid::from(*id)),
                Some(instance_identifier::Selector::InstanceSelector(selector)) => selector
                    .name
                    .as_deref()
                    .is_none_or(|name| central.iter().any(|config| config.network_name == name)),
                None => true,
            }
        }
        _ => false,
    };
    if conflicts {
        return Err(super::network::central_ownership_conflict());
    }
    Ok(())
}

fn proxy_rpc_mutates_runtime_config(service_name: &str, method_name: &str) -> bool {
    matches!(
        (service_name, method_name),
        (
            "api.manage.WebClientService",
            "run_network_instance"
                | "RunNetworkInstance"
                | "retain_network_instance"
                | "RetainNetworkInstance"
                | "delete_network_instance"
                | "DeleteNetworkInstance"
        ) | (
            "api.config.ConfigRpcService",
            "patch_config" | "PatchConfig"
        ) | (
            "api.instance.CredentialManageRpcService",
            "generate_credential"
                | "GenerateCredential"
                | "revoke_credential"
                | "RevokeCredential"
                | "upsert_credential"
                | "UpsertCredential"
        )
    )
}

pub async fn handle_proxy_rpc(
    auth_session: super::users::AuthSession,
    State(client_mgr): AppState,
    Extension(network_service): Extension<Arc<CentralNetworkService>>,
    Path(machine_id): Path<uuid::Uuid>,
    Json(req): Json<ProxyRpcRequest>,
) -> Result<Json<serde_json::Value>, HttpHandleError> {
    let user_id = auth_session
        .user
        .as_ref()
        .ok_or((StatusCode::UNAUTHORIZED, other_error("Unauthorized").into()))?
        .id();

    let _mutation = if proxy_rpc_mutates_runtime_config(&req.service_name, &req.method_name) {
        let guard = network_service.lock_mutations().await;
        let central = network_service
            .db
            .central_owned_runtime_configs(user_id, machine_id)
            .await
            .map_err(super::convert_db_error)?;
        ensure_runtime_mutation_allowed(&req, &central)?;
        Some(guard)
    } else {
        None
    };
    let session = client_mgr
        .get_session_by_machine_id(user_id, &machine_id)
        .ok_or((
            StatusCode::NOT_FOUND,
            other_error("Session not found").into(),
        ))?;
    handle_proxy_rpc_by_session(session.as_ref(), req).await
}

pub fn router() -> Router<super::AppStateInner> {
    Router::new().route(
        "/api/v1/machines/{machine-id}/proxy-rpc",
        post(handle_proxy_rpc),
    )
}

/// Internal proxy-rpc handler: no AuthSession, resolves the active session by machine_id.
pub async fn handle_proxy_rpc_internal(
    State(client_mgr): AppState,
    Path((user_id, machine_id)): Path<(UserIdInDb, uuid::Uuid)>,
    Json(req): Json<ProxyRpcRequest>,
) -> Result<Json<serde_json::Value>, HttpHandleError> {
    let session = client_mgr
        .get_session_by_machine_id(user_id, &machine_id)
        .ok_or((
            StatusCode::NOT_FOUND,
            other_error("Session not found").into(),
        ))?;
    handle_proxy_rpc_by_session(session.as_ref(), req).await
}

pub fn router_internal() -> Router<super::AppStateInner> {
    Router::new().route(
        "/api/internal/users/{user-id}/machines/{machine-id}/proxy-rpc",
        post(handle_proxy_rpc_internal),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use easytier::proto::api::instance::InstanceIdentifier;

    #[test]
    fn config_proxy_mutations_cannot_overwrite_or_remove_central_instances() {
        let central_id = uuid::Uuid::new_v4();
        let other_id = uuid::Uuid::new_v4();
        let central = [CentralOwnedRuntimeConfig {
            instance_id: central_id,
            network_name: "central-network".into(),
        }];
        let cases = [
            (
                "RunNetworkInstance",
                serde_json::to_value(RunNetworkInstanceRequest {
                    inst_id: Some(central_id.into()),
                    ..Default::default()
                })
                .unwrap(),
                true,
            ),
            (
                "RunNetworkInstance",
                serde_json::to_value(RunNetworkInstanceRequest {
                    config: Some(easytier::proto::api::manage::NetworkConfig {
                        instance_id: Some(other_id.to_string()),
                        network_name: Some("central-network".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .unwrap(),
                true,
            ),
            (
                "RetainNetworkInstance",
                serde_json::to_value(RetainNetworkInstanceRequest {
                    inst_ids: vec![other_id.into()],
                })
                .unwrap(),
                true,
            ),
            (
                "RetainNetworkInstance",
                serde_json::to_value(RetainNetworkInstanceRequest {
                    inst_ids: vec![central_id.into()],
                })
                .unwrap(),
                false,
            ),
            (
                "DeleteNetworkInstance",
                serde_json::to_value(DeleteNetworkInstanceRequest {
                    inst_ids: vec![central_id.into()],
                })
                .unwrap(),
                true,
            ),
            (
                "DeleteNetworkInstance",
                serde_json::to_value(DeleteNetworkInstanceRequest {
                    inst_ids: vec![other_id.into()],
                })
                .unwrap(),
                false,
            ),
            (
                "PatchConfig",
                serde_json::to_value(PatchConfigRequest::default()).unwrap(),
                true,
            ),
            (
                "PatchConfig",
                serde_json::to_value(PatchConfigRequest {
                    instance: Some(InstanceIdentifier {
                        selector: Some(instance_identifier::Selector::Id(other_id.into())),
                    }),
                    ..Default::default()
                })
                .unwrap(),
                false,
            ),
        ];
        for (method, payload, conflicts) in cases {
            let request = ProxyRpcRequest {
                service_name: if method == "PatchConfig" {
                    "api.config.ConfigRpcService"
                } else {
                    "api.manage.WebClientService"
                }
                .into(),
                method_name: method.into(),
                payload,
                scope: None,
            };
            let result = ensure_runtime_mutation_allowed(&request, &central);
            assert_eq!(result.is_err(), conflicts, "{method}");
            if let Err(error) = result {
                assert_eq!(error.0, StatusCode::CONFLICT);
            }
            ensure_runtime_mutation_allowed(&request, &[]).unwrap();
        }
    }

    #[test]
    fn credential_mutations_preserve_central_ownership_for_all_selectors() {
        let central_id = uuid::Uuid::new_v4();
        let central = [CentralOwnedRuntimeConfig {
            instance_id: central_id,
            network_name: "central-network".into(),
        }];
        let by_id = |id: uuid::Uuid| InstanceIdentifier {
            selector: Some(instance_identifier::Selector::Id(id.into())),
        };
        let by_name = |name: Option<&str>| InstanceIdentifier {
            selector: Some(instance_identifier::Selector::InstanceSelector(
                instance_identifier::InstanceSelector {
                    name: name.map(str::to_owned),
                },
            )),
        };
        for (method, payload) in [
            (
                "GenerateCredential",
                serde_json::to_value(GenerateCredentialRequest::default()).unwrap(),
            ),
            (
                "UpsertCredential",
                serde_json::to_value(UpsertCredentialRequest::default()).unwrap(),
            ),
            (
                "RevokeCredential",
                serde_json::to_value(RevokeCredentialRequest::default()).unwrap(),
            ),
        ] {
            let snake_case = method.replace("Credential", "_credential").to_lowercase();
            for method in [method, snake_case.as_str()] {
                for (instance, conflicts) in [
                    (None, true),
                    (Some(InstanceIdentifier::default()), true),
                    (Some(by_name(None)), true),
                    (Some(by_id(central_id)), true),
                    (Some(by_name(Some("central-network"))), true),
                    (Some(by_id(uuid::Uuid::new_v4())), false),
                    (Some(by_name(Some("console-network"))), false),
                ] {
                    let mut req = ProxyRpcRequest {
                        service_name: "api.instance.CredentialManageRpcService".into(),
                        method_name: method.into(),
                        payload: payload.clone(),
                        scope: None,
                    };
                    req.payload["instance"] = serde_json::to_value(instance).unwrap();
                    let result = ensure_credential_mutation_allowed(&req, &central);
                    if conflicts {
                        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT, "{method}");
                    } else {
                        result.unwrap();
                    }
                    // Devices with only external or manual configs remain unrestricted.
                    ensure_credential_mutation_allowed(&req, &[]).unwrap();
                }
            }
        }
    }

    #[test]
    fn runtime_config_mutation_detection_covers_proxy_rpc_aliases() {
        for (service, method) in [
            ("api.manage.WebClientService", "run_network_instance"),
            ("api.manage.WebClientService", "RetainNetworkInstance"),
            ("api.manage.WebClientService", "delete_network_instance"),
            ("api.config.ConfigRpcService", "PatchConfig"),
            (
                "api.instance.CredentialManageRpcService",
                "generate_credential",
            ),
            (
                "api.instance.CredentialManageRpcService",
                "RevokeCredential",
            ),
            (
                "api.instance.CredentialManageRpcService",
                "upsert_credential",
            ),
        ] {
            assert!(
                proxy_rpc_mutates_runtime_config(service, method),
                "{service}/{method} must invalidate the managed revision fence"
            );
        }

        for (service, method) in [
            ("api.manage.WebClientService", "list_network_instance"),
            ("api.config.ConfigRpcService", "get_config"),
            (
                "api.instance.CredentialManageRpcService",
                "list_credentials",
            ),
            ("api.instance.StatsRpcService", "get_stats"),
        ] {
            assert!(
                !proxy_rpc_mutates_runtime_config(service, method),
                "{service}/{method} must remain read-only"
            );
        }
    }
}
