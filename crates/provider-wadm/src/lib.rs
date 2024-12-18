use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use futures::stream::{AbortHandle, Abortable};
use futures::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tracing::{debug, error, instrument, warn};
use tracing_futures::Instrument as _;
use wadm_client::{Client, ClientConnectOptions};
use wadm_types::{
    api::{StatusResponse, StatusResult},
    wasmcloud::wadm::handler::StatusUpdate,
};
use wasmcloud_provider_sdk::provider::WrpcClient;
use wasmcloud_provider_sdk::wasmcloud_tracing::context::TraceContextInjector;
use wasmcloud_provider_sdk::{
    core::HostData, get_connection, load_host_data, run_provider, Context, LinkConfig, Provider,
};
use wasmcloud_provider_sdk::{serve_provider_exports, LinkDeleteInfo};

use crate::bindings::exports::wasmcloud::wadm::client::{
    ModelSummary, OamManifest, Status, VersionInfo,
};

mod config;
use config::WadmConfig;

mod bindings {
    wit_bindgen_wrpc::generate!({
        additional_derives: [
            serde::Serialize,
            serde::Deserialize,
        ],
        with: {
            "wasmcloud:wadm/types@0.2.0": wadm_types::wasmcloud::wadm::types,
            "wasmcloud:wadm/handler@0.2.0": generate,
            "wasmcloud:wadm/client@0.2.0": generate,
        }
    });
}

pub async fn run() -> anyhow::Result<()> {
    WadmProvider::run().await
}

struct WadmClientBundle {
    pub client: Client,
    pub sub_handles: Vec<(String, AbortHandle)>,
}

impl Drop for WadmClientBundle {
    fn drop(&mut self) {
        for (_topic, handle) in &self.sub_handles {
            handle.abort();
        }
    }
}

#[derive(Clone)]
pub struct WadmProvider {
    default_config: WadmConfig,
    handler_components: Arc<RwLock<HashMap<String, WadmClientBundle>>>,
    consumer_components: Arc<RwLock<HashMap<String, WadmClientBundle>>>,
}

impl Default for WadmProvider {
    fn default() -> Self {
        WadmProvider {
            handler_components: Arc::new(RwLock::new(HashMap::new())),
            consumer_components: Arc::new(RwLock::new(HashMap::new())),
            default_config: Default::default(),
        }
    }
}

impl WadmProvider {
    pub async fn run() -> anyhow::Result<()> {
        let host_data = load_host_data().context("failed to load host data")?;
        let provider = Self::from_host_data(host_data);
        let shutdown = run_provider(provider.clone(), "wadm-provider")
            .await
            .context("failed to run provider")?;
        let connection = get_connection();
        let wrpc = connection
            .get_wrpc_client(connection.provider_key())
            .await?;
        serve_provider_exports(&wrpc, provider, shutdown, bindings::serve)
            .await
            .context("failed to serve provider exports")
    }

    /// Build a [`WadmProvider`] from [`HostData`]
    pub fn from_host_data(host_data: &HostData) -> WadmProvider {
        let config = WadmConfig::try_from(host_data.config.clone());
        if let Ok(config) = config {
            WadmProvider {
                default_config: config,
                ..Default::default()
            }
        } else {
            warn!("Failed to build connection configuration, falling back to default");
            WadmProvider::default()
        }
    }

    /// Attempt to connect to nats url and create a wadm client
    /// If 'make_status_sub' is true, the client will subscribe to
    /// wadm status updates for this component
    async fn connect(
        &self,
        cfg: WadmConfig,
        component_id: &str,
        make_status_sub: bool,
    ) -> anyhow::Result<WadmClientBundle> {
        let ca_path: Option<PathBuf> = cfg.tls_ca_file.as_ref().map(PathBuf::from);
        let client_opts = ClientConnectOptions {
            url: cfg.cluster_uris.first().cloned(),
            seed: cfg.auth_seed.clone(),
            jwt: cfg.auth_jwt.clone(),
            creds_path: None,
            ca_path,
        };

        // Create the Wadm Client from the NATS client
        let client = Client::new(&cfg.lattice, None, client_opts).await?;
        // let client_arc = Arc::new(client);

        let mut sub_handles = Vec::new();
        if make_status_sub {
            let handle = self
                .handle_status(&client, component_id, &cfg.app_name)
                .await?;
            sub_handles.push(("wadm.status".into(), handle));
        }

        Ok(WadmClientBundle {
            client,
            sub_handles,
        })
    }

    /// Add a subscription to status events
    #[instrument(level = "debug", skip(self, client))]
    async fn handle_status(
        &self,
        client: &Client,
        component_id: &str,
        app_name: &str,
    ) -> anyhow::Result<AbortHandle> {
        debug!(?component_id, "spawning listener for component");
        let mut subscriber = client
            .subscribe_to_status(app_name)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to status: {}", e))?;

        let component_id = Arc::new(component_id.to_string());
        let app_name = Arc::new(app_name.to_string());

        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        tokio::task::spawn(Abortable::new(
            {
                let wrpc = match get_connection().get_wrpc_client(&component_id).await {
                    Ok(wrpc) => Arc::new(wrpc),
                    Err(err) => {
                        error!(?err, "failed to construct wRPC client");
                        return Err(anyhow::anyhow!("Failed to construct wRPC client: {}", err));
                    }
                };
                let semaphore = Arc::new(Semaphore::new(75));
                async move {
                    // Listen for NATS message(s)
                    while let Some(msg) = subscriber.next().await {
                        // Parse the message into a StatusResponse
                        match serde_json::from_slice::<StatusResponse>(&msg.payload) {
                            Ok(status_response) => match status_response.result {
                                StatusResult::Error => {
                                    warn!("Received error status: {}", status_response.message);
                                }
                                StatusResult::NotFound => {
                                    warn!("Status not found for: {}", app_name.clone());
                                }
                                StatusResult::Ok => {
                                    if let Some(status) = status_response.status {
                                        debug!(?status, ?component_id, "received status");

                                        let span =
                                            tracing::debug_span!("handle_message", ?component_id);
                                        let permit = match semaphore.clone().acquire_owned().await {
                                            Ok(p) => p,
                                            Err(_) => {
                                                warn!("Work pool has been closed, exiting queue subscribe");
                                                break;
                                            }
                                        };

                                        let component_id = Arc::clone(&component_id);
                                        let app_name = Arc::clone(&app_name);
                                        let wrpc = Arc::clone(&wrpc);
                                        tokio::spawn(async move {
                                            dispatch_status_update(
                                                &wrpc,
                                                component_id.as_str(),
                                                &app_name,
                                                status.into(),
                                                permit,
                                            )
                                            .instrument(span)
                                            .await;
                                        });
                                    } else {
                                        warn!("Received status OK but no status provided");
                                    }
                                }
                            },
                            Err(e) => {
                                warn!("Failed to deserialize message: {}", e);
                            }
                        };
                    }
                }
            },
            abort_registration,
        ));

        Ok(abort_handle)
    }

    /// Helper function to get the NATS client from the context
    async fn get_client(&self, ctx: Option<Context>) -> anyhow::Result<Client> {
        if let Some(ref source_id) = ctx
            .as_ref()
            .and_then(|Context { component, .. }| component.clone())
        {
            let actors = self.consumer_components.read().await;
            let wadm_bundle = match actors.get(source_id) {
                Some(wadm_bundle) => wadm_bundle,
                None => {
                    error!("actor not linked: {source_id}");
                    bail!("actor not linked: {source_id}")
                }
            };
            Ok(wadm_bundle.client.clone())
        } else {
            error!("no actor in request");
            bail!("no actor in request")
        }
    }
}

#[instrument(level = "debug", skip_all, fields(component_id = %component_id, app_name = %app))]
async fn dispatch_status_update(
    wrpc: &WrpcClient,
    component_id: &str,
    app: &str,
    status: Status,
    _permit: OwnedSemaphorePermit,
) {
    let update = StatusUpdate {
        app: app.to_string(),
        status,
    };
    debug!(
        app = app,
        component_id = component_id,
        "sending status to component",
    );

    let mut cx = async_nats::HeaderMap::new();
    for (k, v) in TraceContextInjector::default_with_span().iter() {
        cx.insert(k.as_str(), v.as_str())
    }

    if let Err(e) =
        bindings::wasmcloud::wadm::handler::handle_status_update(wrpc, Some(cx), &update).await
    {
        error!(
            error = %e,
            "Unable to send message"
        );
    }
}

impl Provider for WadmProvider {
    #[instrument(level = "debug", skip_all, fields(source_id))]
    async fn receive_link_config_as_target(
        &self,
        LinkConfig {
            source_id, config, ..
        }: LinkConfig<'_>,
    ) -> anyhow::Result<()> {
        let config = if config.is_empty() {
            self.default_config.clone()
        } else {
            match WadmConfig::try_from(config.clone()) {
                Ok(cc) => self.default_config.merge(&WadmConfig { ..cc }),
                Err(e) => {
                    error!("Failed to build WADM configuration: {e:?}");
                    return Err(anyhow!(e).context("failed to build WADM config"));
                }
            }
        };

        let mut update_map = self.consumer_components.write().await;
        let bundle = match self.connect(config, source_id, false).await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to connect to NATS: {e:?}");
                bail!(anyhow!(e).context("failed to connect to NATS"))
            }
        };
        update_map.insert(source_id.into(), bundle);

        Ok(())
    }

    #[instrument(level = "debug", skip_all, fields(target_id))]
    async fn receive_link_config_as_source(
        &self,
        LinkConfig {
            target_id, config, ..
        }: LinkConfig<'_>,
    ) -> anyhow::Result<()> {
        let config = if config.is_empty() {
            self.default_config.clone()
        } else {
            // create a config from the supplied values and merge that with the existing default
            match WadmConfig::try_from(config.clone()) {
                Ok(cc) => self.default_config.merge(&cc),
                Err(e) => {
                    error!("Failed to build connection configuration: {e:?}");
                    return Err(anyhow!(e).context("failed to build connection config"));
                }
            }
        };

        let mut update_map = self.handler_components.write().await;
        let bundle = match self.connect(config, target_id, true).await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to connect to NATS: {e:?}");
                bail!(anyhow!(e).context("failed to connect to NATS"))
            }
        };
        update_map.insert(target_id.into(), bundle);

        Ok(())
    }

    #[instrument(level = "info", skip_all, fields(target_id = info.get_source_id()))]
    async fn delete_link_as_source(&self, info: impl LinkDeleteInfo) -> anyhow::Result<()> {
        let component_id = info.get_target_id();
        let mut links = self.handler_components.write().await;
        if let Some(bundle) = links.remove(component_id) {
            debug!(
                    "dropping Wadm client and associated subscriptions [{}] for (handler) component [{}]...",
                    &bundle.sub_handles.len(),
                    component_id
                );
        }

        debug!(
            "finished processing (handler) link deletion for component [{}]",
            component_id
        );

        Ok(())
    }

    #[instrument(level = "info", skip_all, fields(source_id = info.get_source_id()))]
    async fn delete_link_as_target(&self, info: impl LinkDeleteInfo) -> anyhow::Result<()> {
        let component_id = info.get_source_id();
        let mut links = self.consumer_components.write().await;
        if let Some(bundle) = links.remove(component_id) {
            debug!(
                    "dropping Wadm client and associated subscriptions [{}] for (consumer) component [{}]...",
                    &bundle.sub_handles.len(),
                    component_id
                );
        }

        debug!(
            "finished processing (consumer) link deletion for component [{}]",
            component_id
        );

        Ok(())
    }

    /// Handle shutdown request by closing all connections
    async fn shutdown(&self) -> anyhow::Result<()> {
        // clear the handler components
        let mut handlers = self.handler_components.write().await;
        handlers.clear();

        // clear the consumer components
        let mut consumers = self.consumer_components.write().await;
        consumers.clear();

        // dropping all connections should send unsubscribes and close the connections, so no need
        // to handle that here
        Ok(())
    }
}

impl bindings::exports::wasmcloud::wadm::client::Handler<Option<Context>> for WadmProvider {
    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn deploy_model(
        &self,
        ctx: Option<Context>,
        model_name: String,
        version: Option<String>,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<String, String>> {
        let client = self.get_client(ctx).await?;
        match client
            .deploy_manifest(&model_name, version.as_deref())
            .await
        {
            Ok((name, _version)) => Ok(Ok(name)),
            Err(err) => {
                error!("Deployment failed: {err}");
                Ok(Err(format!("Deployment failed: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn undeploy_model(
        &self,
        ctx: Option<Context>,
        model_name: String,
        lattice: Option<String>,
        non_destructive: bool,
    ) -> anyhow::Result<Result<(), String>> {
        let client = self.get_client(ctx).await?;
        match client.undeploy_manifest(&model_name).await {
            Ok(_) => Ok(Ok(())),
            Err(err) => {
                error!("Undeployment failed: {err}");
                Ok(Err(format!("Undeployment failed: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model = %model))]
    async fn put_model(
        &self,
        ctx: Option<Context>,
        model: String,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<(String, String), String>> {
        let client = self.get_client(ctx).await?;
        match client.put_manifest(&model).await {
            Ok(response) => Ok(Ok(response)),
            Err(err) => {
                error!("Failed to store model: {err}");
                Ok(Err(format!("Failed to store model: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(manifest = ?manifest))]
    async fn put_manifest(
        &self,
        ctx: Option<Context>,
        manifest: OamManifest,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<(String, String), String>> {
        let client = self.get_client(ctx).await?;

        // Serialize the OamManifest into bytes
        let manifest_bytes =
            serde_json::to_vec(&manifest).context("Failed to serialize OAM manifest")?;

        // Convert the bytes into a string
        let manifest_string = String::from_utf8(manifest_bytes)
            .context("Failed to convert OAM manifest bytes to string")?;

        match client.put_manifest(&manifest_string).await {
            Ok(response) => Ok(Ok(response)),
            Err(err) => {
                error!("Failed to store manifest: {err}");
                Ok(Err(format!("Failed to store manifest: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn get_model_history(
        &self,
        ctx: Option<Context>,
        model_name: String,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<Vec<VersionInfo>, String>> {
        let client = self.get_client(ctx).await?;
        match client.list_versions(&model_name).await {
            Ok(history) => {
                // Use map to convert each item in the history list
                let converted_history: Vec<_> =
                    history.into_iter().map(|item| item.into()).collect();
                Ok(Ok(converted_history))
            }
            Err(err) => {
                error!("Failed to retrieve model history: {err}");
                Ok(Err(format!("Failed to retrieve model history: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn get_model_status(
        &self,
        ctx: Option<Context>,
        model_name: String,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<Status, String>> {
        let client = self.get_client(ctx).await?;
        match client.get_manifest_status(&model_name).await {
            Ok(status) => Ok(Ok(status.into())),
            Err(err) => {
                error!("Failed to retrieve model status: {err}");
                Ok(Err(format!("Failed to retrieve model status: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn get_model_details(
        &self,
        ctx: Option<Context>,
        model_name: String,
        version: Option<String>,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<OamManifest, String>> {
        let client = self.get_client(ctx).await?;
        match client.get_manifest(&model_name, version.as_deref()).await {
            Ok(details) => Ok(Ok(details.into())),
            Err(err) => {
                error!("Failed to retrieve model details: {err}");
                Ok(Err(format!("Failed to retrieve model details: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx), fields(model_name = %model_name))]
    async fn delete_model_version(
        &self,
        ctx: Option<Context>,
        model_name: String,
        version: Option<String>,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<bool, String>> {
        let client = self.get_client(ctx).await?;
        match client
            .delete_manifest(&model_name, version.as_deref())
            .await
        {
            Ok(response) => Ok(Ok(response)),
            Err(err) => {
                error!("Failed to delete model version: {err}");
                Ok(Err(format!("Failed to delete model version: {err}")))
            }
        }
    }

    #[instrument(level = "debug", skip(self, ctx))]
    async fn get_models(
        &self,
        ctx: Option<Context>,
        lattice: Option<String>,
    ) -> anyhow::Result<Result<Vec<ModelSummary>, String>> {
        let client = self.get_client(ctx).await?;
        match client.list_manifests().await {
            Ok(models) => Ok(Ok(models.into_iter().map(|model| model.into()).collect())),
            Err(err) => {
                error!("Failed to retrieve models: {err}");
                Ok(Err(format!("Failed to retrieve models: {err}")))
            }
        }
    }
}
