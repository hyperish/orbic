# 04. 결정 기록 (orbic)

> 기준일 2026-11-26 · 결정은 **확정**이며, 이전 기록의 제안과 구현 상태는 각 항목에 표시합니다.

`orbic`의 설계 결정과 그 근거를 추적합니다.

## 열려 있는 결정

현재 0건입니다.

## 결정됨

- **#1 · 배포 사업자 연동** (2026-11-26). Cloudflare 등 특정 사업자의 서비스 연동은 해당 서비스를 선택한 앱이 맡습니다. Orbic은 네이티브 HTTP 서버와 선택형 파일 저장 어댑터의 동작을 정의합니다. [배포와 파일 저장](03-adapters-and-pipeline.md#deployment-storage)
- **Phase 2 HTTP 종단과 개발 캐시** (2026-11-26). TLS·HTTP/2는 신뢰하는 프록시에서 종단합니다. 개발 L2는 명시한 Redis/Valkey에 격리된 키 공간으로 연결합니다. [HTTP 종단](03-adapters-and-pipeline.md#http-origin) · [개발 캐시](01-dx-and-dsl.md#dev-cache)

- **`/health`·`/metrics`·`/docs`의 와이어 형식 확정** (2026-11-24, 열린 결정 3을 제안대로 결정하고 세부를 정함):
  - 셋 다 `orbic-adapter-http`가 파이프라인 앞에서 답하는 앱 루트 경로입니다. 마운트 접두어·응답 봉투·가드·CORS를 적용하지 않고 보안 헤더와 `X-Request-Id`만 붙입니다. 앱 라우트가 이 경로와 겹치면 HTTP 앱을 만들 때 실패합니다.
  - `GET /health`: 요청을 받는 중이면 `200 text/plain` `ok`, 셧다운이 시작되면 `503` `shutting down`입니다.
  - `GET /metrics`: Prometheus 텍스트 형식(0.0.4)으로 상태 코드 대역별 요청 수, 처리 중 요청 수, 처리 시간의 합계와 건수를 냅니다. 내장 경로 요청은 세지 않습니다. 직접 연결한 상대의 주소가 허용 목록(기본은 루프백)에 있고 전달 헤더(`X-Forwarded-For`, `Forwarded`)가 없을 때만 답하며, 그 밖은 `404 route_not_found`입니다. 같은 호스트의 프록시를 거친 외부 요청이 루프백으로 보이는 경우를 막기 위함입니다.
  - `/health`와 `/metrics`는 HTTPS 요구(`03-adapters-and-pipeline.md` §1 TLS)에서 제외합니다. 프로브와 수집기는 프록시를 거치지 않고 평문으로 직접 접속하고, 두 경로는 자격 증명을 받지 않습니다.
  - OpenAPI: `.openapi(path)`로 정한 경로의 `GET`이 OpenAPI 3.1 JSON 문서 자체(`application/json`, 봉투 없음)를 돌려줍니다. 인증 없이 열려 있고, 공개 계약만 담습니다. Phase 1은 HTML 뷰어를 제공하지 않으므로 기본 보안 헤더(CSP 포함)를 그대로 적용합니다.
  - 로그: Phase 1은 `hyve-trace` 없이, 공개 형태로 가려지는 오류(`Internal`, 공개 코드가 아닌 `Infra`)의 원문만 `requestId`와 함께 표준 오류 출력에 한 줄로 남깁니다. 원문은 가리기 전에만 있으므로 이 기록은 `orbic-core`가 합니다(전송 중립이라 인프로세스 호출에도 적용).

- **세션·멱등 키·`RateLimit` 저장소와 `Cache`의 관계 확정** (2026-11-24, 열린 결정 2를 제안대로 결정): `orbic-cache`의 `Cache`는 밀어내는 L1 캐시와 밀어내지 않는 저장소(세션, 멱등 키, `RateLimit` 카운터)를 함께 담되 저장소는 캐시 용량과 무관하게 TTL로만 만료됩니다. 가드(`RateLimit`, `AuthGuard`)와 멱등성 게이트는 자신이 놓인 범위에 `provide`된 `Cache`에서 저장소를 얻고, 없으면 조립이 실패합니다. `POST`로 바인딩한 라우트는 `Idempotency-Key`를 받으므로 그 범위에 `Cache`가 있어야 합니다. 이를 위해 `orbic-core`가 `orbic-cache`에 의존합니다(`orbic-cache`는 파이프라인을 모르는 저장소 계층). 가드는 자신이 필요로 하는 값을 선언하고(`Guard::needs`) 조립이 이를 핸들러의 `Ctx<T>`처럼 검사합니다 (`02-architecture.md` §2, `03-adapters-and-pipeline.md` §1, `05-implementation-plan.md` §2)

- **Rust 스타일 `BackendComponent` 조립 아키텍처 및 `loco.rs` 레퍼런스 확정** (2026-11-17 ~ 2026-11-18): `00-overview.md`, `01-dx-and-dsl.md`
- **DB 투트랙 쿼리(`#[table]` 체이닝 + `sql!` 매크로), L1/L2 Tiered 캐시(Singleflight·태그 무효화), 웹 쿠키+네이티브 Bearer 통합 Opaque 세션(권위 저장소 직결 즉시 철회 기본 — 2026-11-24 보정), Dual-Rail 네트워크(표준 HTTP/WS + 고속 `hyve-serde`/WebTransport) 및 필수 백엔드 도메인 확정** (2026-11-18 ~ 2026-11-19): `00-overview.md`, `02-architecture.md`, `03-adapters-and-pipeline.md`
- **crates.io 비충돌 고유명 `orbic`(구 `ring`) 및 순수 `MIT` 라이선스 확정** (2026-11-19): 레포명·crates.io 패키지명·Rust 크레이트명을 `orbic`으로 1:1 일치
- **MIT 포크 스택(`hyper`+`matchit`·`sqlx-core`·`quick_cache`·`fred`·`quinn`/`wtransport`·`tokio-tungstenite`·`utoipa`), 내부 코어 개조 방향, `#[server]`↔`BackendComponent::handle` 명시 등록 계약 및 Phase 1~3 컷 확정** (2026-11-19):
  - Phase 1(`v0.1.0`): `orbic-core`·`orbic-macros`(`#[table]`·`sql!`)·`orbic-adapter-http`(표준 레일 + 고속 레일 요청-응답)·`orbic-db`(`sql!` 캐시 생성·임시 PostgreSQL 하네스 포함)·`orbic-cache` 인메모리 저장소(세션·멱등 키·`RateLimit`)·`orbic-auth`(권위 저장소 직접 조회)·`orbic-openapi`·`TestClient`
  - Phase 2(`v0.2.0`): `orbic-cache` L2(Redis/Valkey)·Tiered(L1+L2)·`orbic-storage`·`orbic-jobs`(DLQ·분산 락 `.every`)·`orbic-adapter-serge`(SSR/SSG)
  - Phase 3(`v0.3.0+`): `orbic-adapter-ws`(WebTransport+WS 폴백)·`orbic-realtime`·`orbic-msa`·SAGA·`orbic-notify` (`01-dx-and-dsl.md` §2, `02-architecture.md` §3)
- **Phase 1 의존 폐포 보정 및 세션 권위 저장소 정리** (2026-11-24): `orbic-db`가 쓰는 `orbic-macros`와 세션·멱등 키·`RateLimit`가 쓰는 `orbic-cache` 인메모리 저장소를 Phase 1에 포함(위 목록에 반영). 즉시 철회의 확정 내용은 "권위 저장소 직접 조회"이며, 단일 노드는 인프로세스 저장소가, 멀티 레플리카는 L2(Redis/Valkey)가 권위 저장소 (`00-overview.md` §1, `03-adapters-and-pipeline.md` §2)
- **Dual-Rail 폴백에서도 고속 레일은 `hyve-serde` 바이너리 유지** (2026-11-24): 고속 레일의 요청-응답은 `orbic-adapter-http` 바이너리 본문, 스트림·실시간은 WebTransport이며 WS로 폴백해도 바이너리 프레임을 유지. JSON 텍스트 프레임은 표준 레일 전용 (`00-overview.md` §1, `02-architecture.md` §1, §3, `03-adapters-and-pipeline.md` §2)
- **`#[server]` 게이팅·소유 기준 정리** (2026-11-24): 클라이언트 빌드 조건 표기를 `not(feature = "server")`로 바로잡고, 제거 기준이 타깃이 아니라 `server` 피처임을 명시. 매크로는 `hyve-macros` 소유이고 메서드·경로 인자는 `orbic`만 해석 (`01-dx-and-dsl.md` §2)
- **`orbic-core`와 `orbic-adapter-http`의 분담 정리** (2026-11-24): 코어 무오염 원칙에 맞춰 CORS·보안 헤더·`413`·`/health`·`/metrics` 노출은 `orbic-adapter-http`로, 전송 중립 요청 파이프라인과 셧다운 조율은 `orbic-core`로 정리. 파이프라인은 `orbic-core`가 소유하고 모든 어댑터와 `TestClient`가 같은 진입점을 호출 (`00-overview.md` §1, `02-architecture.md` §1, §3, `03-adapters-and-pipeline.md` §1)
- **포크 표 보정** (2026-11-24): `quinn` 라이선스 기재, 표의 버전은 확인 시점 스냅샷으로 취급 (`02-architecture.md` §3)
- **`sql!` 오프라인 검증 방식과 방언 테스트 분리 확정** (2026-11-24): 마이그레이션 AST 방식 대신 레포에 커밋하는 오프라인 메타데이터 캐시를 채택. 매크로는 컴파일 중 DB에 접속하지 않고, 캐시에 없는 쿼리는 `hyve codegen` 안내가 담긴 컴파일 오류가 됨. PostgreSQL은 캐시를 만들거나 점검하는 작업에만 필요. `Db::in_memory()`는 SQLite 인메모리이며, 검증되지 않은 방언의 DB에서 `sql!`을 실행하면 명시적 오류. 캐시 생성(`codegen` 단계)과 임시 PostgreSQL 하네스는 `orbic-db`와 함께 Phase 1에 속함. 컴파일 타임 스키마 지문 비교는 **제안**으로 남김 (`00-overview.md` §1, `01-dx-and-dsl.md` §4)
- **`#[server]` 요청 모델·오류 복원 규칙 반영** (2026-11-24): `hyve`가 확정한 호출 스텁 계약(`hyve/docs/02-architecture.md` §2.4)에 맞춰, 전송 인자 1개는 요청 본문 그대로 받고 표준 레일 오류는 03 규약 봉투에 필드를 추가하지 않으며, HTTP 변환이 오류 종류와 재시도 가능 여부를 왕복 가능하게 보존 (`01-dx-and-dsl.md` §2, `03-adapters-and-pipeline.md` §1)
- **네이티브 서버 런타임 확정** (2026-11-24): 기본 런타임은 네이티브 멀티스레드 서버이며 비동기 런타임은 패밀리 공통의 `tokio`(외부 의존성). 엣지 런타임 어댑터는 미정으로 남기고 결정 시점을 적음 (`02-architecture.md` §3, `hyve/docs/02-architecture.md` §2.5)
- **포크 단위에 DB 드라이버 크레이트 포함** (2026-11-24): `sqlx-core`만으로는 드라이버가 없어 `sqlx-postgres`·`sqlx-sqlite`를 같은 포크 단위로 포함 (`02-architecture.md` §3)
- **세션 토큰과 파이프라인 순서의 보안 규칙 반영** (2026-11-24): 공통 명세가 확정한 규칙(`hyperish/docs` `02-core-contracts.md` §6)에 맞춰, 토큰은 OS 암호학적 난수원의 256비트 값, 멱등 응답 재사용은 인증·권한 검사 뒤이고 키 범위에 인증 주체 포함, 세션 철회 시 같은 계열 리프레시 토큰 폐기 (`03-adapters-and-pipeline.md` §1, §2)
- **구현 전 세부 규칙 추가(제안)** (2026-11-24): 조립 계약(`provide` 범위, 조립 시점 검증, `wrap` 순서와 1회 실행, 추출자, 리터럴 검증, OpenAPI와 와이어 일치), 본문 없는 메서드 바인딩의 표준 레일 매핑, 운영 방언 선언 위치와 캐시 위치, `RateLimit` 창, 레일의 미디어 타입 구분 (`01-dx-and-dsl.md` §1, §2, §4, `03-adapters-and-pipeline.md` §1, §2)
- **Phase 1 구현 계획 문서 추가** (2026-11-24): 작업 순서와 완료 기준을 `05-implementation-plan.md`에 둠
- **`#[server]` 스텁의 인프로세스 호출 경로** (2026-11-24): 스텁이 모든 빌드에 있게 되면서(`hyve/docs/04-open-questions.md`), 같은 프로세스의 호출은 `orbic`의 인프로세스 전송이 요청 파이프라인에 넣음. `TestClient`가 전송 트레이트를 구현해 두 레일을 검증 (`01-dx-and-dsl.md` §2, `03-adapters-and-pipeline.md` §1)
- **경계 세부 추가(제안)** (2026-11-24): 오류 종류별 상태 코드 변환과 프레임워크 오류 코드, 멱등 게이트의 적용 대상·재사용 범위·본문 비교 기준, `RateLimit` 키, 코어와 HTTP 어댑터 사이의 메타데이터 경계, `TestClient`가 통과하는 계층, Phase 1의 TLS 종단, 인증 라우트는 앱이 정의, 낙관적 잠금은 전용 API 없음, 공유 모델의 `#[table]` 생성물 기준, `sql!` 캐시·마이그레이션 위치와 `hyve-orbic` 실행 파일 (`01-dx-and-dsl.md` §1, §2, §4, `02-architecture.md` §1, `03-adapters-and-pipeline.md` §1)
- **문서 사이의 어긋남 정리** (2026-11-24): Phase 컷의 상태 표기를 확정으로 맞추고(`00-overview.md`), `orbic-macros`의 범위에서 조립용 매크로 표기를 뺌(`02-architecture.md` §1), 멱등 키 적용 대상을 공통 명세의 "생성성 요청"에 맞춤(`03-adapters-and-pipeline.md` §1)
- **2차 점검 반영** (2026-11-24): 인증 주체 타입(`CurrentUser`)은 앱이 정의해 `orbic-auth`에 등록, 공유 크레이트는 기본 피처를 끈 `orbic`에 의존, `server` 피처를 켜는 빌드에 서버·UI 단일 프로세스 포함, 스키마 판정을 라우트 매칭 단계로, `RateLimit` 헤더는 가드에 도달한 요청에만. 모두 **제안**이며 `server` 피처 문구만 확정 절의 정정 (`01-dx-and-dsl.md` §1, §2, `03-adapters-and-pipeline.md` §1)
