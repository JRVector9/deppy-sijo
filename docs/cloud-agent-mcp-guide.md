# Grok와 Cloud Agent의 Deppy 세션 사용

Deppy에서 사용자가 공유한 원래 터미널 세션의 입력과 출력을 사용한다. Grok bot과 Cloud Agent는 자신의 최종 답변을 `notify`로 그 세션에 남긴다. 답변은 해당 세션의 로컬 이력과 알림 센터에 저장된다. 터미널 stdin에 답변을 쓰지 않는다.

1. `list_sessions`에서 공유된 `session_id`와 `generation`을 보관한다. `input_allowed`는 사용자 입력 허용과 OAuth 입력 범위를 함께 반영한다. `paste_bracketed`는 현재 관측한 DEC 2004 모드이며, `paste_ai_confirmed`는 AI 실행 정체성이 확보되어 있는지 보여준다. 이 값들은 관측값이다. 입력 시 다시 검사한다.
2. `read_output`으로 최근 관측된 화면을 읽는다. `lossless:false`, `may_be_stale:true`를 고려한다. `screen:null`은 캐시가 같다는 뜻이다. 새 출력이 없다는 증거나 턴 완료 신호가 아니다. `retry_after_ms` 뒤 다시 읽고 반환된 cursor를 사용할 수 있다. 신뢰할 수 있는 턴 완료 경계나 stdout 전체 로그를 제공하지 않는다.
3. 단일 줄 수동 입력은 기존 `send_text`를 사용한다. 본문 최대 8192 UTF-8 바이트이고 개행·탭·ESC·제어문자를 거부한다. `submit` 기본값은 false이며 true일 때만 Enter를 붙인다. 기존 공유 셸·대화 상자의 승인된 수동 입력 계약을 유지한다. `send_ctrl_c`는 승인된 원래 세션에 Ctrl+C를 한 번 보낸다.
4. 여러 줄 프롬프트 또는 의도적인 붙여넣기는 `paste_text`를 사용한다. `submit` 기본값은 false다. LF, CRLF, 탭은 확인된 bracketed paste 모드에서만 허용하며 CRLF는 LF로 정규화된다. 한글과 이모지는 UTF-8로 보존한다. 단독 CR, ESC 및 나머지 제어문자는 거부한다. 모드가 꺼져 있을 때 한 줄 일반 텍스트는 수동 입력으로 허용하며, 여러 줄이나 탭은 거부한다. 알려진 AI의 실행 정체성이 확인되지 않으면 거부한다.
5. 답변을 마치면 **자신이 작성한 최종 답변**으로 `notify`를 호출한다. 분석만 하고 터미널에 입력하지 않았어도 필요하다. 처음 보관한 원래 session_id/generation을 사용한다. 탭을 전환해도 다른 세션에 보내지 않는다. 저장이 시작된 답변의 완료는 연결을 중지한 후에도 원래 세션에 반영된다.

## 붙여넣기 예

```json
{
  "name": "paste_text",
  "arguments": {
    "session_id": "<list_sessions의 원래 UUID>",
    "generation": "<원래 generation>",
    "operation_id": "<이 작업의 고유 ID>",
    "text": "첫째 줄 😀\r\n둘째 줄\t설명",
    "submit": true
  }
}
```

본문은 최대 32768 UTF-8 바이트다. 직렬화한 arguments와 전체 HTTP JSON 본문은 각각 65536 바이트 이하여야 한다. 큰따옴표·역슬래시 등의 JSON 이스케이프와 요청 봉투 때문에 본문 한도 전에 거부될 수 있다. 전체 HTTP 한도는 기존과 같다.

`submit:true`는 본문과 별도 Enter 하나를 하나의 PTY 큐 예약으로 입장시킨다. 일부 본문만 입장하거나 거부된 본문 뒤에 Enter만 보내지 않는다. 확인된 AI 세션에 기존 입력 초안이나 선택/승인 대화 상자가 있으면 새 프롬프트 제출을 거부한다. `submit:false`는 의도적으로 기존 초안에 붙일 수 있지만 대화 상자에는 자연어 붙여넣기를 보내지 않는다. 모드·원래 실행·세션·허용권·인증·마감 시간은 비동기 이력 claim 뒤와 실제 큐 입장 경계에서 다시 확인한다.

`status:"queued"`는 PTY 큐 입장 확인이며 AI 실행 또는 완료 확인이 아니다. `completion:"not_confirmed"`를 그대로 해석한다. 동일 operation_id는 정확히 같은 도구와 arguments에만 재사용한다. `unknown`은 자동으로 다시 보내지 않는다. 새 ID를 만들어 재시도하면 이중 실행될 수 있다. Deppy는 보이지 않는 새 AI 프로세스를 만들어 이 요청을 수행하지 않는다.

```json
{
  "name": "notify",
  "arguments": {
    "session_id": "<원래 UUID>",
    "generation": "<원래 generation>",
    "operation_id": "<답변 저장 작업의 고유 ID>",
    "message": "Grok 또는 Cloud Agent가 직접 작성한 최종 답변"
  }
}
```

`notify`는 최대 16384 UTF-8 바이트다. 입력 허용이 없어도 읽기 공유가 유지된 원래 세션으로 답변을 저장할 수 있다. 공유·인증이 이미 취소되거나 요청이 만료됐다면 새 저장 요청은 거부된다. 성공 응답은 이력 저장 완료 이후에 반환된다.
