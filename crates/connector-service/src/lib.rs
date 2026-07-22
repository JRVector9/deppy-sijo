//! Lazy Connector coordinator, capability ports, and latest-only snapshots.
//!
//! This crate never owns a concrete database, keyring implementation, UI, runtime, or terminal.
//! The app composition root supplies repository/secrets factories; the coordinator opens the
//! repository only after the first real Connector command and drops it when its idle worker exits.

mod coordinator;
mod ports;
mod snapshot;

pub use connector_contract as contract;
pub use coordinator::{
    AppRequest, ConnectorCoordinator, ConnectorCoordinatorConfig, ConnectorServiceMetrics,
    CoordinatorClock, DispatchError, DispatchOutcome, OperationIdFactory, SystemCoordinatorClock,
    SystemOperationIdFactory,
};
pub use ports::{
    AuthorizationState, AuthorizedInvokeRequest, CancellationToken, ConnectorMcp, ConnectorOAuth,
    ConnectorRepository, ConnectorRepositoryFactory, ConnectorSecrets, DiscoverOutput,
    DiscoveredTool, ImportPlan, LiveToolSchema, McpTransportSnapshot, OAuthOutput, OverviewData,
    RepositoryToolPage, ServiceError, StoredOAuthClient, cancel_live_mcp_connection,
};
pub use snapshot::SnapshotReader;
