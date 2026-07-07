#!/bin/sh
# 일회성 dev 코드사인 인증서 설정 — 한 번만 실행하면 된다.
#
# 왜: 기본 빌드는 ad-hoc 서명(codesign --sign -)이라 매 빌드마다 code signature가 달라진다.
# macOS TCC(데스크탑 폴더 접근 등)는 앱을 signature로 식별하므로, 재빌드할 때마다 "새 앱"으로
# 보고 권한을 다시 물어본다. 고정된 자체서명 인증서로 서명하면 signature가 안정적이라 한 번
# 승인한 권한(데스크탑 접근 등)이 재빌드 후에도 유지된다.
#
# 이 스크립트는 자체서명 코드사인 인증서를 만들고 **login keychain에서 codeSigning 용도로
# 신뢰(trust)**한다 — 그 신뢰 설정에 keychain 비밀번호를 1회 입력해야 한다(정상). 이후 빌드
# (scripts/package-macos.sh)는 이 인증서를 자동으로 사용한다.
#
# 배포용이 아니다(Developer ID 아님). 로컬 개발 편의용.
set -eu

CERT_CN="deppy-sijo-dev"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "$CERT_CN"; then
    echo "이미 설정됨: '$CERT_CN' 코드사인 인증서가 신뢰된 상태입니다. 할 일 없음."
    exit 0
fi

# 이름은 같지만 미신뢰인 잔재가 있으면 정리(중복 서명 모호성 방지).
for H in $(security find-identity 2>/dev/null | grep "$CERT_CN" | awk '{print $2}' | sort -u); do
    security delete-identity -Z "$H" >/dev/null 2>&1 || true
done

CDIR="$(mktemp -d)"
trap 'rm -rf "$CDIR"' EXIT

echo "1/3 자체서명 코드사인 인증서 생성…"
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$CDIR/key.pem" -out "$CDIR/cert.pem" -subj "/CN=$CERT_CN" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" >/dev/null 2>&1
# macOS security가 읽는 legacy PKCS12. -A로 import하면 codesign이 키를 프롬프트 없이 쓴다.
openssl pkcs12 -export -legacy -out "$CDIR/cert.p12" \
    -inkey "$CDIR/key.pem" -in "$CDIR/cert.pem" -passout pass:deppy >/dev/null 2>&1 ||
    openssl pkcs12 -export -out "$CDIR/cert.p12" \
        -inkey "$CDIR/key.pem" -in "$CDIR/cert.pem" -passout pass:deppy >/dev/null 2>&1

KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
echo "2/3 keychain에 import…"
security import "$CDIR/cert.p12" -A -P deppy -k "$KEYCHAIN" >/dev/null 2>&1 ||
    security import "$CDIR/cert.p12" -A -P deppy >/dev/null 2>&1

echo "3/3 codeSigning 용도로 신뢰 설정 — keychain 비밀번호를 물어봅니다(1회)…"
# 사용자 도메인 trust (sudo 불필요). codesign이 미신뢰 인증서를 거부하므로 이 단계가 필수다.
security add-trusted-cert -r trustRoot -p codeSign "$CDIR/cert.pem"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "$CERT_CN"; then
    echo ""
    echo "✅ 완료. 이제 scripts/package-macos.sh가 '$CERT_CN'으로 서명합니다."
    echo "   재빌드해도 데스크탑 접근 등 권한이 유지됩니다(한 번만 '허용'하면 됨)."
else
    echo ""
    echo "⚠️  신뢰 설정이 반영되지 않았습니다. Keychain Access.app에서 '$CERT_CN' 인증서를"
    echo "   열어 '신뢰 > 코드 서명: 항상 신뢰'로 설정해 주세요."
    exit 1
fi
