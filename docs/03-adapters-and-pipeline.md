# 03. 파이프라인과 호환 어댑터 (orbic)

> 상태: 코어 `ref` + 호환 어댑터 분리 원칙은 **확정**, 도메인별 어댑터 세부는 **제안** · 기준일: 2026-11-24

`orbic`의 모든 백엔드 도메인(DB, Cache, MSA, WS/WT)은 **내부 고성능 코어 파이프라인**과 **외부·인프라 호환 어댑터**의 2단 구조로 동작합니다(`hyperish/docs` `02-core-contracts.md` §2~§7).

## 1. 백엔드 요청·작업 파이프라인

이 파이프라인은 `orbic-core`가 소유하며, 외부 어댑터·내부 MSA 채널·인메모리 `TestClient`가 모두 같은 진입점을 호출합니다. 어댑터와 `TestClient`는 아래 단계를 복제하지 않습니다(`hyperish/docs` `02-core-contracts.md` §5).

1. **어댑터 수신 및 컨텍스트 생성**: 외부 어댑터(`orbic-adapter-http`, `orbic-adapter-ws` 등) 또는 내부 MSA 채널이 요청을 받아 상관관계 ID(`requestId`)와 구조적 취소 토큰(`CancellationToken`)을 포함한 요청 컨텍스트(핸들러에는 `Ctx<T>`로 주입)를 생성합니다.
2. **접근 권한·멱등성 게이트**:
   - 보호 대상은 요청 시점마다 소유 계층이 권한과 철회 여부를 검사합니다(`02-core-contracts.md` §6).
   - 부수효과가 있는 생성성 요청(`POST`로 바인딩한 라우트)의 `Idempotency-Key`는 `orbic-cache`를 통해 중복 실행과 동시 처리(`409 request_in_progress`)를 제어합니다. 게이트는 키가 온 요청에만 작동하고, 키가 없으면 그대로 처리합니다. 처음 응답은 성공이든 `Domain` 오류든 그대로 재사용하고, `Infra`·`Internal` 오류는 저장하지 않아 다시 시도할 수 있습니다. "같은 본문"은 레일(미디어 타입)과 본문 바이트의 다이제스트가 모두 같은 것이며, 같은 키를 다른 레일로 보내면 `409 idempotency_key_mismatch`입니다. 보관 기간은 구현이 정하고 문서화합니다.
   - 순서는 라우트 매칭 → `wrap` 가드(선언한 순서대로) → 멱등성 게이트 → 추출·검증 → 핸들러입니다. 멱등 응답 재사용은 항상 인증·권한 검사 뒤이고, 키의 범위는 인증 주체·메서드·경로·키의 조합입니다(`hyperish/docs` `02-core-contracts.md` §6).
   - `RateLimit`는 선언한 창(`per_minute(60)`이면 1분)이 지나면 초기화되며, 남은 횟수와 초기화까지 남은 시간을 `hyperish/docs` `03-api-protocol.md` §8의 헤더로 그 범위의 모든 응답에 싣습니다. 키와 범위는 `01-dx-and-dsl.md` §1의 조립 계약이 정합니다.
   - `orbic-core`는 멱등 키, 속도 제한 정보, 자격 증명을 전송 중립 요청·응답 메타데이터로 다룹니다. 이를 HTTP 헤더(`Idempotency-Key`, `RateLimit-*`, 쿠키, `Authorization`, `X-CSRF-Token`)와 맞바꾸는 것은 `orbic-adapter-http`입니다.
3. **도메인 실행 및 커밋/보상**:
   - `orbic-db` 트랜잭션과 `orbic-jobs` 워커는 취소 가능 지점과 커밋 지점을 명시하며, 커밋 이후 실패 시 보상 절차 또는 재조정(reconciliation) 상태를 기록합니다(`02-core-contracts.md` §3).
4. **응답 변환**: 내부 구조화 오류(`Domain` / `Infra` / `Internal`)는 어댑터 경계에서 내부 스택 노출 없이 외부 규약의 오류 형식으로 변환됩니다. `#[server]` 스텁이 두 레일에서 같은 오류를 복원할 수 있도록, HTTP 변환은 오류의 종류와 재시도 가능 여부를 상태 코드와 `Retry-After`로 왕복 가능하게 정의합니다(`Domain`은 4xx, `Infra`는 `503`, `Internal`은 `500` — `hyperish/docs` `03-api-protocol.md` §4, `01-dx-and-dsl.md` §2).
   - `Domain`: `code`가 `03-api-protocol.md` §4 표에 있으면 그 상태 코드, `_not_found`로 끝나면 `404`, 그 밖은 `422`입니다. `code`는 그대로 내보냅니다.
   - `Infra`: `503`입니다. `code`는 오류가 공개용으로 선언한 값이면 그대로, 아니면 `service_unavailable`입니다. 재시도 가능하면 `Retry-After`를 붙입니다.
   - `Internal`: `500 internal_error`입니다. 원문은 로그에만 남깁니다.
   - 프레임워크가 내는 오류: 라우트 없음 `404 route_not_found`, 메서드 불일치 `405 method_not_allowed`, 본문 크기 초과 `413 payload_too_large`, 받을 수 없는 본문 형식 `415 unsupported_media_type`, 고속 레일 스키마 불일치 `415 schema_mismatch`, CSRF 실패 `403 csrf_token_invalid`.
   - 고속 레일은 같은 상태 코드에 아카이브한 오류 값을 싣습니다(`hyve/docs/03-adapters-and-pipeline.md` §3.3).

- **`TestClient`의 위치**: `TestClient`는 소켓만 빼고 `orbic-adapter-http`의 변환 계층(상태 코드, 헤더, 봉투, CORS, 보안 헤더, 요청 크기 제한)을 그대로 통과합니다. 그래서 같은 시나리오가 `TestClient`와 실제 서버에서 같은 응답을 냅니다. `TestClient`는 `hyve-api`의 전송 트레이트도 구현해 `#[server]` 스텁을 두 레일로 구동합니다.
- **TLS (제안)**: Phase 1의 `orbic-adapter-http`는 HTTP/1.1을 서빙하고 TLS 종단은 앞단 프록시에 맡깁니다. 운영 설정에서는 신뢰하도록 지정한 프록시가 전달한 스킴이 HTTPS가 아닌 요청을 거부하고, 루프백에 바인딩한 개발·테스트 서버만 평문을 허용합니다. 자체 TLS와 HTTP/2는 Phase 2에서 정합니다.

## 2. 호환 어댑터 구성

| 영역 | 코어 추상화 (`orbic-*`) | 호환 어댑터 및 동작 방식 |
|---|---|---|
| **Dual-Rail API 전송** | `hyve-api` + `orbic-core` | • **표준 레일**: 외부 호환용 **HTTP/JSON** (`hyperish/docs` `03-api-protocol.md` 준수: 커서 페이지네이션, `expectedVersion` 낙관적 잠금, `RateLimit-*` 헤더)<br>• **고속 레일**: `serge` ↔ `orbic` 인라인 서버 함수 및 MSA 호출 시 **`hyve-serde` 제로카피 바이너리** 직결(요청-응답은 `orbic-adapter-http`의 바이너리 본문, 스키마가 어긋나면 표준 레일로 폴백). 두 레일은 같은 경로에서 미디어 타입으로 구분하며, 와이어 형식은 `hyve/docs/03-adapters-and-pipeline.md` §3을 따름 및 클라이언트 언마운트 시 종단 간 취소(`CancellationToken`) 전파 |
| **Dual-Rail 실시간 (WT / WS)** | `orbic-realtime` (`seq`, 룸 상태, 중복 제거) | **WebTransport(HTTP/3 QUIC 다중 스트림·데이터그램)**를 1순위로 연결하고, QUIC/UDP가 차단된 네트워크에서는 **WebSocket**(`04-realtime-communication.md`)으로 자동 폴백. 고속 레일은 폴백 WS에서도 `hyve-serde` 바이너리 프레임을 유지하고, JSON 텍스트 프레임은 외부 클라이언트용 표준 레일에만 씀. 브라우저 WebTransport도 커스텀 인증 헤더를 보낼 수 없으므로 동일한 `?ticket=` 일회용 티켓 핸드셰이크를 쓰며, 폴백 전환 시에도 `seq`·중복 제거 `id` 연속성을 유지 |
| **인증·세션 (`orbic-auth`)** | `orbic-auth` + `orbic-cache` | 웹(`HttpOnly` 쿠키 + `X-CSRF-Token`)과 네이티브/CLI(`Bearer` 토큰) 이중 전송 어댑터. 즉시 철회 보장을 위해 **Opaque 세션의 권위 저장소 직접 조회를 기본**(단일 노드는 인프로세스 저장소, 멀티 레플리카는 L2 Redis/Valkey)으로 하며, 멀티 레플리카에서 L1 캐싱 옵트인 시 초단기 TTL·단절 시 L1 전체 플러시·노출 창 명시(`02-core-contracts.md` §6). 세션·CSRF·리프레시 토큰은 OS의 암호학적 난수원에서 만든 256비트 값이며(`hyperish/docs` `03-api-protocol.md` §3의 64비트 이상 요구를 충족) `Ulid` 같은 식별용 ID를 쓰지 않음. 세션을 철회하면 같은 계열의 리프레시 토큰도 폐기함. 단일 노드의 인프로세스 저장소는 프로세스를 재시작하면 세션이 사라짐 |
| **DB & Cache** | `orbic-db`, `orbic-cache` | **PostgreSQL** 및 **SQLite/LibSQL** 드라이버 어댑터(`#[table]` 체이닝 + `sql!` 매크로), **L1 인메모리(`quick_cache` S3-FIFO) + L2 `Redis/Valkey`(`fred` RESP3)** Tiered 캐시 어댑터(Singleflight·태그 무효화) |
| **Storage · Jobs · Notify** | `orbic-storage`, `orbic-jobs`, `orbic-notify` | 로컬/S3/R2 멀티파트 업로드(크기·체크섬 검증 후 활성화), 지연·재시도·DLQ 작업 큐 및 Cron 스케줄러, 이메일·푸시(APNs/FCM)·HMAC 웹훅 어댑터 |
| **`serge` 웹 서빙** | `orbic-adapter-serge` | `serge`가 산출한 정적 에셋 매니페스트(`SergeBundleManifest`), 라우트 메타데이터, `serge-web-ssr` 스트리밍 핸들러(`SergeSsrHandler`)를 받아 프로덕션 서빙 (`serge/docs/03-adapters-and-pipeline.md` §3 참조) |
