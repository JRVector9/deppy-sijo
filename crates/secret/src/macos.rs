//! 기존 User(login) Keychain 좌표를 유지하는 비대화형 SecItem 접근.
use anyhow::Context;
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::string::{CFString, CFStringRef};
use security_framework::os::macos::keychain::{SecKeychain, SecPreferencesDomain};

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecUseAuthenticationUI: CFStringRef;
    static kSecUseAuthenticationUIFail: CFStringRef;
    static kSecClass: CFStringRef;
    static kSecClassGenericPassword: CFStringRef;
    static kSecAttrService: CFStringRef;
    static kSecAttrAccount: CFStringRef;
    static kSecUseKeychain: CFStringRef;
    static kSecMatchSearchList: CFStringRef;
    static kSecReturnData: CFStringRef;
    static kSecReturnAttributes: CFStringRef;
    static kSecMatchLimit: CFStringRef;
    static kSecMatchLimitAll: CFStringRef;
    static kSecValueData: CFStringRef;
    fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> i32;
    fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> i32;
    fn SecItemUpdate(query: CFDictionaryRef, attributes: CFDictionaryRef) -> i32;
    fn SecItemDelete(query: CFDictionaryRef) -> i32;
}

#[derive(Clone, Copy)]
enum Operation {
    Read,
    Add,
    Update,
    Delete,
    Inventory,
}

// SAFETY: 호출자는 Security.framework의 정적 CFString 상수만 전달한다.
unsafe fn cf(value: CFStringRef) -> CFString {
    unsafe { CFString::wrap_under_get_rule(value) }
}

fn query(
    operation: Operation,
    id: Option<&str>,
    keychain: &CFType,
) -> CFDictionary<CFString, CFType> {
    let mut pairs = unsafe {
        vec![
            (cf(kSecClass), cf(kSecClassGenericPassword).into_CFType()),
            (
                cf(kSecAttrService),
                CFString::new(crate::KEYRING_SERVICE).into_CFType(),
            ),
        ]
    };
    if let Some(id) = id {
        pairs.push((
            unsafe { cf(kSecAttrAccount) },
            CFString::new(id).into_CFType(),
        ));
    }
    // SAFETY: 프레임워크 정적 상수와 소유한 CF 객체만 dictionary에 보관한다.
    unsafe {
        pairs.push((
            cf(kSecUseAuthenticationUI),
            cf(kSecUseAuthenticationUIFail).into_CFType(),
        ));
        if matches!(operation, Operation::Add) {
            pairs.push((cf(kSecUseKeychain), keychain.clone()));
        } else {
            pairs.push((
                cf(kSecMatchSearchList),
                CFArray::from_CFTypes(std::slice::from_ref(keychain)).into_CFType(),
            ));
        }
        if matches!(operation, Operation::Read) {
            pairs.push((cf(kSecReturnData), CFBoolean::true_value().into_CFType()));
        }
        if matches!(operation, Operation::Inventory) {
            pairs.push((
                cf(kSecReturnAttributes),
                CFBoolean::true_value().into_CFType(),
            ));
            pairs.push((cf(kSecMatchLimit), cf(kSecMatchLimitAll).into_CFType()));
        }
    }
    CFDictionary::from_CFType_pairs(&pairs)
}

fn missing_or_error(status: i32) -> anyhow::Result<bool> {
    match status {
        0 => Ok(false),
        -25_300 => Ok(true),
        status => Err(security_framework::base::Error::from_code(status).into()),
    }
}

// OS 접근은 test mock 경로가 아닌 production에서만 호출한다.
#[cfg_attr(test, allow(dead_code))]
fn login_keychain() -> anyhow::Result<CFType> {
    Ok(SecKeychain::default_for_domain(SecPreferencesDomain::User)
        .context("macOS login keychain 조회 실패")?
        .into_CFType())
}

#[cfg_attr(test, allow(dead_code))]
fn copy_matching(query: &CFDictionary<CFString, CFType>) -> anyhow::Result<Option<CFType>> {
    let mut result = std::ptr::null();
    // SAFETY: query는 유효한 CFDictionary이며 out pointer는 이 호출 동안 살아 있다.
    let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
    // 반환 객체가 있으면 오류 경로에서도 create 소유권을 회수한다.
    let result = (!result.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(result) });
    if missing_or_error(status)? {
        return Ok(None);
    }
    Ok(Some(result.context("keyring 조회 결과가 비어 있음")?))
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn set(id: &str, secret: &crate::SecretString) -> anyhow::Result<()> {
    let keychain = login_keychain()?;
    let add = query(Operation::Add, Some(id), &keychain);
    let (keys, values) = add.get_keys_and_values();
    // SAFETY: 이 모듈이 구성한 query의 key는 CFString, value는 CFType이다.
    let mut pairs = keys
        .into_iter()
        .zip(values)
        .map(|(key, value)| unsafe {
            (
                CFString::wrap_under_get_rule(key.cast()),
                CFType::wrap_under_get_rule(value),
            )
        })
        .collect::<Vec<_>>();
    let data = unsafe { cf(kSecValueData) };
    let password = CFData::from_buffer(secret.expose().as_bytes()).into_CFType();
    pairs.push((data.clone(), password.clone()));
    let add = CFDictionary::from_CFType_pairs(&pairs);
    // SAFETY: 소유한 dictionary를 전달하며 add 결과 객체는 요청하지 않는다.
    let status = unsafe { SecItemAdd(add.as_concrete_TypeRef(), std::ptr::null_mut()) };
    let status = if status == -25_299 {
        let update = query(Operation::Update, Some(id), &keychain);
        let value = CFDictionary::from_CFType_pairs(&[(data, password)]);
        // SAFETY: 같은 login keychain에 한정한 query와 password-only 속성을 전달한다.
        unsafe { SecItemUpdate(update.as_concrete_TypeRef(), value.as_concrete_TypeRef()) }
    } else {
        status
    };
    if status != 0 {
        return Err(security_framework::base::Error::from_code(status).into());
    }
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn get(id: &str) -> anyhow::Result<crate::SecretString> {
    let result = copy_matching(&query(Operation::Read, Some(id), &login_keychain()?))?
        .ok_or_else(|| security_framework::base::Error::from_code(-25_300))?;
    let data = result
        .downcast::<CFData>()
        .context("keyring 조회 결과가 data가 아님")?;
    // 복사 전에 UTF-8을 검사해 실패 경로에 평문 Vec을 남기지 않는다.
    let text = std::str::from_utf8(data.bytes()).context("keyring 값이 UTF-8 문자열이 아님")?;
    Ok(crate::SecretString::new(text.to_owned()))
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn has(id: &str) -> anyhow::Result<bool> {
    let result = copy_matching(&query(Operation::Read, Some(id), &login_keychain()?))?;
    if let Some(result) = result {
        anyhow::ensure!(
            result.downcast::<CFData>().is_some(),
            "keyring 조회 결과가 data가 아님"
        );
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn delete(id: &str) -> anyhow::Result<()> {
    let query = query(Operation::Delete, Some(id), &login_keychain()?);
    // SAFETY: 소유한 dictionary는 호출 완료까지 유효하다.
    missing_or_error(unsafe { SecItemDelete(query.as_concrete_TypeRef()) })?;
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn list(prefix: &str) -> anyhow::Result<Vec<String>> {
    let Some(result) = copy_matching(&query(Operation::Inventory, None, &login_keychain()?))?
    else {
        return Ok(Vec::new());
    };
    let array = result
        .downcast::<CFArray>()
        .context("keyring inventory 결과가 array가 아님")?;
    let mut ids = Vec::new();
    for item in array.iter() {
        // SAFETY: SecItemCopyMatching의 CFArray는 살아 있는 CF 객체를 소유한다.
        let item = unsafe { CFType::wrap_under_get_rule(*item) };
        let dict = item
            .downcast::<CFDictionary>()
            .context("keyring inventory 항목이 dictionary가 아님")?;
        // CF dictionary의 값은 CFType으로 소유하고 필요한 문자열 타입만 별도로 검사한다.
        let dict = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(dict.as_concrete_TypeRef())
        };
        let service = dict
            .find(CFString::new("svce"))
            .and_then(|v| v.downcast::<CFString>());
        let account = dict
            .find(CFString::new("acct"))
            .and_then(|v| v.downcast::<CFString>());
        let (Some(service), Some(account)) = (service, account) else {
            anyhow::bail!("keyring inventory 필수 속성이 없음");
        };
        if service == crate::KEYRING_SERVICE && account.to_string().starts_with(prefix) {
            ids.push(account.to_string());
            anyhow::ensure!(
                ids.len() <= crate::SECRET_PREFIX_ENTRY_LIMIT,
                "secret entry inventory exceeds fixed prefix ceiling"
            );
        }
    }
    crate::collect_bounded_secret_ids(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::array::CFArray;

    #[test]
    fn every_operation_fails_instead_of_requesting_authentication_ui() {
        let sentinel = CFString::new("test-only-keychain").into_CFType();
        for operation in [
            Operation::Read,
            Operation::Add,
            Operation::Update,
            Operation::Delete,
            Operation::Inventory,
        ] {
            let q = query(operation, Some("policy-test"), &sentinel);
            let key = unsafe { cf(kSecUseAuthenticationUI) };
            let value = q.find(&key).expect("인증 정책이 빠지면 OS가 UI를 허용한다");
            assert_eq!(
                *value,
                unsafe { cf(kSecUseAuthenticationUIFail) }.into_CFType()
            );
        }
    }

    #[test]
    fn existing_login_keychain_and_service_account_are_preserved() {
        let sentinel = CFString::new("test-only-keychain").into_CFType();
        for operation in [
            Operation::Read,
            Operation::Update,
            Operation::Delete,
            Operation::Inventory,
        ] {
            let q = query(operation, Some("policy-test"), &sentinel);
            let list = q
                .find(&unsafe { cf(kSecMatchSearchList) })
                .expect("검색 범위가 없으면 다른 keychain을 조회한다")
                .downcast::<CFArray>()
                .unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(*list.get(0).unwrap(), sentinel.as_CFTypeRef());
            assert_eq!(
                *q.find(&unsafe { cf(kSecAttrService) }).unwrap(),
                CFString::new(crate::KEYRING_SERVICE).into_CFType()
            );
            assert_eq!(
                *q.find(&unsafe { cf(kSecAttrAccount) }).unwrap(),
                CFString::new("policy-test").into_CFType()
            );
        }
        let add = query(Operation::Add, Some("policy-test"), &sentinel);
        assert_eq!(
            *add.find(&unsafe { cf(kSecUseKeychain) }).unwrap(),
            sentinel
        );
        assert!(add.find(&unsafe { cf(kSecMatchSearchList) }).is_none());
    }

    #[test]
    fn interaction_denied_is_never_classified_as_missing() {
        assert!(!missing_or_error(0).unwrap());
        assert!(missing_or_error(-25_300).unwrap());
        for status in [-25_308, -25_293, -25_291, -50] {
            let error = missing_or_error(status).expect_err("접근 오류는 부재가 아니다");
            assert_eq!(
                error
                    .downcast_ref::<security_framework::base::Error>()
                    .unwrap()
                    .code(),
                status
            );
        }
    }
}
