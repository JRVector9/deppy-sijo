//! 실제 암호 채널과 싱크를 잇는 재접속 회귀. 실행 앱은 띄우지 않는다.
use super::*;
use web_remote::relay::contract::{ConnectionId, DeviceId, PairingId, RelayPermissions};
use web_remote::relay::crypto::{
    PendingHandshake, RELAY_PROTOCOL_VERSION, RelayIdentity, RelayRole,
};
use web_remote::relay::repository::{
    ApprovalResult, PendingInsert, PendingRelayDevice, RelayDeviceRecord, RelayRepository,
    RevocationResult,
};
use web_remote::relay_client::{RelayFrameSink, SinkOutcome};

struct Repository {
    device: std::sync::Mutex<Option<RelayDeviceRecord>>,
    verifier: std::sync::Mutex<Option<[u8; 32]>>,
}

impl RelayRepository for Repository {
    fn insert_pending(&self, _: PendingRelayDevice, _: u64) -> anyhow::Result<PendingInsert> {
        anyhow::bail!("unused")
    }
    fn approve_pending(&self, _: PairingId, _: u64) -> anyhow::Result<ApprovalResult> {
        anyhow::bail!("unused")
    }
    fn pending_count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    fn device(&self, id: DeviceId) -> anyhow::Result<Option<RelayDeviceRecord>> {
        Ok(self
            .device
            .lock()
            .unwrap()
            .clone()
            .filter(|device| device.device_id() == id))
    }
    fn list_devices(&self, _: usize) -> anyhow::Result<Vec<RelayDeviceRecord>> {
        Ok(self.device.lock().unwrap().clone().into_iter().collect())
    }
    fn revoke_device(&self, _: DeviceId, _: u64) -> anyhow::Result<RevocationResult> {
        self.device.lock().unwrap().take();
        Ok(RevocationResult::Revoked)
    }
    fn touch_device(&self, _: DeviceId, _: u64) -> anyhow::Result<bool> {
        Ok(true)
    }
    fn store_reconnect_verifier(
        &self,
        id: DeviceId,
        key: &[u8; 65],
        verifier: &[u8; 32],
        now: u64,
    ) -> anyhow::Result<bool> {
        if !self
            .device(id)?
            .is_some_and(|device| device.is_admitted(key, now))
        {
            return Ok(false);
        }
        *self.verifier.lock().unwrap() = Some(*verifier);
        Ok(true)
    }
    fn reconnect_verifier(&self, _: DeviceId) -> anyhow::Result<Option<[u8; 32]>> {
        Ok(*self.verifier.lock().unwrap())
    }
}

fn fixture(known: bool) -> (RelayDashboardSink, Arc<Repository>) {
    let now = unix_now_secs();
    let desktop_identity = RelayIdentity::generate().unwrap();
    let device_identity = RelayIdentity::generate().unwrap();
    let record = |id, key| {
        RelayDeviceRecord::new(
            DeviceId::from_bytes([id; 16]),
            key,
            "phone".into(),
            RelayPermissions::default(),
            now - 1,
            now + 3600,
            None,
            None,
        )
        .unwrap()
    };
    let device = record(2, *device_identity.public_key_sec1());
    let desktop_record = record(1, *desktop_identity.public_key_sec1());
    let connection = ConnectionId::from_bytes([3; 16]);
    let desktop = PendingHandshake::begin(
        desktop_identity,
        device.identity_public_sec1().to_vec(),
        RelayRole::Desktop,
        RELAY_PROTOCOL_VERSION,
        connection,
    )
    .unwrap();
    let browser = PendingHandshake::begin(
        device_identity,
        desktop_record.identity_public_sec1().to_vec(),
        RelayRole::Device,
        RELAY_PROTOCOL_VERSION,
        connection,
    )
    .unwrap();
    let browser_hello = browser.sign_peer_offer(desktop.offer()).unwrap();
    let desktop_channel = desktop
        .finish(browser_hello)
        .unwrap()
        .confirm_admitted(&device, now)
        .unwrap();
    let repository = Arc::new(Repository {
        device: std::sync::Mutex::new(Some(device.clone())),
        verifier: std::sync::Mutex::new(None),
    });
    let route = relay_protocol::RouteId::from_bytes([4; 16]);
    let mut sink = RelayDashboardSink::new(
        web_remote::session_core::SessionCore::spawn(None),
        route,
        relay_protocol::AdmissionCredential::from_bytes([5; 32]),
        Box::new(RelayIdentity::generate),
        Arc::new(RelayMailbox::default()),
        repository.clone(),
    );
    sink.activate(desktop_channel, device, known);
    (sink, repository)
}

#[test]
fn relay_reconnect_registration_requires_publication_ack_before_dashboard() {
    let (mut sink, repository) = fixture(false);
    let frames = sink.drain_outbound();
    assert_eq!(frames.len(), 1);
    let (registered, _) = relay_protocol::RelayFrame::decode(&frames[0]).unwrap();
    assert_eq!(
        registered.frame_type(),
        relay_protocol::FrameType::Ciphertext
    );
    assert!(!sink.registration_ready);
    assert!(!String::from_utf8_lossy(registered.payload()).contains("relay_registered"));
    let request =
        serde_json::json!({"type":"relay_register", "version":2, "verifier":"07".repeat(32)});
    assert!(matches!(
        sink.command(request.to_string().as_bytes()),
        SinkOutcome::Continue
    ));
    assert_eq!(*repository.verifier.lock().unwrap(), Some([7; 32]));
    let frames = sink.drain_outbound();
    assert_eq!(frames.len(), 1, "게시 중에는 dashboard를 내보내지 않는다");
    let (publish, _) = relay_protocol::RelayFrame::decode(&frames[0]).unwrap();
    assert_eq!(
        publish.frame_type(),
        relay_protocol::FrameType::ReconnectPublish
    );
    let ack = relay_protocol::RelayFrame::new(
        relay_protocol::FrameType::ReconnectPublished,
        publish.route_id(),
        publish.connection_id(),
        0,
        &[7; 32],
    )
    .unwrap()
    .to_vec();
    assert!(matches!(sink.accept(&ack), SinkOutcome::Continue));
    let ready = sink.drain_outbound();
    assert_eq!(
        relay_protocol::RelayFrame::decode(&ready[0])
            .unwrap()
            .0
            .frame_type(),
        relay_protocol::FrameType::Ciphertext
    );
    assert!(sink.registration_ready);
}

#[test]
fn relay_reconnect_revocation_blocks_commands_and_already_queued_output() {
    let (mut sink, repository) = fixture(true);
    repository.device.lock().unwrap().take();
    assert!(matches!(
        sink.command(br#"{"type":"request_keyframe"}"#),
        SinkOutcome::CloseChannel
    ));
    assert!(
        sink.drain_outbound().is_empty(),
        "취소 후 대기 중 ready도 내보내지 않는다"
    );
    assert!(!sink.gate.is_active());
}

#[test]
fn relay_reconnect_changed_key_permissions_and_expiry_block_active_output() {
    for scenario in 0..3 {
        let (mut sink, repository) = fixture(true);
        let current = repository.device.lock().unwrap().clone().unwrap();
        let now = unix_now_secs();
        let key = if scenario == 0 {
            *RelayIdentity::generate().unwrap().public_key_sec1()
        } else {
            *current.identity_public_sec1()
        };
        let permissions = if scenario == 1 {
            RelayPermissions::new(true, true, false)
        } else {
            current.permissions()
        };
        *repository.device.lock().unwrap() = Some(
            RelayDeviceRecord::new(
                current.device_id(),
                key,
                "phone".into(),
                permissions,
                now - 10,
                if scenario == 2 { now } else { now + 3600 },
                None,
                None,
            )
            .unwrap(),
        );
        assert!(matches!(
            sink.command(br#"{"type":"request_keyframe"}"#),
            SinkOutcome::CloseChannel
        ));
        assert!(
            sink.drain_outbound().is_empty(),
            "인증 뒤 변경도 송신 경계에서 차단한다"
        );
        assert!(!sink.gate.is_active());
    }
}

#[test]
fn relay_reconnect_republishes_only_current_device_verifiers() {
    let (mut sink, repository) = fixture(true);
    sink.control_outbound.clear();
    *repository.verifier.lock().unwrap() = Some([8; 32]);
    assert!(sink.republish_grants());
    assert_eq!(sink.control_outbound.len(), 2);
    let (frame, _) = relay_protocol::RelayFrame::decode(&sink.control_outbound[0]).unwrap();
    assert_eq!(
        frame.frame_type(),
        relay_protocol::FrameType::ReconnectPublish
    );
    assert_eq!(&frame.payload()[..32], &[8; 32]);
    assert_eq!(
        relay_protocol::RelayFrame::decode(&sink.control_outbound[1])
            .unwrap()
            .0
            .frame_type(),
        relay_protocol::FrameType::ReconnectSync
    );
    sink.control_outbound.clear();
    repository.device.lock().unwrap().take();
    assert!(sink.republish_grants());
    assert_eq!(sink.control_outbound.len(), 1);
    assert_eq!(
        relay_protocol::RelayFrame::decode(&sink.control_outbound[0])
            .unwrap()
            .0
            .frame_type(),
        relay_protocol::FrameType::ReconnectSync
    );
}
