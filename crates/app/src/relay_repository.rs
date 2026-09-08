//! 프로덕션 Relay 영속 어댑터 — SQLite를 소유하는 유일한 계층.
//!
//! `web-remote`는 저장소 중립 포트(`RelayRepository`)와 값 타입만 정의한다. 실제
//! `storage::Db`는 앱이 소유하고, 모든 행은 여기서 **검증된 공개 생성자**를 통과해야
//! 레코드가 된다. SQLite는 길이와 `0x04` 접두사까지만 볼 수 있으므로 P-256 곡선 위의
//! 점인지는 이 어댑터가 확인한다 — 승인(변형) 이전에.
//!
//! 이 모듈은 Tailscale·loopback 서버·protocol-v3 인증 상태를 전혀 건드리지 않는다.
//! Keychain 거부는 Relay만 비활성화하는 오류이며 앱 기동을 막지 않는다.
//! 기동 배선(App 부팅 경로 연결)은 Task 4의 몫이다 — 여기서는 어댑터만 제공한다.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use secret::SecretStore;
use web_remote::relay::contract::{DeviceId, PairingId, RelayAction, RelayPermissions};
use web_remote::relay::crypto::RelayIdentity;
use web_remote::relay::repository::{
    ApprovalResult, PendingInsert, PendingRelayDevice, RelayDeviceRecord, RelayRepository,
    RevocationResult, get_or_create_relay_identity, validate_public_identity,
};

pub struct AppRelayRepository {
    db: Mutex<storage::Db>,
}

impl AppRelayRepository {
    /// 앱 데이터 디렉터리의 메타데이터 DB를 열고, 이전 프로세스가 남긴 pending 행을
    /// 전부 지운다. 검증된 메모리 승인(`PairingApproval`)은 프로세스와 함께 사라지므로
    /// 살아남은 pending 행은 페어링 증거가 될 수 없다 — 되살아나기 전에 지운다.
    ///
    /// 단일 인스턴스 lock을 요구한다. 두 프로세스가 동시에 pending을 지우고 쓰면 한쪽의
    /// 진행 중인 의식이 다른 쪽에 지워진다. 식별키 생성과 같은 이유로 타입으로 강제한다.
    pub fn open(metadata_db_path: &Path, _run_lock: &persist::LockFile) -> anyhow::Result<Self> {
        let db = storage::Db::open(metadata_db_path).context("Relay 저장소 열기 실패")?;
        let purged = db
            .delete_all_relay_pending_devices()
            .context("Relay pending 재시작 정리 실패")?;
        if purged > 0 {
            tracing::info!(purged, "이전 실행이 남긴 Relay pending 페어링을 정리했다");
        }
        Ok(Self { db: Mutex::new(db) })
    }

    fn locked(&self) -> anyhow::Result<std::sync::MutexGuard<'_, storage::Db>> {
        self.db
            .lock()
            .map_err(|_| anyhow::anyhow!("Relay 저장소 잠금이 오염됐다"))
    }
}

/// Relay 개인 식별키 생성은 앱의 단일 인스턴스 lock(`deppy.lock`)을 잡은 뒤에만 허용한다.
/// 두 프로세스가 동시에 Keychain 슬롯을 만들면 나중 것이 앞선 기기 인가를 무효화한다.
/// lock 참조를 인자로 요구해 이 순서를 컴파일 타임에 강제한다.
/// Keychain 거부는 여기서 Relay 범위 오류로만 올라간다 — 호출자는 Relay를 끄면 되고
/// Tailscale·앱 기동은 그대로 둔다.
pub fn create_relay_identity_after_single_instance_lock(
    _run_lock: &persist::LockFile,
    store: &dyn SecretStore,
) -> anyhow::Result<RelayIdentity> {
    get_or_create_relay_identity(store)
}

/// 공급자 생성 자체는 Keychain을 읽지 않는다. 명시적으로 켠 Relay의 기기 핸드셰이크가
/// 호출할 때만 접근하며, 앱 시작/OFF 경로도 같은 공급자를 사용해 이 경계를 검증한다.
pub fn relay_identity_supplier(
    run_lock: Arc<persist::LockFile>,
    store: Arc<dyn SecretStore>,
) -> web_remote::relay_client::RelayIdentitySupplier {
    Box::new(move || create_relay_identity_after_single_instance_lock(&run_lock, store.as_ref()))
}

impl RelayRepository for AppRelayRepository {
    fn insert_pending(
        &self,
        pending: PendingRelayDevice,
        trusted_now: u64,
    ) -> anyhow::Result<PendingInsert> {
        let lifetime = pending.lifetime();
        let row = storage::RelayPendingDeviceRow {
            pairing_id: *pending.pairing_id().as_bytes(),
            device_id: *pending.device_id().as_bytes(),
            identity_public_sec1: *pending.identity_public_sec1(),
            display_name: pending.display_name().to_owned(),
            permission_view: pending.permissions().allows(RelayAction::View),
            permission_input: pending.permissions().allows(RelayAction::Input),
            permission_upload: pending.permissions().allows(RelayAction::Upload),
            permission_approval: pending.permissions().allows(RelayAction::Approval),
            issued_at: i64::try_from(lifetime.issued_at())?,
            pairing_expires_at: i64::try_from(lifetime.pairing_expires_at())?,
            device_expires_at: i64::try_from(lifetime.device_expires_at())?,
        };
        Ok(
            match self
                .locked()?
                .insert_relay_pending_device(&row, i64::try_from(trusted_now)?)?
            {
                storage::RelayPendingInsert::Stored => PendingInsert::Stored,
                storage::RelayPendingInsert::LimitReached => PendingInsert::PendingLimitReached,
                storage::RelayPendingInsert::Conflict => PendingInsert::Conflict,
            },
        )
    }

    /// 승인은 기기를 발행하는 유일한 변형이다. 그러므로 **변형 이전에** 저장된 pending
    /// 행을 그대로 읽어 공개키·표시 이름을 검증한다. 변조된 행이면 아무것도 승인하지
    /// 않고 실패한다(fail-closed).
    fn approve_pending(
        &self,
        pairing_id: PairingId,
        approved_at: u64,
    ) -> anyhow::Result<ApprovalResult> {
        let db = self.locked()?;
        let Some(stored) = db.relay_pending_device(pairing_id.as_bytes())? else {
            return Ok(ApprovalResult::NotFound);
        };
        validate_public_identity(&stored.identity_public_sec1, &stored.display_name)
            .context("Relay pending 행이 유효한 기기 신원이 아니다")?;

        // 검증한 바로 그 공개키를 승인 트랜잭션에 함께 넘긴다. 검증과 발행 사이에 행이
        // 바뀌면 저장 계층이 같은 스냅샷 안에서 걸러 아무것도 발행하지 않는다.
        match db.approve_relay_pending_device(
            pairing_id.as_bytes(),
            &stored.identity_public_sec1,
            i64::try_from(approved_at)?,
        )? {
            storage::RelayDeviceApproval::Approved(row) => {
                Ok(ApprovalResult::Approved(device_from_row(row)?))
            }
            storage::RelayDeviceApproval::NotFound => Ok(ApprovalResult::NotFound),
            storage::RelayDeviceApproval::Expired => Ok(ApprovalResult::Expired),
            storage::RelayDeviceApproval::DeviceLimitReached => {
                Ok(ApprovalResult::DeviceLimitReached)
            }
        }
    }

    fn pending_count(&self) -> anyhow::Result<usize> {
        self.locked()?.relay_pending_device_count()
    }

    fn device(&self, device_id: DeviceId) -> anyhow::Result<Option<RelayDeviceRecord>> {
        self.locked()?
            .relay_device(device_id.as_bytes())?
            .map(device_from_row)
            .transpose()
    }

    fn list_devices(&self, limit: usize) -> anyhow::Result<Vec<RelayDeviceRecord>> {
        self.locked()?
            .list_relay_devices_bounded(limit)?
            .into_iter()
            .map(device_from_row)
            .collect()
    }

    fn revoke_device(
        &self,
        device_id: DeviceId,
        revoked_at: u64,
    ) -> anyhow::Result<RevocationResult> {
        Ok(
            match self
                .locked()?
                .revoke_relay_device(device_id.as_bytes(), i64::try_from(revoked_at)?)?
            {
                storage::RelayDeviceRevocation::Revoked => RevocationResult::Revoked,
                storage::RelayDeviceRevocation::NotFound => RevocationResult::NotFound,
            },
        )
    }

    fn store_reconnect_verifier(
        &self,
        device_id: DeviceId,
        identity: &[u8; 65],
        verifier: &[u8; 32],
        now: u64,
    ) -> anyhow::Result<bool> {
        let db = self.locked()?;
        let Some(row) = db.relay_device(device_id.as_bytes())? else {
            return Ok(false);
        };
        let device = device_from_row(row)?;
        if !device.is_admitted(identity, now) {
            return Ok(false);
        }
        db.store_relay_reconnect_verifier(
            device_id.as_bytes(),
            identity,
            verifier,
            i64::try_from(now)?,
        )
    }

    fn reconnect_verifier(&self, device_id: DeviceId) -> anyhow::Result<Option<[u8; 32]>> {
        self.locked()?
            .relay_reconnect_verifier(device_id.as_bytes())
    }

    fn touch_device(&self, device_id: DeviceId, seen_at: u64) -> anyhow::Result<bool> {
        self.locked()?
            .touch_relay_device(device_id.as_bytes(), i64::try_from(seen_at)?)
    }
}

/// 모든 행 → 레코드 변환은 이 한 곳을 지난다. 곡선 검증·이름·수명 검사는
/// `RelayDeviceRecord::new`가 수행하고, 실패는 그대로 위로 올린다.
fn device_from_row(row: storage::RelayDeviceRow) -> anyhow::Result<RelayDeviceRecord> {
    RelayDeviceRecord::new(
        DeviceId::from_bytes(row.device_id),
        row.identity_public_sec1,
        row.display_name,
        RelayPermissions::new(
            row.permission_view,
            row.permission_input,
            row.permission_approval,
        )
        .with_upload(row.permission_upload),
        u64::try_from(row.issued_at)?,
        u64::try_from(row.device_expires_at)?,
        row.last_seen_at.map(u64::try_from).transpose()?,
        row.revoked_at.map(u64::try_from).transpose()?,
    )
    .map(|device| device.with_authorization_epoch(row.authorization_epoch))
    .context("Relay 기기 행이 유효한 레코드가 아니다")
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_remote::relay::contract::ConnectionId;
    use web_remote::relay::crypto::{PendingHandshake, RELAY_PROTOCOL_VERSION, RelayRole};
    use web_remote::relay::pairing::{PairingApproval, PairingRegistry};
    use web_remote::relay::repository::{
        MAX_PENDING_RELAY_DEVICES, MAX_RELAY_DEVICES, RELAY_IDENTITY_SECRET_ID,
        RelayDeviceProposal, RelayPairingLifetime,
    };

    const ISSUED_AT: u64 = 1_800_000_000;
    const PAIRING_WINDOW_SECS: u64 = 300;
    const PAIRING_EXPIRES_AT: u64 = ISSUED_AT + PAIRING_WINDOW_SECS;
    const DEVICE_EXPIRES_AT: u64 = ISSUED_AT + 86_400;

    struct TempDir(std::path::PathBuf, std::sync::OnceLock<persist::LockFile>);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "deppy-relay-adapter-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path, std::sync::OnceLock::new())
        }

        fn db_path(&self) -> std::path::PathBuf {
            self.0.join("metadata.sqlite3")
        }

        /// 디렉터리당 lock 하나. 같은 경로를 두 번 잡으면 "이미 실행 중" 오류다 — 실제 앱과
        /// 같은 규칙이다.
        fn lock(&self) -> &persist::LockFile {
            self.1
                .get_or_init(|| persist::LockFile::acquire(&self.0.join("deppy.lock")).unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct MemStore(Mutex<std::collections::HashMap<String, String>>);

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &secret::SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(secret::SecretString::new)
                .context("missing")
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    struct DeniedStore;

    impl SecretStore for DeniedStore {
        fn set_secret(&self, _: &str, _: &secret::SecretString) -> anyhow::Result<()> {
            anyhow::bail!("keychain denied")
        }

        fn get_secret(&self, _: &str) -> anyhow::Result<secret::SecretString> {
            anyhow::bail!("keychain denied")
        }

        fn delete_secret(&self, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("keychain denied")
        }

        fn has_secret(&self, _: &str) -> anyhow::Result<bool> {
            anyhow::bail!("keychain denied")
        }
    }

    fn opaque_id(value: u16) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..2].copy_from_slice(&value.to_be_bytes());
        bytes
    }

    fn device_id(value: u16) -> DeviceId {
        DeviceId::from_bytes(opaque_id(value))
    }

    /// 길이 65 · `0x04` 접두사를 만족하지만 P-256 곡선 위에 없는 점 — SQLite CHECK는
    /// 통과하고 어댑터에서만 걸러진다.
    fn off_curve_public_key() -> [u8; 65] {
        let mut point = [0u8; 65];
        point[0] = 0x04;
        point[1..].fill(0x01);
        assert!(validate_public_identity(&point, "probe").is_err());
        point
    }

    fn lifetime() -> RelayPairingLifetime {
        RelayPairingLifetime::new(ISSUED_AT, PAIRING_EXPIRES_AT, DEVICE_EXPIRES_AT).unwrap()
    }

    /// 실제 서명된 핸드셰이크에서 얻은 검증 승인 하나와, 그 승인이 묶인 기기 공개키.
    /// `PairingBinding`은 web-remote 내부 생성자라 외부 크레이트는 이 경로로만 만든다.
    fn verified_approval(connection: u8) -> (PairingApproval, [u8; 65]) {
        let desktop_identity = RelayIdentity::generate().unwrap();
        let device_identity = RelayIdentity::generate().unwrap();
        let device_public = *device_identity.public_key_sec1();
        let connection = ConnectionId::from_bytes([connection; 16]);
        let desktop = PendingHandshake::begin(
            desktop_identity,
            device_public.to_vec(),
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let device = PendingHandshake::begin(
            device_identity,
            desktop.identity().public_key_sec1().to_vec(),
            RelayRole::Device,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let device_hello = device.sign_peer_offer(desktop.offer()).unwrap();
        let handshake = desktop.finish(device_hello).unwrap();

        let mut registry = PairingRegistry::new();
        let issued = registry.issue(ISSUED_AT).unwrap();
        registry
            .verify_secret_for_binding(
                issued.id(),
                issued.secret(),
                ISSUED_AT,
                handshake.pairing_binding(),
            )
            .unwrap();
        (
            registry.consume(issued.id(), ISSUED_AT).unwrap(),
            device_public,
        )
    }

    struct Paired {
        pairing_id: PairingId,
        device_id: DeviceId,
        public_key: [u8; 65],
    }

    /// 검증된 승인으로 pending 행 하나를 만들어 저장한다. 프로덕션과 같은 경로다 —
    /// 임의 생성자는 존재하지 않는다.
    fn insert(repository: &AppRelayRepository, value: u16, connection: u8) -> Paired {
        let (approval, public_key) = verified_approval(connection);
        let record = PendingRelayDevice::from_pairing_approval(
            &approval,
            RelayDeviceProposal {
                device_id: device_id(value),
                identity_public_sec1: public_key,
                display_name: format!("phone-{value}"),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
        )
        .unwrap();
        let pairing_id = record.pairing_id();
        assert_eq!(
            repository.insert_pending(record, ISSUED_AT).unwrap(),
            PendingInsert::Stored
        );
        Paired {
            pairing_id,
            device_id: device_id(value),
            public_key,
        }
    }

    #[test]
    fn relay_reconnect_verifier_survives_adapter_restart_and_revocation_erases_it() {
        let dir = TempDir::new("reconnect");
        let repository = AppRelayRepository::open(&dir.db_path(), dir.lock()).unwrap();
        let paired = insert(&repository, 5, 0x51);
        repository
            .approve_pending(paired.pairing_id, ISSUED_AT + 1)
            .unwrap();
        assert!(
            repository
                .store_reconnect_verifier(
                    paired.device_id,
                    &paired.public_key,
                    &[6; 32],
                    ISSUED_AT + 2
                )
                .unwrap()
        );
        drop(repository);
        let repository = AppRelayRepository::open(&dir.db_path(), dir.lock()).unwrap();
        assert_eq!(
            repository.reconnect_verifier(paired.device_id).unwrap(),
            Some([6; 32])
        );
        repository
            .revoke_device(paired.device_id, ISSUED_AT + 3)
            .unwrap();
        assert_eq!(
            repository.reconnect_verifier(paired.device_id).unwrap(),
            None
        );
        assert!(
            !repository
                .store_reconnect_verifier(
                    paired.device_id,
                    &paired.public_key,
                    &[6; 32],
                    ISSUED_AT + 4
                )
                .unwrap()
        );
    }

    #[test]
    fn relay_repository_admits_publishes_and_revokes_across_restart() {
        let dir = TempDir::new("round-trip");
        let path = dir.db_path();
        let paired = {
            let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();
            let paired = insert(&repository, 3, 0x31);
            let ApprovalResult::Approved(device) = repository
                .approve_pending(paired.pairing_id, ISSUED_AT + 1)
                .unwrap()
            else {
                panic!("승인이 기기를 발행해야 한다");
            };
            assert_eq!(device.device_id(), paired.device_id);
            assert_eq!(device.issued_at(), ISSUED_AT + 1);
            assert_eq!(device.device_expires_at(), DEVICE_EXPIRES_AT);
            assert!(device.permissions().allows(RelayAction::View));
            assert!(!device.permissions().allows(RelayAction::Input));
            assert_eq!(repository.pending_count().unwrap(), 0);
            assert!(
                repository
                    .touch_device(paired.device_id, ISSUED_AT + 2)
                    .unwrap()
            );
            paired
        };

        let reopened = AppRelayRepository::open(&path, dir.lock()).unwrap();
        let device = reopened.device(paired.device_id).unwrap().unwrap();
        assert!(device.is_admitted(&paired.public_key, ISSUED_AT + 3));
        assert_eq!(device.last_seen_at(), Some(ISSUED_AT + 2));
        assert_eq!(
            reopened
                .approve_pending(paired.pairing_id, ISSUED_AT + 3)
                .unwrap(),
            ApprovalResult::NotFound,
            "소비된 페어링은 재시작 후에도 되살아나지 않는다"
        );
        assert_eq!(reopened.list_devices(MAX_RELAY_DEVICES).unwrap().len(), 1);
        assert!(
            reopened.list_devices(0).is_err(),
            "상한을 넘는 스냅샷은 잘라서 주지 않고 거부한다"
        );

        assert_eq!(
            reopened
                .revoke_device(paired.device_id, ISSUED_AT + 4)
                .unwrap(),
            RevocationResult::Revoked
        );
        assert!(
            !reopened
                .device(paired.device_id)
                .unwrap()
                .unwrap()
                .is_admitted(&paired.public_key, ISSUED_AT + 4)
        );
        assert!(
            !reopened
                .touch_device(paired.device_id, ISSUED_AT + 5)
                .unwrap()
        );

        let after_revocation = AppRelayRepository::open(&path, dir.lock()).unwrap();
        assert!(
            !after_revocation
                .device(paired.device_id)
                .unwrap()
                .unwrap()
                .is_admitted(&paired.public_key, ISSUED_AT + 6),
            "취소는 재시작을 넘어 유지된다"
        );
    }

    #[test]
    fn pairing_deadline_and_device_expiry_stay_separate_through_the_adapter() {
        let dir = TempDir::new("split-expiry");
        let repository = AppRelayRepository::open(&dir.db_path(), dir.lock()).unwrap();

        let late = insert(&repository, 10, 0x32);
        assert_eq!(
            repository
                .approve_pending(late.pairing_id, PAIRING_EXPIRES_AT)
                .unwrap(),
            ApprovalResult::Expired,
            "정확히 페어링 마감 시각의 승인은 실패한다"
        );
        assert!(repository.device(late.device_id).unwrap().is_none());
        assert_eq!(repository.pending_count().unwrap(), 0);

        let in_time = insert(&repository, 11, 0x33);
        let ApprovalResult::Approved(device) = repository
            .approve_pending(in_time.pairing_id, PAIRING_EXPIRES_AT - 1)
            .unwrap()
        else {
            panic!("마감 직전 승인은 성공한다");
        };
        assert_eq!(device.device_expires_at(), DEVICE_EXPIRES_AT);
        assert!(
            device.is_admitted(&in_time.public_key, PAIRING_EXPIRES_AT + 1),
            "발행된 기기는 5분 페어링 마감보다 오래 산다"
        );
        assert!(!device.is_admitted(&in_time.public_key, DEVICE_EXPIRES_AT));
    }

    /// 변조된 pending 행: 65바이트 `0x04` 접두사지만 곡선 밖 점. SQLite CHECK는 통과하고
    /// 어댑터가 **승인(변형) 이전에** 거부해야 한다.
    #[test]
    fn off_curve_pending_row_is_rejected_before_any_approval_mutation() {
        let dir = TempDir::new("pending-corruption");
        let path = dir.db_path();
        let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();
        let paired = insert(&repository, 20, 0x34);

        tamper(&path, |connection| {
            let changed = connection
                .execute(
                    "UPDATE relay_pending_devices SET identity_public_sec1 = ?1",
                    [off_curve_public_key().as_slice()],
                )
                .unwrap();
            assert_eq!(changed, 1);
        });

        let error = repository
            .approve_pending(paired.pairing_id, ISSUED_AT + 1)
            .unwrap_err();
        assert!(format!("{error:#}").contains("Relay pending"), "{error:#}");
        assert!(
            repository.device(paired.device_id).unwrap().is_none(),
            "변조된 행은 기기를 발행하지 못한다"
        );
        assert_eq!(
            repository.pending_count().unwrap(),
            1,
            "실패한 승인은 pending 행을 소비하지 않는다"
        );
    }

    /// 변조된 기기 행은 읽기 자체가 실패해야 한다 — 무효 레코드가 admission 판단까지
    /// 도달하면 안 된다.
    #[test]
    fn off_curve_device_row_is_rejected_before_admission() {
        let dir = TempDir::new("device-corruption");
        let path = dir.db_path();
        let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();
        let paired = insert(&repository, 21, 0x35);
        repository
            .approve_pending(paired.pairing_id, ISSUED_AT + 1)
            .unwrap();

        tamper(&path, |connection| {
            let changed = connection
                .execute(
                    "UPDATE relay_devices SET identity_public_sec1 = ?1",
                    [off_curve_public_key().as_slice()],
                )
                .unwrap();
            assert_eq!(changed, 1);
        });

        assert!(repository.device(paired.device_id).is_err());
        assert!(repository.list_devices(MAX_RELAY_DEVICES).is_err());
    }

    #[test]
    fn opening_the_repository_purges_pending_rows_left_by_a_previous_process() {
        let dir = TempDir::new("restart-purge");
        let path = dir.db_path();
        {
            let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();
            for (index, value) in (30..33u16).enumerate() {
                insert(&repository, value, 0x40 + index as u8);
            }
            assert_eq!(repository.pending_count().unwrap(), 3);
        }

        let reopened = AppRelayRepository::open(&path, dir.lock()).unwrap();
        assert_eq!(
            reopened.pending_count().unwrap(),
            0,
            "검증된 메모리 승인이 사라진 뒤 남은 pending 행은 페어링 증거가 아니다"
        );
    }

    /// 상한 도달을 어댑터가 결정적으로 전달하는지 본다. 상한까지 채우는 행은 원시 SQL로
    /// 직접 넣는다 — 페어링 의식 수백 회를 돌리지 않고 경계만 재현하기 위해서다.
    /// SQL 자체의 상한 동작은 `storage` 테스트가 따로 고정한다.
    #[test]
    fn adapter_reports_pending_and_device_limits_without_evicting_rows() {
        let dir = TempDir::new("limits");
        let path = dir.db_path();
        let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();

        fill_pending(&path, MAX_PENDING_RELAY_DEVICES - 1);
        assert_eq!(
            repository.pending_count().unwrap(),
            MAX_PENDING_RELAY_DEVICES - 1
        );
        let last = insert(&repository, 50, 0x50);
        assert_eq!(
            repository.pending_count().unwrap(),
            MAX_PENDING_RELAY_DEVICES
        );

        let (approval, public_key) = verified_approval(0x51);
        let overflow = PendingRelayDevice::from_pairing_approval(
            &approval,
            RelayDeviceProposal {
                device_id: device_id(51),
                identity_public_sec1: public_key,
                display_name: "overflow".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
        )
        .unwrap();
        assert_eq!(
            repository.insert_pending(overflow, ISSUED_AT).unwrap(),
            PendingInsert::PendingLimitReached
        );
        assert_eq!(
            repository.pending_count().unwrap(),
            MAX_PENDING_RELAY_DEVICES,
            "상한 거부는 기존 행을 밀어내지 않는다"
        );

        fill_devices(&path, MAX_RELAY_DEVICES);
        assert_eq!(
            repository
                .approve_pending(last.pairing_id, ISSUED_AT + 1)
                .unwrap(),
            ApprovalResult::DeviceLimitReached
        );
        assert!(
            repository.device(last.device_id).unwrap().is_none(),
            "상한 거부는 기기를 발행하지 않는다"
        );
        assert_eq!(
            repository.pending_count().unwrap(),
            MAX_PENDING_RELAY_DEVICES,
            "상한 거부는 pending 행도 소비하지 않는다"
        );
    }

    #[test]
    fn a_future_dated_pending_row_is_rejected_against_the_trusted_clock() {
        let dir = TempDir::new("trusted-clock");
        let repository = AppRelayRepository::open(&dir.db_path(), dir.lock()).unwrap();
        insert(&repository, 40, 0x60);

        let future = RelayPairingLifetime::new(
            ISSUED_AT + 100,
            PAIRING_EXPIRES_AT + 100,
            DEVICE_EXPIRES_AT + 100,
        )
        .unwrap();
        let (approval, public_key) = verified_approval(0x61);
        let record = PendingRelayDevice::from_pairing_approval(
            &approval,
            RelayDeviceProposal {
                device_id: device_id(41),
                identity_public_sec1: public_key,
                display_name: "future phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: future,
            },
        )
        .unwrap();
        assert!(repository.insert_pending(record, ISSUED_AT).is_err());
        assert_eq!(
            repository.pending_count().unwrap(),
            1,
            "거부는 기존 행을 정리하지 않는다"
        );
    }

    #[test]
    fn relay_identity_creation_requires_the_single_instance_lock() {
        let dir = TempDir::new("identity-lock");
        let run_lock = dir.lock();
        let store = MemStore::default();

        let first = create_relay_identity_after_single_instance_lock(run_lock, &store).unwrap();
        let second = create_relay_identity_after_single_instance_lock(run_lock, &store).unwrap();
        assert_eq!(first.public_key_sec1(), second.public_key_sec1());
        assert_eq!(store.0.lock().unwrap().len(), 1);
        assert!(
            store
                .0
                .lock()
                .unwrap()
                .contains_key(RELAY_IDENTITY_SECRET_ID)
        );

        // 앱 크레이트 어디에서도 lock 없는 우회 생성 경로를 열어 두지 않는다.
        let call_sites = walk_app_sources()
            .into_iter()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .contains("get_or_create_relay_identity")
            })
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            call_sites,
            vec!["src/relay_repository.rs".to_owned()],
            "Relay 식별키 생성은 단일 인스턴스 lock을 요구하는 이 함수만 호출한다"
        );
    }

    /// Keychain 거부는 Relay 범위 오류로만 올라가고, 이 모듈은 Tailscale·loopback 서버
    /// 상태를 전혀 참조하지 않는다. 실제 기동 수명주기 격리 테스트는 Relay 클라이언트를
    /// 붙이는 Task 4의 몫이다 — Task 2는 기동 배선을 만들지 않는다.
    #[test]
    fn keychain_denial_is_relay_scoped_and_names_no_transport_state() {
        let dir = TempDir::new("keychain-denial");
        let run_lock = dir.lock();
        let error =
            create_relay_identity_after_single_instance_lock(run_lock, &DeniedStore).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("Relay identity"), "{rendered}");

        // Relay 저장소는 Keychain이 거부돼도 독립적으로 계속 열린다.
        let repository = AppRelayRepository::open(&dir.db_path(), dir.lock()).unwrap();
        assert_eq!(repository.pending_count().unwrap(), 0);

        // 앱의 다른 모듈을 하나도 import하지 않는다 — Tailscale·loopback 서버·App 상태에
        // 닿을 경로 자체가 없다는 뜻이다. (문서 주석은 불변식을 설명하므로 제외한다.)
        let code = production_source()
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<String>();
        for forbidden in [
            "use crate::",
            "crate::tailscale",
            "tailscale::",
            "WebRemoteServer",
            "ws_api",
            "WEB_TOKEN_ID",
            "TcpListener",
        ] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn the_relay_database_never_stores_identity_secrets_or_terminal_plaintext() {
        let dir = TempDir::new("plaintext-scan");
        let path = dir.db_path();
        let store = MemStore::default();
        let run_lock = dir.lock();
        let _identity = create_relay_identity_after_single_instance_lock(run_lock, &store).unwrap();
        {
            let repository = AppRelayRepository::open(&path, dir.lock()).unwrap();
            let paired = insert(&repository, 9, 0x70);
            repository
                .approve_pending(paired.pairing_id, ISSUED_AT + 1)
                .unwrap();
        }

        let bytes = std::fs::read(&path).unwrap();
        let stored_secret = store
            .0
            .lock()
            .unwrap()
            .get(RELAY_IDENTITY_SECRET_ID)
            .unwrap()
            .clone();
        assert!(
            !bytes
                .windows(stored_secret.len())
                .any(|window| window == stored_secret.as_bytes())
        );
        assert!(
            !bytes
                .windows(b"TERMINAL_PLAINTEXT_MARKER".len())
                .any(|window| window == b"TERMINAL_PLAINTEXT_MARKER")
        );

        let code = production_source()
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<String>();
        for forbidden in ["terminal_payload", "private_key", "PairingSecret"] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
    }

    fn production_source() -> &'static str {
        include_str!("relay_repository.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
    }

    /// 테스트에서만 쓰는 외부 변조 경로 — 공격자가 DB 파일을 직접 고치는 상황을
    /// 재현한다. 어댑터는 이 연결을 쓰지 않는다.
    fn tamper(path: &Path, mutate: impl FnOnce(&rusqlite::Connection)) {
        let connection = rusqlite::Connection::open(path).unwrap();
        mutate(&connection);
    }

    /// 상한 재현용 원시 채움. 곡선 검증을 거치지 않은 합성 공개키를 쓰지만 이 행들은
    /// 승인/조회 대상이 아니라 개수만 세어진다.
    fn fill_pending(path: &Path, count: usize) {
        tamper(path, |connection| {
            for index in 0..count {
                let synthetic = synthetic_id(index);
                connection
                    .execute(
                        "INSERT INTO relay_pending_devices (
                             pairing_id, device_id, identity_public_sec1, display_name,
                             permission_view, permission_input, permission_upload,
                             permission_approval, issued_at, pairing_expires_at, device_expires_at
                         ) VALUES (?1, ?2, ?3, ?4, 1, 0, 0, 0, ?5, ?6, ?7)",
                        rusqlite::params![
                            synthetic.as_slice(),
                            synthetic.as_slice(),
                            synthetic_key(index).as_slice(),
                            format!("filler-{index}"),
                            ISSUED_AT as i64,
                            PAIRING_EXPIRES_AT as i64,
                            DEVICE_EXPIRES_AT as i64,
                        ],
                    )
                    .unwrap();
            }
        });
    }

    fn fill_devices(path: &Path, count: usize) {
        tamper(path, |connection| {
            for index in 0..count {
                let synthetic = synthetic_id(index + 10_000);
                connection
                    .execute(
                        "INSERT INTO relay_devices (
                             device_id, identity_public_sec1, display_name,
                             permission_view, permission_input, permission_upload,
                             permission_approval, issued_at, device_expires_at,
                             last_seen_at, revoked_at
                         ) VALUES (?1, ?2, ?3, 1, 0, 0, 0, ?4, ?5, NULL, NULL)",
                        rusqlite::params![
                            synthetic.as_slice(),
                            synthetic_key(index + 10_000).as_slice(),
                            format!("filler-{index}"),
                            ISSUED_AT as i64,
                            DEVICE_EXPIRES_AT as i64,
                        ],
                    )
                    .unwrap();
            }
        });
    }

    fn synthetic_id(index: usize) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        bytes[8] = 0xAA;
        bytes
    }

    fn synthetic_key(index: usize) -> [u8; 65] {
        let mut bytes = [0x07u8; 65];
        bytes[0] = 0x04;
        bytes[1..9].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        bytes
    }

    fn walk_app_sources() -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![std::path::PathBuf::from("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }
}
