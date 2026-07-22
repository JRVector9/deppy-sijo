//! Lazy Connector coordinator, capability ports, and latest-only snapshots.
//!
//! This crate never owns a concrete database, keyring implementation, UI, runtime, or terminal.
//! The app composition root supplies repository/secrets factories; the coordinator opens the
//! repository only after the first real Connector command and drops it when its idle worker exits.

mod coordinator;
mod mcp_adapter;
mod oauth_adapter;
mod ports;
mod snapshot;

pub use connector_contract as contract;
pub use coordinator::{
    ConnectorCoordinator, ConnectorCoordinatorConfig, ConnectorHost, ConnectorServiceMetrics,
    CoordinatorClock, DispatchError, DispatchOutcome, HostAction, OperationIdFactory,
    SystemCoordinatorClock, SystemOperationIdFactory,
};
pub use mcp_adapter::ProductionConnectorMcp;
pub use oauth_adapter::ProductionConnectorOAuth;
pub use ports::{
    AuthorizationState, AuthorizedInvokeRequest, CancellationToken, ConnectorMcp, ConnectorOAuth,
    ConnectorRepository, ConnectorRepositoryFactory, ConnectorSecrets, CredentialResolutionRequest,
    DiscoverOutput, DiscoveredTool, HttpAuthBinding, ImportCandidate, ImportPlan,
    InvocationContext, LiveToolSchema, McpRequestTarget, McpTransportSnapshot,
    OAuthAuthorizeOutput, OAuthClientRequest, OAuthCompletion, OAuthContinuation,
    OAuthCredentialUpdate, OAuthDiscovery, OAuthEventSink, OAuthFailure, OAuthPublishDescriptor,
    OAuthPublishMode, OAuthPublishResult, OAuthRecoveryTarget, OAuthRefreshOutcome,
    OAuthRefreshRequest, Observed, OverviewData, RepositoryCas, RepositoryMcpTarget,
    RepositoryToolPage, ResolvedCredential, ResolvedCredentials, ServiceError, StoredOAuthClient,
    cancel_live_mcp_connection,
};
pub use snapshot::SnapshotReader;
