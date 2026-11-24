# 03. 파이프라인과 호환 어댑터 (orbic)

> 상태: 코어 `ref` + 호환 어댑터 분리 원칙은 **확정**, 도메인별 어댑터 세부는 **제안** · 기준일: 2026-11-24

`orbic`의 모든 백엔드 도메인(DB, Cache, MSA, WS/WT)은 **내부 고성능 코어 파이프라인**과 **외부·인프라 호환 어댑터**의 2단 구조로 동작합니다(`hyperish/docs` `02-core-contracts.md` §2~§7).

## 1. 백엔드 요청·작업 파이프라인

이 파이프라인은 `orbic-core`가 소유하며, 외부 어댑터·내부 MSA 채널·인메모리 `TestClient`가 모두 같은 진입점을 호출합니다. 어댑터와 `TestClient`는 아래 단계를 복제하지 않습니다(`hyperish/docs` `02-core-contracts.md` §5).

1. **어댑터 수신 및 컨텍스트 생성**: 외부 어댑터(`orbic-adapter-http`, `orbic-adapter-ws` 등) 또는 내부 MSA 채널이 요청을 받아 상관관계 ID(`requestId`)와 구조적 취소 토큰(`CancellationToken`)을 포함한 요청 컨텍스트(핸들러에는 `Ctx<T>`로 주입)를 생성합니다.
2. **접근 권한·멱등성 게이트**:
   - 보호 대상은 요청 시점마다 소유 계층이 권한과 철회 여부를 검사합니다(`02-core-contracts.md` §6).
   - 부수효과가 있는 생성성 요청(`POST`로 바인딩한 라우트)의 `Idempotency-Key`는 `orbic-cache`를 통해 중복 실행과 동시 처리(`409 request_in_progress`)를 제어합니다. 게이트는 키가 온 요청에만 작동하고, 키가 없으면 그대로 처리합니다. 처음 응답은 성공이든 `Domain` 오류든 그대로 재사용하고, `Infra`·`Internal` 오류는 저장하지 않아 다시 시도할 수 있습니다. "같은 본문"은 레일(미디어 타입)과 본문 바이트의 다이제스트가 모두 같은 것이며, 같은 키를 다른 레일로 보내면 `409 idempotency_key_mismatch`입니다. 보관 기간은 구현이 정하고 문서화합니다.
   - **게이트 세부 (제안, 2026-11-24)**: 처음 응답의 보관 기간은 24시간입니다. 처리 중 응답의 `Retry-After`는 1초입니다. 키는 1~255바이트의 출력 가능한 문자열이며 어기면 `400 invalid_request`입니다. 바인딩된 메서드로 판정하므로 고속 레일로 `POST`가 되어 오는 `GET` 바인딩 `#[server]` 함수는 게이트를 거치지 않습니다. 키 범위의 경로는 패턴이 아니라 실제 경로입니다. 인증 주체(가드가 요청 스코프에 넣는 `Principal`)가 없는 요청은 응답의 주인을 한정할 수 없으므로 저장도 재사용도 하지 않습니다. 인프로세스 레일은 본문 바이트와 전송 재시도가 없어 게이트를 거치지 않습니다. 실패(`Infra`·`Internal`)했거나 도중에 취소된 요청은 키를 즉시 풀어 다시 시도할 수 있습니다.
   - 순서는 라우트 매칭(고속 레일이면 함수 식별자와 스키마 지문 판정 포함) → `wrap` 가드(선언한 순서대로) → 멱등성 게이트 → 추출·검증 → 핸들러입니다. 멱등 응답 재사용은 항상 인증·권한 검사 뒤이고, 키의 범위는 인증 주체·메서드·경로·키의 조합입니다(`hyperish/docs` `02-core-contracts.md` §6).
   - `RateLimit`는 선언한 창(`per_minute(60)`이면 1분)이 지나면 초기화되며, 남은 횟수와 초기화까지 남은 시간을 `hyperish/docs` `03-api-protocol.md` §8의 헤더로 `RateLimit` 가드에 도달한 요청의 응답에 싣습니다. 앞선 가드가 먼저 거부한 요청은 세지 않고 헤더도 없습니다. 프록시 뒤에서 클라이언트 주소는 신뢰하도록 지정한 프록시가 전달한 값을 씁니다. 키와 범위는 `01-dx-and-dsl.md` §1의 조립 계약이 정합니다. (제안, 2026-11-24) 창은 주체의 첫 요청에서 시작하는 고정 창이고, 초과하면 `429 rate_limited`와 창이 끝날 때까지의 `Retry-After`(초 단위 올림)입니다. `RateLimit`이 겹치면 응답 헤더는 남은 횟수가 가장 적은 것을 싣습니다. 프록시도 주소도 없는 요청(인프로세스)은 하나의 익명 주체로 셉니다.
   - `orbic-core`는 멱등 키, 속도 제한 정보, 자격 증명을 전송 중립 요청·응답 메타데이터로 다룹니다. 이를 HTTP 헤더(`Idempotency-Key`, `RateLimit-*`, 쿠키, `Authorization`, `X-CSRF-Token`)와 맞바꾸는 것은 `orbic-adapter-http`입니다.
3. **도메인 실행 및 커밋/보상**:
   - `orbic-db` 트랜잭션과 `orbic-jobs` 워커는 취소 가능 지점과 커밋 지점을 명시하며, 커밋 이후 실패 시 보상 절차 또는 재조정(reconciliation) 상태를 기록합니다(`02-core-contracts.md` §3).
4. **응답 변환**: 내부 구조화 오류(`Domain` / `Infra` / `Internal`)는 어댑터 경계에서 내부 스택 노출 없이 외부 규약의 오류 형식으로 변환됩니다. `#[server]` 스텁이 두 레일에서 같은 오류를 복원할 수 있도록, HTTP 변환은 오류의 종류와 재시도 가능 여부를 상태 코드와 `Retry-After`로 왕복 가능하게 정의합니다(`Domain`은 4xx, `Infra`는 `503`, `Internal`은 `500` — `hyperish/docs` `03-api-protocol.md` §4, `01-dx-and-dsl.md` §2).
   - `Domain`: `code`가 `03-api-protocol.md` §4 표에 있으면 그 상태 코드, `_not_found`로 끝나면 `404`, 그 밖은 `422`입니다. `code`는 그대로 내보냅니다.
   - `Infra`: `503`입니다. `code`는 오류가 공개용으로 선언한 값이면 그대로, 아니면 `service_unavailable`입니다. 재시도 가능하면 `Retry-After`를 붙입니다.
   - `Internal`: `500 internal_error`입니다. 원문은 로그에만 남깁니다.
   - 프레임워크가 내는 오류: 라우트 없음 `404 route_not_found`, 메서드 불일치 `405 method_not_allowed`, 본문 크기 초과 `413 payload_too_large`, 받을 수 없는 본문 형식 `415 unsupported_media_type`(고속 레일을 받지 않는 경로, 모르는 함수 식별자 포함), 고속 레일 스키마 불일치 `415 schema_mismatch`, CSRF 실패 `403 csrf_token_invalid`.
   - 위 규칙으로 정한 공개 형태(종류, `code`, `message`, `details`, 재시도 가능 여부, `requestId`)는 레일을 고르기 전에 한 번만 만듭니다. 표준 레일은 그것을 봉투와 상태 코드로, 고속 레일은 같은 상태 코드에 그것을 아카이브해 싣습니다(`hyve/docs/03-adapters-and-pipeline.md` §3.3). `Infra`의 `code` 치환과 `Internal`의 가림은 두 레일에 똑같이 적용되고, 내부 오류 값 자체는 어느 레일로도 나가지 않습니다.

- **`TestClient`의 위치**: `TestClient`는 소켓만 빼고 `orbic-adapter-http`의 변환 계층(상태 코드, 헤더, 봉투, CORS, 보안 헤더, 요청 크기 제한)을 그대로 통과합니다. 그래서 같은 시나리오가 `TestClient`와 실제 서버에서 같은 응답을 냅니다. `TestClient`는 `hyve-api`의 전송 트레이트도 구현하며, 레일을 지정하면 `#[server]` 스텁의 호출을 그 레일의 직렬화와 변환 계층까지 통과시킵니다. 레일을 지정하지 않은 인프로세스 전송은 직렬화 없이 파이프라인만 거칩니다(`hyve/docs/03-adapters-and-pipeline.md` §3.2).
- **HTTP 변환 세부 (제안, 2026-11-24)**: `orbic-adapter-http`의 `HttpApp::handle`이 소켓 없는 변환 전체이고 `HttpApp::serve`는 소켓만 더합니다. `TestClient`는 `handle`을 부릅니다.
  - 설정: 앱 루트에 `provide`한 `HttpConfig`(없으면 기본값)를 서버와 `TestClient`가 함께 읽습니다. 본문 상한 1 MiB, CORS 허용 Origin(정확한 Origin만, `*`는 조립 실패), 신뢰하는 프록시 주소 범위, `/metrics` 허용 범위(기본 루프백), 세션 쿠키 이름(기본 `hyperish_session`), 셧다운 유예 30초.
  - 상관관계 ID: 요청 헤더 `X-Request-Id`가 1~128자의 출력 가능한 ASCII면 그 값을, 아니면 새 값을 씁니다(거부하지 않음). 모든 응답이 같은 헤더로 그 ID를 싣습니다(`hyperish/docs` `03-api-protocol.md` §4).
  - 레일: `Content-Type`이 `application/vnd.hyperish.rkyv`면 고속 레일이고 `Hyperish-Fn`과 16진수 `Hyperish-Schema`가 없으면 `415 unsupported_media_type`입니다. 본문이 있는 표준 레일 요청은 `application/json`(매개변수 허용)이어야 합니다. 고속 레일의 오류는 아카이브한 오류 값이지만 `415`는 폴백 신호라 JSON 봉투입니다. 어댑터가 파이프라인 전에 내는 오류(`413`, `415`, API 메서드가 아닌 메서드의 `405`, HTTPS 거부, 허용하지 않은 Origin의 사전 요청)는 JSON 봉투입니다.
  - 재시도: 오류가 재시도 가능할 때만, 그리고 그때는 반드시 `Retry-After`를 붙입니다. 값은 파이프라인이 정한 초(속도 제한의 초기화까지, 처리 중 응답의 1초) 또는 기본 1초입니다. `rate_limited`와 `request_in_progress`는 재시도 가능한 오류입니다. `405`는 `Allow`를 붙입니다.
  - 자격 증명: `Authorization: Bearer`가 쿠키보다 우선합니다(헤더는 의도해서 보내고 쿠키는 저절로 실림). 세션 쿠키는 `Path=/; HttpOnly; Secure; SameSite=Lax`이고 `Max-Age`가 없는 세션 쿠키입니다(만료는 서버의 유휴·무조건 만료가 정함). 로그아웃은 `Max-Age=0`으로 지웁니다.
  - 보안 헤더(모든 응답): `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`, `Strict-Transport-Security: max-age=31536000`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store`.
  - CORS: 허용한 Origin의 요청에 `Access-Control-Allow-Origin`(그 Origin), `Access-Control-Allow-Credentials: true`, `Access-Control-Expose-Headers`(`Retry-After`, `RateLimit-*`, `X-Request-Id` — 다른 Origin의 스텁이 재시도 가능 여부를 복원하려면 필요)를 붙이고, 모든 응답에 `Vary: Origin`을 붙입니다. 사전 요청(`OPTIONS` + `Origin` + `Access-Control-Request-Method`)은 파이프라인 전에 답하며, 허용한 Origin이면 `204`와 허용 메서드·헤더·`Access-Control-Max-Age: 600`, 아니면 `403 forbidden`입니다.
  - 프록시: 신뢰하는 프록시가 직접 보낸 요청만 `X-Forwarded-For`·`X-Forwarded-Proto`를 믿습니다. 클라이언트 주소는 `X-Forwarded-For`에서 오른쪽부터 신뢰하는 프록시가 아닌 첫 항목이고(왼쪽 항목은 클라이언트가 쓴 값), 형식이 깨진 사슬은 믿지 않습니다. 스킴은 `X-Forwarded-Proto`의 마지막 값(가장 가까운 프록시가 본 값)입니다.
  - 취소: 요청은 자기 연결의 작업 안에서 실행되어, 클라이언트가 연결을 끊으면 핸들러가 취소됩니다. 셧다운은 수신을 멈추고 파이프라인의 셧다운을 시작한 뒤(새 요청은 재시도 가능한 `503`, `/health`는 `503`) 연결을 우아하게 닫고, 유예가 지나면 실행 중인 요청을 취소하고 1초 뒤 연결을 끊습니다.
- **TLS (제안)**: Phase 1의 `orbic-adapter-http`는 HTTP/1.1을 서빙하고 TLS 종단은 앞단 프록시에 맡깁니다. 운영 설정에서는 신뢰하도록 지정한 프록시가 전달한 스킴이 HTTPS가 아닌 요청을 거부하고, 루프백에 바인딩한 개발·테스트 서버만 평문을 허용합니다. 자체 TLS와 HTTP/2는 Phase 2에서 정합니다.

## 2. 호환 어댑터 구성

| 영역 | 코어 추상화 (`orbic-*`) | 호환 어댑터 및 동작 방식 |
|---|---|---|
| **Dual-Rail API 전송** | `hyve-api` + `orbic-core` | • **표준 레일**: 외부 호환용 **HTTP/JSON** (`hyperish/docs` `03-api-protocol.md` 준수: 커서 페이지네이션, `expectedVersion` 낙관적 잠금, `RateLimit-*` 헤더)<br>• **고속 레일**: `serge` ↔ `orbic` 인라인 서버 함수 및 MSA 호출 시 **`hyve-serde` 제로카피 바이너리** 직결(요청-응답은 `orbic-adapter-http`의 바이너리 본문, 스키마가 어긋나면 표준 레일로 폴백). 두 레일은 같은 경로에서 미디어 타입으로 구분하며, 와이어 형식은 `hyve/docs/03-adapters-and-pipeline.md` §3을 따름 및 클라이언트 언마운트 시 종단 간 취소(`CancellationToken`) 전파 |
| **Dual-Rail 실시간 (WT / WS)** | `orbic-realtime` (`seq`, 룸 상태, 중복 제거) | **WebTransport(HTTP/3 QUIC 다중 스트림·데이터그램)**를 1순위로 연결하고, QUIC/UDP가 차단된 네트워크에서는 **WebSocket**(`04-realtime-communication.md`)으로 자동 폴백. 고속 레일은 폴백 WS에서도 `hyve-serde` 바이너리 프레임을 유지하고, JSON 텍스트 프레임은 외부 클라이언트용 표준 레일에만 씀. 브라우저 WebTransport도 커스텀 인증 헤더를 보낼 수 없으므로 동일한 `?ticket=` 일회용 티켓 핸드셰이크를 쓰며, 폴백 전환 시에도 `seq`·중복 제거 `id` 연속성을 유지 |
| **인증·세션 (`orbic-auth`)** | `orbic-auth` + `orbic-cache` | 웹(`HttpOnly` 쿠키 + `X-CSRF-Token`)과 네이티브/CLI(`Bearer` 토큰) 이중 전송 어댑터. 즉시 철회 보장을 위해 **Opaque 세션의 권위 저장소 직접 조회를 기본**(단일 노드는 인프로세스 저장소, 멀티 레플리카는 L2 Redis/Valkey)으로 하며, 멀티 레플리카에서 L1 캐싱 옵트인 시 초단기 TTL·단절 시 L1 전체 플러시·노출 창 명시(`02-core-contracts.md` §6). 세션·CSRF·리프레시 토큰은 OS의 암호학적 난수원에서 만든 256비트 값이며(`hyperish/docs` `03-api-protocol.md` §3의 64비트 이상 요구를 충족) `Ulid` 같은 식별용 ID를 쓰지 않음. 세션을 철회하면 같은 계열의 리프레시 토큰도 폐기함. 단일 노드의 인프로세스 저장소는 프로세스를 재시작하면 세션이 사라짐 |
| **DB & Cache** | `orbic-db`, `orbic-cache` (Phase 1 캐시 규칙, 제안 2026-11-24: TTL은 적재를 시작한 때부터 세고, 0이면 동시 적재만 합치고 저장하지 않으며, 오류는 저장하지 않고, 한 키에는 한 타입의 값만 둠) | **PostgreSQL** 및 **SQLite/LibSQL** 드라이버 어댑터(`#[table]` 체이닝 + `sql!` 매크로), **L1 인메모리(`quick_cache` S3-FIFO) + L2 `Redis/Valkey`(`fred` RESP3)** Tiered 캐시 어댑터(Singleflight·태그 무효화) |
| **Storage · Jobs · Notify** | `orbic-storage`, `orbic-jobs`, `orbic-notify` | 로컬/S3/R2 멀티파트 업로드(크기·체크섬 검증 후 활성화), 지연·재시도·DLQ 작업 큐 및 Cron 스케줄러, 이메일·푸시(APNs/FCM)·HMAC 웹훅 어댑터 |
| **`serge` 웹 서빙** | `orbic-adapter-serge` | `serge`가 산출한 정적 에셋 매니페스트(`SergeBundleManifest`), 라우트 메타데이터, `serge-web-ssr` 스트리밍 핸들러(`SergeSsrHandler`)를 받아 프로덕션 서빙 (`serge/docs/03-adapters-and-pipeline.md` §3 참조) |
