use pageserver_api::{models::*, shard::TenantShardId};
use reqwest::{IntoUrl, Method};
use utils::{
    http::error::HttpErrorBody,
    id::{TenantId, TimelineId},
};

pub mod util;

#[derive(Debug)]
pub struct Client {
    mgmt_api_endpoint: String,
    authorization_header: Option<String>,
    client: reqwest::Client,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("receive body: {0}")]
    ReceiveBody(reqwest::Error),

    #[error("receive error body: {0}")]
    ReceiveErrorBody(String),

    #[error("pageserver API: {0}")]
    ApiError(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait ResponseErrorMessageExt: Sized {
    async fn error_from_body(self) -> Result<Self>;
}

impl ResponseErrorMessageExt for reqwest::Response {
    async fn error_from_body(self) -> Result<Self> {
        let status = self.status();
        if !(status.is_client_error() || status.is_server_error()) {
            return Ok(self);
        }

        let url = self.url().to_owned();
        Err(match self.json::<HttpErrorBody>().await {
            Ok(HttpErrorBody { msg }) => Error::ApiError(msg),
            Err(_) => {
                Error::ReceiveErrorBody(format!("Http error ({}) at {}.", status.as_u16(), url))
            }
        })
    }
}

pub enum ForceAwaitLogicalSize {
    Yes,
    No,
}

impl Client {
    pub fn new(mgmt_api_endpoint: String, jwt: Option<&str>) -> Self {
        Self {
            mgmt_api_endpoint,
            authorization_header: jwt.map(|jwt| format!("Bearer {jwt}")),
            client: reqwest::Client::new(),
        }
    }

    pub async fn list_tenants(&self) -> Result<Vec<pageserver_api::models::TenantInfo>> {
        let uri = format!("{}/v1/tenant", self.mgmt_api_endpoint);
        let resp = self.get(&uri).await?;
        resp.json().await.map_err(Error::ReceiveBody)
    }

    pub async fn tenant_details(
        &self,
        tenant_id: TenantId,
    ) -> Result<pageserver_api::models::TenantDetails> {
        let uri = format!("{}/v1/tenant/{tenant_id}", self.mgmt_api_endpoint);
        self.get(uri)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    pub async fn list_timelines(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<pageserver_api::models::TimelineInfo>> {
        let uri = format!("{}/v1/tenant/{tenant_id}/timeline", self.mgmt_api_endpoint);
        self.get(&uri)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    pub async fn timeline_info(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        force_await_logical_size: ForceAwaitLogicalSize,
    ) -> Result<pageserver_api::models::TimelineInfo> {
        let uri = format!(
            "{}/v1/tenant/{tenant_id}/timeline/{timeline_id}",
            self.mgmt_api_endpoint
        );

        let uri = match force_await_logical_size {
            ForceAwaitLogicalSize::Yes => format!("{}?force-await-logical-size={}", uri, true),
            ForceAwaitLogicalSize::No => uri,
        };

        self.get(&uri)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    pub async fn keyspace(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> Result<pageserver_api::models::partitioning::Partitioning> {
        let uri = format!(
            "{}/v1/tenant/{tenant_id}/timeline/{timeline_id}/keyspace",
            self.mgmt_api_endpoint
        );
        self.get(&uri)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    async fn get<U: IntoUrl>(&self, uri: U) -> Result<reqwest::Response> {
        self.request(Method::GET, uri, ()).await
    }

    async fn request<B: serde::Serialize, U: reqwest::IntoUrl>(
        &self,
        method: Method,
        uri: U,
        body: B,
    ) -> Result<reqwest::Response> {
        let req = self.client.request(method, uri);
        let req = if let Some(value) = &self.authorization_header {
            req.header(reqwest::header::AUTHORIZATION, value)
        } else {
            req
        };
        let res = req.json(&body).send().await.map_err(Error::ReceiveBody)?;
        let response = res.error_from_body().await?;
        Ok(response)
    }

    pub async fn status(&self) -> Result<()> {
        let uri = format!("{}/v1/status", self.mgmt_api_endpoint);
        self.get(&uri).await?;
        Ok(())
    }

    pub async fn tenant_create(&self, req: &TenantCreateRequest) -> Result<TenantId> {
        let uri = format!("{}/v1/tenant", self.mgmt_api_endpoint);
        self.request(Method::POST, &uri, req)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    pub async fn tenant_config(&self, req: &TenantConfigRequest) -> Result<()> {
        let uri = format!("{}/v1/tenant/config", self.mgmt_api_endpoint);
        self.request(Method::PUT, &uri, req).await?;
        Ok(())
    }

    pub async fn tenant_secondary_download(&self, tenant_id: TenantShardId) -> Result<()> {
        let uri = format!(
            "{}/v1/tenant/{}/secondary/download",
            self.mgmt_api_endpoint, tenant_id
        );
        self.request(Method::POST, &uri, ())
            .await?
            .error_for_status()
            .map(|_| ())
            .map_err(|e| Error::ApiError(format!("{}", e)))
    }

    pub async fn location_config(
        &self,
        tenant_id: TenantId,
        config: LocationConfig,
        flush_ms: Option<std::time::Duration>,
    ) -> Result<()> {
        let req_body = TenantLocationConfigRequest { tenant_id, config };
        let path = format!(
            "{}/v1/tenant/{}/location_config",
            self.mgmt_api_endpoint, tenant_id
        );
        let path = if let Some(flush_ms) = flush_ms {
            format!("{}?flush_ms={}", path, flush_ms.as_millis())
        } else {
            path
        };
        self.request(Method::PUT, &path, &req_body).await?;
        Ok(())
    }

    pub async fn timeline_create(
        &self,
        tenant_id: TenantId,
        req: &TimelineCreateRequest,
    ) -> Result<TimelineInfo> {
        let uri = format!(
            "{}/v1/tenant/{}/timeline",
            self.mgmt_api_endpoint, tenant_id
        );
        self.request(Method::POST, &uri, req)
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }

    pub async fn tenant_reset(&self, tenant_shard_id: TenantShardId) -> Result<()> {
        let uri = format!(
            "{}/v1/tenant/{}/reset",
            self.mgmt_api_endpoint, tenant_shard_id
        );
        self.request(Method::POST, &uri, ())
            .await?
            .json()
            .await
            .map_err(Error::ReceiveBody)
    }
}
