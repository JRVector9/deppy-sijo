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

#[derive(Default)]
struct KeychainSpy {
    calls: std::sync::atomic::AtomicUsize,
    values: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl secret::SecretStore for KeychainSpy {
    fn set_secret(&self, id: &str, value: &secret::SecretString) -> anyhow::Result<()> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.values
            .lock()
            .unwrap()
            .insert(id.into(), value.expose().into());
        Ok(())
    }
    fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.values
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map(secret::SecretString::new)
            .ok_or_else(|| anyhow::anyhow!("missing test key"))
    }
    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.values.lock().unwrap().contains_key(id))
    }
    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.values.lock().unwrap().remove(id);
        Ok(())
    }
    fn list_secret_ids(&self, _: &str) -> anyhow::Result<Vec<String>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[test]
fn relay_startup_and_off_never_access_the_keychain() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use web_remote::relay_client::{
        RelayEndpoint, RelayObserver, RelayState, RelayTransport, TransportError,
    };

    struct NoConnect(Arc<AtomicUsize>);
    impl RelayTransport for NoConnect {
        fn connect(
            &mut self,
            _: &RelayEndpoint,
            _: std::time::Duration,
        ) -> Result<Box<dyn web_remote::relay_client::RelaySession>, TransportError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Unavailable)
        }
    }
    struct Observer(std::sync::mpsc::Sender<RelayState>);
    impl RelayObserver for Observer {
        fn state_changed(&self, state: RelayState) {
            let _ = self.0.send(state);
        }
    }
    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir =
        TestDir(std::env::temp_dir().join(format!("deppy-relay-spy-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let run_lock = Arc::new(persist::LockFile::acquire(&dir.0.join("deppy.lock")).unwrap());
    let spy = Arc::new(KeychainSpy::default());
    let identity = crate::relay_repository::relay_identity_supplier(run_lock.clone(), spy.clone());
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        0,
        "공급자 생성은 키에 접근하지 않는다"
    );
    let core = web_remote::session_core::SessionCore::spawn(None);
    let repository = Arc::new(Repository {
        device: std::sync::Mutex::new(None),
        verifier: std::sync::Mutex::new(None),
    });
    let mut sink = RelayDashboardSink::new(
        core.clone(),
        relay_protocol::RouteId::from_bytes([4; 16]),
        relay_protocol::AdmissionCredential::from_bytes([5; 32]),
        identity,
        Arc::new(RelayMailbox::default()),
        repository,
    );
    // 시작 직후 라우트 입장만 처리할 때도 기기 키를 미리 읽지 않는다.
    sink.session_started();
    assert!(!sink.drain_outbound().is_empty());
    sink.session_ended();
    assert_eq!(spy.calls.load(Ordering::SeqCst), 0);
    let attempts = Arc::new(AtomicUsize::new(0));
    let (send, receive) = std::sync::mpsc::channel();
    let mut worker = web_remote::relay_client::RelayWorker::spawn(
        RelayEndpoint::parse("wss://relay.example.test").unwrap(),
        Box::new(NoConnect(attempts.clone())),
        Box::new(sink),
        Arc::new(Observer(send)),
        web_remote::relay_client::RelayDeadlines::default(),
        web_remote::relay_client::BackoffPolicy::default(),
    );
    assert!(worker.disable());
    assert_eq!(
        receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        RelayState::Halted(web_remote::relay_client::HaltReason::Disabled)
    );
    worker.shutdown();
    core.shutdown();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "OFF는 네트워크에도 접근하지 않는다"
    );
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        0,
        "OFF는 Keychain 접근0이다"
    );
    // 양성 대조: 같은 production supplier를 명시적으로 호출하면 spy가 접근을 관측한다.
    let identity = crate::relay_repository::relay_identity_supplier(run_lock, spy.clone());
    identity().unwrap();
    assert!(spy.calls.load(Ordering::SeqCst) > 0);
}

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
fn relay_reapproval_of_the_same_identity_closes_the_old_channel() {
    let (mut sink, repository) = fixture(true);
    let current = repository.device.lock().unwrap().clone().unwrap();
    *repository.device.lock().unwrap() = Some(
        RelayDeviceRecord::new(
            current.device_id(),
            *current.identity_public_sec1(),
            current.display_name().into(),
            current.permissions(),
            current.issued_at() + 1,
            current.device_expires_at() + 1,
            None,
            None,
        )
        .unwrap(),
    );
    assert!(matches!(
        sink.command(br#"{"type":"request_keyframe"}"#),
        SinkOutcome::CloseChannel
    ));
    assert!(sink.drain_outbound().is_empty());
}

#[test]
fn relay_same_clock_authorization_epoch_change_closes_the_old_channel() {
    let (mut sink, repository) = fixture(true);
    let current = repository.device.lock().unwrap().clone().unwrap();
    *repository.device.lock().unwrap() = Some(current.with_authorization_epoch([1; 16]));
    assert!(matches!(
        sink.command(br#"{"type":"request_keyframe"}"#),
        SinkOutcome::CloseChannel
    ));
    assert!(sink.drain_outbound().is_empty());
    assert!(!sink.gate.is_active());
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
