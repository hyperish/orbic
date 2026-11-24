# 05. 구현 계획 (orbic)

> 상태: **제안** · 기준일: 2026-11-24

Phase 1(`v0.1.0`)의 작업 순서와 완료 기준입니다. Phase 1에 들어가는 크레이트는 [`04-open-questions.md`](04-open-questions.md)의 Phase 컷이, 크레이트 구조와 책임은 [`02-architecture.md`](02-architecture.md)가 정합니다. 패밀리 차원의 완료 기준은 `hyperish/docs`의 `00-vision.md`에 있습니다.

`orbic`은 `hyve`의 `0.1.0-alpha.N` 프리릴리스가 배포된 뒤 그 버전에 의존해 시작합니다(`hyperish/docs` `05-development-rules.md` §5).

## 1. 출발점

- 현재 코드는 서드파티 의존성이 없는 자리표시자입니다. 크레이트는 `orbic`과 `orbic-macros`뿐이고, 요청 파이프라인 전체가 `TestClient` 안에 들어 있습니다. 고쳐 쓰지 않고 교체합니다.
- 공개 API의 기준은 문서입니다. `crates/orbic/tests/mvp.rs`는 문서 예시를 옮긴 테스트로 다시 씁니다.
- 구현에 앞서 할 일:
  1. 포크 대상의 기준 리비전을 다시 확인해 고정하고 기록합니다(`hyperish/docs` `05-development-rules.md` §7).
  2. 워크스페이스를 `02-architecture.md` §1의 크레이트 구조로 재편합니다.
  3. `hyve` 의존을 프리릴리스의 정확한 버전 요구로 바꾸고, `Cargo.lock`을 커밋하고, CI에 최소 Rust 버전 빌드를 추가합니다.

## 2. 작업 순서

| 단계 | 크레이트 | 산출물 | 앞선 단계 |
|---|---|---|---|
| 1 | `orbic-core` | `BackendComponent` 트리, `provide` 범위 해석, `wrap` 순서, 라우트 트리, 전송 중립 요청 파이프라인, 조립 시점 검증, 셧다운 조율 | 없음 |
| 2 | `orbic-macros` | `#[table]`·`#[pk]`, `sql!`(캐시 읽기), `ttl!`·`key!`·`tag!` | 없음 |
| 3 | `orbic-cache` 인메모리 저장소 | 세션·멱등 키·`RateLimit` 저장소, Singleflight, 태그 무효화, `orbic-core`의 멱등성 게이트와 `RateLimit` 가드 | 1 |
| 4 | `orbic-db` | `sqlx-core`와 드라이버 포크 편입, 체이닝 쿼리, 트랜잭션과 취소 시 롤백, 마이그레이션, `Db::in_memory()`, 임시 PostgreSQL 하네스, `hyve-orbic codegen`(`sql!` 캐시 생성과 점검) | 1, 2 |
| 5 | `orbic-auth` | 불투명 세션, CSRF, Bearer, `AuthGuard`, 리프레시 회전, 철회 | 1, 3 |
| 6 | `orbic-adapter-http` | `hyper` 포크 편입, 표준 레일, 고속 레일 요청-응답, CORS·보안 헤더·요청 크기 제한·`/health`·`/metrics`, 오류 변환 | 1 |
| 7 | `orbic-openapi` | 스키마 수집과 `/docs` 서빙 | 1, 6 |
| 8 | `orbic` | 재수출, prelude, `TestClient`, 인프로세스 전송 | 1~7 |

- **진행 메모 (2026-11-24)**:
  - `hyve`가 아직 배포되지 않아, 워크스페이스는 형제 경로와 정확한 버전 요구를 함께 적어 의존합니다(`hyperish/docs` `05-development-rules.md` §5). CI는 `hyve`를 옆에 체크아웃합니다. 배포 뒤에는 레지스트리 버전 요구만 남깁니다.
  - 파이프라인 테스트는 자기 `server` 피처를 가진 미배포 픽스처 크레이트 `orbic-tests`에 두고, CI가 피처를 끈 빌드와 켠 빌드를 모두 검사합니다.
  - 1단계 `orbic-core`의 요청은 세 레일(표준 JSON, 고속 바이너리, 직렬화 없는 인프로세스)을 받습니다. 라우트 매칭(고속 레일의 함수 식별자·스키마 지문 판정 포함)이 가드보다 먼저이고, 오류는 응답에 싣기 전에 공개 형태와 `requestId`를 갖춥니다. 프레임워크 오류 코드는 `route_not_found`, `method_not_allowed`, `unsupported_media_type`, `schema_mismatch`, 셧다운 중의 재시도 가능한 `service_unavailable`입니다.
  - 2단계 `orbic-macros`는 `#[table]`이 만드는 메타데이터와 `sql!` 결과가 기댈 타입(`orbic-db`의 값·테이블·`Sql`)과 `key!`·`tag!`의 키 타입(`orbic-cache`)을 함께 만듭니다. 이 크레이트들의 나머지는 3·4단계입니다. `sql!` 캐시 형식은 별도 크레이트 `orbic-sql-cache`입니다(`02-architecture.md` §1). §1 스케치의 `Id::new()` 인자는 바인딩할 타입이 정해지지 않아 `Id::<Project>::new()`로 고쳤습니다.
  - 3단계: 열린 결정 2를 결정하면서 `orbic-core`가 `orbic-cache`에 의존합니다(저장소 계층이 아래). 그래서 게이트와 `RateLimit` 가드는 `orbic-core`에, 저장소는 `orbic-cache`에 있습니다. 표의 "앞선 단계 1"은 게이트와 가드가 1단계의 파이프라인 위에 놓인다는 뜻으로 남깁니다. L1은 `quick_cache =0.7.0`을 포크하지 않고 정확한 버전으로 고정해 씁니다(`02-architecture.md` §3 실행 기록).
  - 4단계: `sqlx` 0.9.1의 최소 Rust 버전(1.94)에 따라 `orbic` 워크스페이스의 최소 Rust 버전을 1.94로 올렸습니다(`02-architecture.md` §3 실행 기록). PostgreSQL 테스트와 `hyve-orbic codegen --check`(픽스처 캐시가 최신인지)는 `HYPERISH_TEST_POSTGRES_URL`이 필요하므로 CI에 PostgreSQL 서비스를 둡니다. 저장소 루트의 `hyve.toml`·`migrations/`·`.orbic/sql/`은 `orbic-tests`가 쓰는 픽스처 앱 워크스페이스입니다. 2단계의 캐시 형식은 엔진이 실제로 알려주는 정보에 맞춰 `null`(미확인)을 받도록 바꿨고, 2단계에 손으로 쓴 캐시 파일을 `hyve-orbic codegen`이 같은 내용으로 다시 만들어 냄을 확인했습니다(파라미터의 `nullable` 필드 제거만 다름).

`.every`(잡)와 L2 캐시는 Phase 2, `.room`(실시간)과 MSA는 Phase 3입니다.

## 3. 완료 기준

- **문서 예시**: `01-dx-and-dsl.md` §1 스케치 중 `Project`·`CreateProjectInput` 모델, `list_projects`, `create_project`, `app()`, `create_project_rejects_empty_title` 테스트가 컴파일되고 통과합니다. `project_component()`에서는 `.room`과 `.every` 줄을 제외합니다. §2의 `create_project_rpc`와 `project_rpc_component()`도 포함합니다. 스케치가 생략한 앱 쪽 정의(`User`, `test_user()`)는 테스트 픽스처로 채웁니다.
- **조립 계약**: `provide` 범위, 누락된 주입과 중복 라우트의 조립 실패, 여러 겹 `mount`에서 가드가 한 번만 실행되는 것을 테스트로 확인합니다.
- **HTTP 규약**: `hyperish/docs` `03-api-protocol.md`의 응답 봉투, 상태 코드 표, 커서 페이지네이션, 멱등성(재사용, 본문 불일치, 처리 중), 낙관적 잠금, 속도 제한 헤더를 계약 테스트로 확인합니다.
- **보안**: 인증 없는 요청이나 다른 주체가 멱등 응답을 재사용하지 못하고, 토큰이 암호학적 난수이며, 철회한 세션은 리프레시 토큰으로 되살릴 수 없고, 내부 결함 응답에 내부 타입명이 나오지 않습니다.
- **단일 파이프라인**: 같은 시나리오를 `TestClient`와 실제 HTTP 서버에 돌려 같은 응답을 얻습니다.
- **두 레일**: `TestClient`에 레일을 지정해 `#[server]` 스텁을 호출하고, 고속 레일과 표준 레일에서 같은 결과와 같은 오류가 나오고 스키마가 어긋나면 표준 레일로 내려가는 것을 확인합니다.
- **DB**: 실패하거나 취소된 트랜잭션은 자신의 변경만 되돌리고 다른 트랜잭션의 커밋에 영향을 주지 않습니다. 캐시에 없는 `sql!`은 컴파일 오류가 되고, 검증되지 않은 방언에서의 실행은 명시적 오류가 됩니다.
- **OpenAPI**: 생성된 문서가 실제 응답(봉투, 오류, 인증 요구)과 일치합니다.
- **포크 검증**: 포크본이 원본과 달라진 점과 측정 결과를 남깁니다.
- **CI**: 포맷, 린트, 테스트, 최소 Rust 버전 빌드가 통과하고, 공개 크레이트 전부가 패키징됩니다. 충족하면 `0.1.0-alpha.N` 프리릴리스를 배포합니다.
