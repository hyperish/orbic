# 03. 파이프라인과 호환 어댑터 (orbic)

> 코어와 어댑터의 분리, HTTP 오리진 및 파일 저장의 책임 범위는 **확정**입니다. 기존 도메인별 제안은 해당 절에 표시합니다. 기준일 2026-11-26

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

<a id="http-origin"></a>

### HTTP 오리진과 TLS (확정, 2026-11-26)

Phase 1~2의 프로덕션 오리진은 네이티브 `HttpApp`의 HTTP/1.1 리스너입니다. 외부 HTTPS의 인증서와 ALPN(`h2` 또는 `http/1.1`)은 앞단 프록시가 소유합니다. 프록시는 전달 헤더를 직접 설정하고 접근이 제한된 오리진으로 HTTP/1.1을 전달합니다. 오리진은 설정된 프록시의 피어 주소와 HTTPS 스킴을 확인합니다. 개발 루프백 및 기존 헬스·메트릭 예외는 §1의 규칙을 따릅니다. Phase 2는 오리진 자체 TLS·HTTP/2 리스너를 제공하지 않습니다. SSR 청크는 프록시가 버퍼링하지 않고 전달하며, 클라이언트 해제는 오리진 요청 취소로 전파해야 합니다. [현재 리스너](../crates/orbic-adapter-http/src/server.rs) · [HTTP/2의 TLS 협상](https://www.rfc-editor.org/rfc/rfc9113.html#section-3.2)

## 2. 호환 어댑터 구성

| 영역 | 코어 추상화 (`orbic-*`) | 호환 어댑터 및 동작 방식 |
|---|---|---|
| **Dual-Rail API 전송** | `hyve-api` + `orbic-core` | • **표준 레일**: 외부 호환용 **HTTP/JSON** (`hyperish/docs` `03-api-protocol.md` 준수: 커서 페이지네이션, `expectedVersion` 낙관적 잠금, `RateLimit-*` 헤더)<br>• **고속 레일**: `serge` ↔ `orbic` 인라인 서버 함수 및 MSA 호출 시 **`hyve-serde` 제로카피 바이너리** 직결(요청-응답은 `orbic-adapter-http`의 바이너리 본문, 스키마가 어긋나면 표준 레일로 폴백). 두 레일은 같은 경로에서 미디어 타입으로 구분하며, 와이어 형식은 `hyve/docs/03-adapters-and-pipeline.md` §3을 따름 및 클라이언트 언마운트 시 종단 간 취소(`CancellationToken`) 전파 |
| **Dual-Rail 실시간 (WT / WS)** | `orbic-realtime` (`seq`, 룸 상태, 중복 제거) | **WebTransport(HTTP/3 QUIC 다중 스트림·데이터그램)**를 1순위로 연결하고, QUIC/UDP가 차단된 네트워크에서는 **WebSocket**(`04-realtime-communication.md`)으로 자동 폴백. 고속 레일은 폴백 WS에서도 `hyve-serde` 바이너리 프레임을 유지하고, JSON 텍스트 프레임은 외부 클라이언트용 표준 레일에만 씀. 브라우저 WebTransport도 커스텀 인증 헤더를 보낼 수 없으므로 동일한 `?ticket=` 일회용 티켓 핸드셰이크를 쓰며, 폴백 전환 시에도 `seq`·중복 제거 `id` 연속성을 유지 |
| **인증·세션 (`orbic-auth`)** | `orbic-auth` + `orbic-cache` | 웹(`HttpOnly` 쿠키 + `X-CSRF-Token`)과 네이티브/CLI(`Bearer` 토큰) 이중 전송 어댑터. 즉시 철회 보장을 위해 **Opaque 세션의 권위 저장소 직접 조회를 기본**(단일 노드는 인프로세스 저장소, 멀티 레플리카는 L2 Redis/Valkey)으로 하며, 멀티 레플리카에서 L1 캐싱 옵트인 시 초단기 TTL·단절 시 L1 전체 플러시·노출 창 명시(`02-core-contracts.md` §6). 세션·CSRF·리프레시 토큰은 OS의 암호학적 난수원에서 만든 256비트 값이며(`hyperish/docs` `03-api-protocol.md` §3의 64비트 이상 요구를 충족) `Ulid` 같은 식별용 ID를 쓰지 않음. 세션을 철회하면 같은 계열의 리프레시 토큰도 폐기함. 단일 노드의 인프로세스 저장소는 프로세스를 재시작하면 세션이 사라짐 |
| **DB & Cache** | `orbic-db`, `orbic-cache` (Phase 1 캐시 규칙, 제안 2026-11-24: TTL은 적재를 시작한 때부터 세고, 0이면 동시 적재만 합치고 저장하지 않으며, 오류는 저장하지 않고, 한 키에는 한 타입의 값만 둠) | **PostgreSQL** 및 **SQLite/LibSQL** 드라이버 어댑터(`#[table]` 체이닝 + `sql!` 매크로), **L1 인메모리(`quick_cache` S3-FIFO) + L2 `Redis/Valkey`(`fred` RESP3)** Tiered 캐시 어댑터(Singleflight·태그 무효화) |
| **Storage · Jobs · Notify** | `orbic-storage`, `orbic-jobs`, `orbic-notify` | 로컬 파일 저장 및 S3 호환 멀티파트 업로드(크기·체크섬 검증 후 활성화), 지연·재시도·DLQ 작업 큐 및 Cron 스케줄러, 이메일·푸시(APNs/FCM)·HMAC 웹훅 어댑터 |
| **`serge` 웹 서빙** | `orbic-adapter-serge` | Phase 2에서 버전이 맞는 번들을 마운트하고 라우트 가드 실행 뒤 정적 HTML 또는 SSR 스트림을 제공합니다. HTTP 응답과 취소는 Orbic이 소유합니다. [번들·서빙 v1](https://github.com/hyperish/serge/blob/main/docs/03-adapters-and-pipeline.md#bundle-v1) · [SSR 타입](https://github.com/hyperish/serge/blob/main/docs/03-adapters-and-pipeline.md#ssr-v1) |

<a id="deployment-storage"></a>

## 3. 배포 환경과 파일 저장 (확정, 2026-11-26)

Orbic의 배포 단위는 네이티브 HTTP 서버 실행 파일입니다. 앱 운영자가 서버를 실행할 환경과 앞단 프록시를 선택합니다. 프록시를 거칠 때에도 가드·쿠키·두 레일·요청 취소는 직접 연결과 같은 결과를 내야 합니다. [HTTP 오리진 계약](#http-origin)

Phase 2의 `orbic-storage`는 대용량 파일을 다루는 앱이 선택합니다. 로컬 디스크에 저장할 수 있고, 객체 저장소를 선택한 앱은 S3 호환 엔드포인트를 설정합니다. 멀티파트 업로드는 권한 확인 후 파트별 서명 URL을 발급하고, 재시도 시 같은 업로드를 이어갑니다. 전체 크기와 SHA-256을 검증한 뒤에만 파일을 공개합니다.

앱이 `orbic`의 `storage` 피처를 켜면 `orbic::storage::DiskStorage`를, `storage-s3`를 켜면 `ObjectStorage`도 쓸 수 있습니다. 가드가 인증한 `Principal`과 예상 `Integrity`로 업로드를 시작하고 재시도할 때 같은 ID를 사용합니다. 앱 전용 디렉터리 또는 비공개 버킷에 저장합니다. 디스크에서는 `open`, 객체 저장소에서는 `download_url`이 소유권과 검증 완료 상태를 확인합니다. `bash scripts/test-storage-s3.sh`는 임시 로컬 서버에서 실제 서명 PUT, 파트 교체, 재개, 완료와 다운로드 검사를 실행합니다. [디스크](../crates/orbic-storage/src/disk.rs) · [객체 저장소](../crates/orbic-storage/src/object.rs) · [통합 테스트](../crates/orbic-storage/tests/object.rs)

## 4. 지연 작업과 분산 스케줄러

`jobs` 피처를 켜고 앱에 `Db`를 제공하면 `orbic::serve`가 컴포넌트에 등록된 `.every`와 UTC cron 작업을 실행합니다. 스케줄 갱신과 작업 추가는 같은 트랜잭션을 사용합니다. SQLite 파일 또는 PostgreSQL을 공유하는 두 서버가 같은 시각에 깨어나도 그 회차의 작업은 하나만 생깁니다. `TestClient`는 타이머를 시작하지 않습니다. [서버 조립 테스트](../crates/orbic/tests/schedules.rs)

`orbic::jobs::JobQueue`는 작업 ID, 실행 예정 시각, 시도 횟수와 처리 결과를 DB에 보관합니다. 워커는 임대한 작업만 갱신하거나 완료할 수 있습니다. 임대 만료 뒤 재실행될 수 있으므로 외부 쓰기에는 같은 작업 ID를 멱등 키로 사용합니다. 재시도 한도에 이르면 데드레터로, 외부 효과가 불명확하면 조정 대기 상태로 보관합니다. [두 데이터베이스의 워커 경쟁·재시도 테스트](../crates/orbic-jobs/tests/queue.rs)

## 5. Serge 프로덕션 UI

`serge` 피처를 켠 서버는 `orbic::site::SergeHost::open(root, policies, handler)`로 확인한 호스트를 앱 루트에 제공합니다. `Policies::public`, `guard`, `reserve`로 공개 그룹과 보호 경로, 추가 API 접두어를 지정합니다. SSR 핸들러는 번들과 같은 빌드 ID를 반환합니다. `reload`는 해시·파일·라우트·권한·빌드 ID 검사가 끝난 뒤에만 새 번들로 바꿉니다. [SSR 스트림·연결 종료와 가드 통합 테스트](../crates/orbic/tests/site.rs)

### 서비스 간 보상 작업

`jobs` 피처의 `JobQueue::sagas()`는 각 서비스의 실행·보상 작업 이름과 요청 입력을 같은 DB에 저장합니다. 호출자는 재시도에도 같은 Saga ID를 사용합니다. 워커는 `claim` 또는 `run_once`로 자신의 작업을 받고 외부 호출에 `Work::key`를 멱등 키로 전달합니다. 확실한 거부는 이미 커밋한 단계를 역순으로 보상합니다. 결과가 불분명한 호출과 자동 보상할 수 없는 단계는 `Reconcile`에 머무릅니다. 서비스에서 실제 결과를 조회한 뒤 `resolve(id, revision, decision)`을 호출해야 다음 단계로 넘어갑니다. SQLite와 PostgreSQL에서 중복 시작, 워커 경합·재시작, 취소 중 커밋 확인, 역순 보상과 오래된 워커·재조정 결과 거부를 검사합니다. [서비스 보상 테스트](../crates/orbic-jobs/tests/saga.rs)

## 5. 실시간 룸

`orbic/realtime`을 켜고 `Rooms::new(db, cache, retention)`으로 이벤트 보관 개수를 정합니다. `Rooms::define`의 리듀서는 기존 상태·주체·명령에서 새 상태와 이벤트를 계산합니다. 컴포넌트의 인증 가드 아래에 `.room(path, definition)`을 등록하면 해당 경로의 POST가 30초 동안 한 번 쓸 수 있는 티켓을 발급합니다. 서버는 티켓 해시를 보관하며 연결 중에도 세션 철회를 확인합니다.

`RoomServer`는 앱이 제공한 TLS 인증서로 WebTransport를 받고, 루프백 WebSocket을 함께 엽니다. TLS 종단에 직접 연결할 때는 `accept_websocket`을 사용합니다. 브라우저와 네이티브의 `serge::realtime::subscribe`는 연결마다 티켓을 새로 받아 마지막 적용 순번부터 재개합니다. 누락분이 보관 기간을 벗어나면 일관된 스냅샷이 도착합니다. 같은 룸·주체·요청 ID의 명령은 DB 트랜잭션에서 한 번 적용됩니다. 순서가 필요한 명령과 확인 응답은 신뢰성 있는 스트림을 사용합니다. [서버](../crates/orbic-realtime/src/lib.rs) · [SQLite·PostgreSQL 회귀 테스트](../crates/orbic-realtime/src/store/tests.rs)

## 6. 서비스 간 RPC와 이벤트

앱이 `orbic/msa`를 켜면 `Service::discover(&[url_a, url_b]).await?`로 같은 서비스를 제공하는 주소를 등록할 수 있습니다. 각 URL에는 컴포넌트 접두사를 포함합니다. `bearer`로 호출 주체를 설정하고 `hyve::api::provide_transport`로 제공하면 일반 서버 함수 스텁을 그대로 호출합니다. `monitor`는 종료 토큰을 받으며 각 복제본의 `/health`를 주기적으로 확인합니다. `replace`로 실행 중 주소 목록을 교체할 수 있습니다. 한 호출은 같은 복제본에서 바이너리를 먼저 시도하고 415일 때만 JSON으로 한 번 다시 보냅니다. 요청 ID와 W3C `traceparent`가 다음 서비스까지 이어집니다. [실행 테스트](../crates/orbic/tests/msa.rs) · [W3C Trace Context](https://www.w3.org/TR/trace-context/)

`EventBus::publish_in(&tx, topic, producer, id, payload)`는 업무 데이터와 이벤트를 한 트랜잭션으로 커밋합니다. `consume(topic, group, handler)`는 핸들러에 `Tx`를 전달하고 성공하면 그 그룹의 순번을 같은 트랜잭션에서 갱신합니다. 실패하거나 작업이 취소되면 양쪽 모두 롤백합니다. 각 소비 그룹은 독립적으로 진행하며 외부 시스템으로 보내는 작업에는 이벤트 ID를 멱등 키로 사용할 수 있습니다. [이벤트 구현](../crates/orbic-msa/src/events.rs)

## 7. 이메일·푸시·웹훅

`orbic/notify`에는 로컬 SMTP 수신함과 TLS SMTP 릴레이, 앱이 지정한 JSON 메일 API, APNs와 FCM v1 전송이 있습니다. APNs에는 앱의 공급자 JWT와 대상 토큰, FCM에는 OAuth 액세스 토큰을 호출할 때 전달합니다. Live Activity의 토픽과 HTTP/2 헤더는 어댑터가 생성합니다. 인증 정보는 앱이 관리합니다. 인수 테스트는 루프백 SMTP·HTTP/1.1·HTTP/2 수신기에서 메일 원문과 각 요청의 헤더·본문을 검사합니다. [알림 인수 테스트](../crates/orbic-notify/src/push/tests.rs) · [APNs 요청 규약](https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns) · [FCM HTTP v1](https://firebase.google.com/docs/cloud-messaging/send/v1-api)

서명 웹훅 POST 핸들러는 `Delivery`를 인자로 받아 `Webhooks::accept`에 전달합니다. 검증 대상은 원문 바이트와 발송자가 서명한 ID·타임스탬프입니다. 유효 시간은 5분이며 키 교체 기간에 여러 키를 받을 수 있습니다. 같은 발송자의 같은 ID와 본문은 재처리하지 않습니다. 수신 기록과 업무 데이터 변경은 SQLite 또는 PostgreSQL에서 같은 트랜잭션에 저장합니다. [실제 HTTP 수신 테스트](../crates/orbic/tests/webhooks.rs) · [Standard Webhooks 명세](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md)

### 실시간 힌트와 독립 명령 스트림

방의 `on_hint`에 등록한 핸들러는 현재 세션과 경로의 권한을 다시 확인한 후 1 KiB 이내의 힌트 프레임에 응답합니다. `send_hint`는 WebTransport에서 데이터그램을 쓰며, 힌트를 놓쳐도 영속 순번과 복구 커서는 그대로입니다. `send_stream`은 별도 단방향 스트림으로 명령 하나를 전송합니다. 같은 명령 ID를 다른 스트림이나 WebSocket으로 재전송해도 쓰기 트랜잭션은 한 번만 커밋됩니다. 세션당 동시 입력 스트림 수는 네 개, 명령 프레임 상한은 64 KiB입니다. 브라우저와 네이티브 클라이언트 모두 협상 결과에 따라 WebSocket으로 전환합니다. [서버 구현](../crates/orbic-realtime/src/lib.rs)
