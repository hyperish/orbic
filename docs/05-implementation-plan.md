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
  - `hyve`가 아직 배포되지 않아, 워크스페이스는 형제 경로와 정확한 버전 요구를 함께 적어 의존합니다(`hyperish/docs` `05-development-rules.md` §5). CI는 `hyve`를 옆에 체크아웃합니다. 배포 뒤에는 레지스트리 버전 요구만 남깁니다. CI는 공개 크레이트 10개를 패키징하고 각 패키지만으로 빌드하며, 아직 배포되지 않은 `hyve` 버전은 형제 체크아웃을 `[patch.crates-io]`로 대신 씁니다(2026-11-25).
  - 파이프라인 테스트는 자기 `server` 피처를 가진 미배포 픽스처 크레이트 `orbic-tests`에 두고, CI가 피처를 끈 빌드와 켠 빌드를 모두 검사합니다.
  - 1단계 `orbic-core`의 요청은 세 레일(표준 JSON, 고속 바이너리, 직렬화 없는 인프로세스)을 받습니다. 라우트 매칭(고속 레일의 함수 식별자·스키마 지문 판정 포함)이 가드보다 먼저이고, 오류는 응답에 싣기 전에 공개 형태와 `requestId`를 갖춥니다. 프레임워크 오류 코드는 `route_not_found`, `method_not_allowed`, `unsupported_media_type`, `schema_mismatch`, 셧다운 중의 재시도 가능한 `service_unavailable`입니다.
  - 2단계 `orbic-macros`는 `#[table]`이 만드는 메타데이터와 `sql!` 결과가 기댈 타입(`orbic-db`의 값·테이블·`Sql`)과 `key!`·`tag!`의 키 타입(`orbic-cache`)을 함께 만듭니다. 이 크레이트들의 나머지는 3·4단계입니다. `sql!` 캐시 형식은 별도 크레이트 `orbic-sql-cache`입니다(`02-architecture.md` §1). §1 스케치의 `Id::new()` 인자는 바인딩할 타입이 정해지지 않아 `Id::<Project>::new()`로 고쳤습니다.
  - 3단계: 열린 결정 2를 결정하면서 `orbic-core`가 `orbic-cache`에 의존합니다(저장소 계층이 아래). 그래서 게이트와 `RateLimit` 가드는 `orbic-core`에, 저장소는 `orbic-cache`에 있습니다. 표의 "앞선 단계 1"은 게이트와 가드가 1단계의 파이프라인 위에 놓인다는 뜻으로 남깁니다. L1은 `quick_cache =0.7.0`을 포크하지 않고 정확한 버전으로 고정해 씁니다(`02-architecture.md` §3 실행 기록).
  - 4단계: `sqlx` 0.9.1의 최소 Rust 버전(1.94)에 따라 `orbic` 워크스페이스의 최소 Rust 버전을 1.94로 올렸습니다(`02-architecture.md` §3 실행 기록). PostgreSQL 테스트와 `hyve-orbic codegen --check`(픽스처 캐시가 최신인지)는 `HYPERISH_TEST_POSTGRES_URL`이 필요하므로 CI에 PostgreSQL 서비스를 둡니다. 저장소 루트의 `hyve.toml`·`migrations/`·`.orbic/sql/`은 `orbic-tests`가 쓰는 픽스처 앱 워크스페이스입니다. 2단계의 캐시 형식은 엔진이 실제로 알려주는 정보에 맞춰 `null`(미확인)을 받도록 바꿨고, 2단계에 손으로 쓴 캐시 파일을 `hyve-orbic codegen`이 같은 내용으로 다시 만들어 냄을 확인했습니다(파라미터의 `nullable` 필드 제거만 다름).
  - 5단계: 인증 주체 타입의 등록 방식과 세션 쿠키 이름의 위치는 `01-dx-and-dsl.md` §1(인증 엔드포인트의 구현 세부)에 적었습니다. 세션의 무조건 만료는 벽시계(`Timestamp::now`) 기준이고, 유휴 만료는 `Cache`의 시계 기준입니다. 6단계를 하며 세션 조회(`current()`, 새로 고친 페이지가 CSRF 토큰을 다시 받음)를 더했고, 쿠키 이름 설정을 코어에서 `HttpConfig`로 옮겼습니다.
  - 6단계: 착수 전에 열린 결정 3(`/health`·`/metrics`·`/docs`)을 결정했습니다(`04-open-questions.md`). HTTP 변환 세부는 `03-adapters-and-pipeline.md` §1에 적었습니다. 같은 시나리오를 `HttpApp::handle`(`TestClient`가 쓸 경로)과 루프백 소켓 위의 실제 서버에 돌려 상태·헤더·본문이 같음을 확인합니다(`Date`·`Content-Length`처럼 `hyper`가 붙이는 헤더 제외). 코어의 `rate_limited`·`request_in_progress`를 재시도 가능한 오류로 바로잡았습니다(공통 규약상 `Retry-After`가 필수인데 전송은 그 헤더로 재시도 가능 여부를 복원하므로). `hyper`는 포크하지 않고 고정했습니다(`02-architecture.md` §3 실행 기록).
  - 7단계: 문서 생성에 필요한 라우트 기술(`RouteShape`)과 가드 기술(`GuardShape`)을 `orbic-core`에 더했습니다(프로토콜 중립, `hyve-validate` 스키마). 그래서 `Valid<T>` 추출자는 `T: Schema`를 요구합니다(`#[model]`은 모두 충족). 완료 기준의 "실제 응답과 일치"는 생성한 문서로 실제 HTTP 응답(성공, 목록, `204`, `400`·`401`·`403`·`404`·`413`·`415`·`422`·`429`)의 본문을 그 상태 코드에 적힌 스키마로 검증하고 적힌 헤더가 오는지 확인하는 테스트로 봅니다. `utoipa`는 포크하지 않고 고정했습니다(`02-architecture.md` §3 실행 기록).
  - 8단계: §1 스케치의 `create_project`는 `user`를 `db.tx(|tx| async move { .. })` 안으로 옮긴 뒤 다시 써서 컴파일되지 않았습니다. 같은 뜻으로 태그를 `created.owner_id`로 무효화하도록 스케치를 고쳤습니다(2단계의 `Id::<Project>::new()`와 같은 종류의 정정). §2 스케치의 `db.insert(Project { .. }).await`가 함수의 결과이므로 `insert`는 행을 값으로 받아 그 행을 돌려줍니다(4단계 API 변경). 인프로세스 전송을 위해 `orbic-core`에 `Pipeline::dispatch_call`(타입이 있는 `#[server]` 호출을 직렬화 없이 파이프라인에 넣음)을 더했고, 전송이 취소된 호출을 끝내는 오류 코드 `cancelled`를 `hyve-api`에 더했습니다(`hyve/docs/02-architecture.md`). 스케치 테스트는 `crates/orbic-tests/tests/sketch.rs`에 문서 그대로(`.room`·`.every` 줄 제외) 있고, 같은 파일에서 단일 파이프라인(`TestClient`와 실제 서버의 응답 비교), 두 레일과 인프로세스(같은 결과·같은 오류·스키마 불일치 폴백), HTTP 규약(커서 페이지 순회, 낙관적 잠금, 처리 중 키의 `409`와 `Retry-After`, 오류 메시지에 내부 이름 없음)을 확인합니다.

- 로컬 개발 서버(2026-11-25 구현, 2026-11-26부터 `hyve-orbic dev`): 서버를 선언한 패키지(`[package.metadata.orbic.server]`)를 빌드·실행하고 소스 변경 시 재기동합니다. 실제 작은 워크스페이스로 `stages`의 `dev` 노출 조건, 시작·재기동(이전 프로세스 종료 확인)·실행 파일이 그대로면 재기동하지 않음·서버의 자체 종료 알림·표준 입력이 닫히면 서버와 함께 멈춤을 확인합니다(`apps/orbic-cli/tests/dev.rs`). 로컬 DB 연결과 `codegen` 자동 실행(2026-11-25, `01-dx-and-dsl.md` §3): `hyve.toml`의 미끼 `database_url`을 이기고 SQLite 로컬 DB에 마이그레이션 적용, 새 마이그레이션에 모든 서버 재기동과 데이터 유지, 새 `sql!`이 빌드 전에 캐시됨, 환경에 이미 있는 `HYPERISH_ORBIC_DATABASE_URL`은 그대로 넘기고 아무것도 만들지 않음, PostgreSQL 방언은 테스트 서버에 워크스페이스 전용 DB를 만들고 다음 개발 세션에서 다시 사용함을 같은 파일이 확인합니다. 캐시는 Phase 1에서 연결할 하네스가 없습니다(L2와 함께 Phase 2). Windows는 **미검증**입니다.

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

### 배포 경계 검증 (2026-11-26)

서버 피처로 `cargo test -p orbic-tests --features server --test db --test http`를 실행했습니다. 전용 임시 PostgreSQL과 SQLite에서 DB 테스트 9건, 실제 루프백 연결과 요청 파이프라인을 사용하는 HTTP 테스트 11건이 통과했습니다. 여기에는 트랜잭션 롤백, 신뢰 프록시의 HTTPS 판정, 연결 종료 시 요청 취소, 두 레일의 응답이 포함됩니다. [DB 테스트](../crates/orbic-tests/tests/db.rs) · [HTTP 테스트](../crates/orbic-tests/tests/http.rs)

Phase 2 인수에서는 프록시의 HTTP/2 클라이언트와 HTTP/1.1 오리진 간 SSR 첫 청크·취소, L2를 공유하는 두 서버의 세션 철회를 확인합니다. 파일 저장 어댑터는 임시 디렉터리와 로컬 S3 호환 서버에서 업로드 재개·중단, 무결성 검증 실패 시 비공개 유지, 완료 후 재조회를 검사합니다. [HTTP 오리진](03-adapters-and-pipeline.md#http-origin) · [L2 하네스](01-dx-and-dsl.md#dev-cache) · [파일 저장 계약](03-adapters-and-pipeline.md#deployment-storage)

### 프리릴리스 기록

- **`0.1.0-alpha.0` (2026-11-25)**: 태그 `v0.1.0-alpha.0`은 `8225d7a`입니다. `hyve` `0.1.0-alpha.0`이 배포된 뒤 패치 없이 레지스트리 기준으로 `cargo publish --workspace --dry-run`을 리허설했고, 공개 크레이트 10개를 의존 순서대로 crates.io에 배포했습니다. **확정의 실행 방식을 바꿈**: 사용자 결정으로 위 기준을 모두 채우기 전에 배포했습니다. **포크 검증**은 미완입니다. `quick_cache`·`sqlx`·`hyper`·`utoipa`는 포크 없이 고정 의존이며(`02-architecture.md` §3 실행 기록), 포크본이 없으므로 이 기준은 충족으로 보지 않습니다. PR을 거치지 않았기 때문에 `main`이 아닌 `feat/phase1`의 커밋에 태그했습니다. 레지스트리에서 받아 확인한 것은 둘입니다. `cargo install orbic-cli --version =0.1.0-alpha.0`이 설치되어 `hyve-orbic stages`가 `codegen`을 알립니다. 또 경로 없이 `hyve`·`orbic` `=0.1.0-alpha.0`에만 의존하는 새 크레이트(`hyve.toml`과 `migrations/` 포함)에서 `#[table]` 모델, `Db::in_memory()` 마이그레이션, `Valid<T>` 거부(`400`), `TestClient` 생성·목록 테스트가 통과합니다.
- 이후 변경은 `0.1.0-alpha.1`로 올렸습니다(`9e99a83`).

2026-11-26에 SQLite 1,024행 × 10열을 여덟 번 조회하는 release 빌드를 공개 alpha.1과 번갈아 21회 측정했습니다. 전체 조회와 결과 확인을 포함한 행당 중앙값은 로컬 986 ns(930~1,077), alpha.1 1,287 ns(1,222~1,367)였습니다. SQLite와 PostgreSQL에서 결과 형식과 오류를 검사합니다. [재현 스크립트](../scripts/bench-db-rows.py) · [행 디코더](../crates/orbic-db/src/driver.rs)

64개 경로에 네 메서드와 기존 GET 교체를 포함한 OpenAPI 경로 표를 생성해 원본과 값이 같은지 검사했습니다. 로컬 release 빌드의 21회 교대 측정에서 중앙값은 62 µs(61~75), utoipa 범용 경로는 93 µs(92~105)였습니다. [동등성 테스트와 벤치마크](../crates/orbic-openapi/src/paths/mod.rs)

Hyper 0.14.32의 본문 수집 경로를 1.12.0 프레임에 맞춰 편입했습니다. HTTP가 이미 해석한 길이와 실제 수신 바이트에 제한을 적용합니다. 단일 4 KiB 프레임의 release 측정 중앙값은 20 ns(20~24), 기존 `Limited::collect().to_bytes()`는 26 ns(26~30)였습니다. 빈 프레임·트레일러·전송 중단·분할된 본문과 크기 초과도 검사합니다. [구현과 소스 고지](../crates/orbic-adapter-http/src/body/mod.rs) · [측정과 경계 테스트](../crates/orbic-adapter-http/src/body/tests.rs)

Valkey L2의 고정 형식 응답에는 fred 10.1.0의 튜플·바이트 변환을 편입했습니다. 상태 코드를 먼저 읽어 필요한 필드만 변환하고, 스키마와 아카이브에는 RESP 버퍼를 재사용합니다. 세 응답 종류를 섞어 21회 비교한 release 빌드의 처리당 중앙값은 69 ns(67~77), 범용 튜플 경로는 78 ns(75~93)였습니다. 서로 다른 복제본의 동시 적재·태그 무효화·연결 복구는 실제 Valkey에서 확인했습니다. [응답 처리와 비교 테스트](../crates/orbic-cache/src/redis/reply/mod.rs) · [복제본 통합 테스트](../crates/orbic-cache/tests/redis.rs)

quick_cache 0.7.0의 샤드 선택을 요청 중복 방지·요청량 제한의 비퇴거 저장소에 편입했습니다. 단일 키의 갱신은 원자적이고 서로 다른 키는 독립적으로 만료됩니다. 8개 스레드가 1,024개 키를 65,536회 갱신할 때 21회 중앙값은 갱신당 75 ns(71~81), 기존 단일 뮤텍스는 140 ns(73~155)였습니다. 만료 후 뒤늦은 완료와 진행 중인 윈도의 제한값 변경도 검사합니다. [샤드 선택과 측정](../crates/orbic-cache/src/ttl.rs)
