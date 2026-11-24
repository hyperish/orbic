# 01. DX 및 컴포넌트 설계 (orbic)

> 상태: Rust 스타일 컴포넌트 조립 아키텍처·`loco.rs` 도메인 생산성·영역별 표현 방식 고정(조립은 순수 Rust 체이닝)은 **확정**, 세부 API 시그니처는 **제안** · 기준일: 2026-11-24

`orbic`의 DX 목표는 **`loco.rs`처럼 백엔드에 필요한 모든 부품(ORM/Query, Cache, Auth, Middleware, 워커, 실시간 룸, Swagger, TestClient)이 처음부터 준비되어 있으면서**, 고정된 폴더 구조를 강요하지 않고 **깔끔한 Rust 스타일 컴포넌트(`BackendComponent`)로 개발자가 원하는 아키텍처를 직접 조립**하게 하는 것입니다.

## 1. Rust 스타일 백엔드 컴포넌트 아키텍처 (확정)

- **개념을 묶는 단위로서의 Component**: 라우트, 의존성 상태(`provide`), 미들웨어/권한 가드(`wrap`), 실시간 룸(`room`), 백그라운드 워커를 도메인별 `BackendComponent`로 묶습니다.
- **깔끔한 독립 Rust 핸들러 함수**: JS 프레임워크식의 깊은 인라인 클로저 중첩 대신, 가독성 높은 일반 `async fn` 핸들러와 타입 추출자(`Ctx<Db>`, `Ctx<CurrentUser>`, `Valid<Input>`)로 비즈니스 로직을 깔끔하게 분리합니다.
- **검증(Validation) · Swagger · TestClient 자동 연동**:
  - 입력 타입에 `Valid<T>`(`hyve-validate`)를 쓰면 핸들러 진입 전에 자동 검증되고 실패 시 `400 invalid_request`가 반환됩니다.
  - 컴포넌트에 등록된 핸들러와 `Valid<T>` 스키마는 자동으로 OpenAPI 트리에 수집되어 `.openapi("/docs")`로 서빙됩니다.

### 조립 계약 (제안, 2026-11-24)

- **`provide`의 범위**: 컴포넌트가 제공한 값은 그 컴포넌트와 그 아래에 `mount`된 컴포넌트에서만 보이고, 가장 가까운 제공이 우선합니다. 형제 컴포넌트는 서로의 제공을 보지 못합니다.
- **조립 시점 검증**: 주입은 `OrbicApp`을 조립할 때 해석합니다. 핸들러가 요구하는 `Ctx<T>`의 제공이 없거나 같은 메서드·경로가 두 번 등록되면 조립이 실패합니다. 요청 시점의 `500`으로 미루지 않습니다.
- **`wrap`의 순서**: 가드는 선언한 순서대로 바깥에서 안쪽으로 실행하며(먼저 `wrap`한 가드가 먼저 실행), `mount`가 여러 겹이어도 한 요청에서 각 가드는 정확히 한 번 실행됩니다.
- **추출자**: 핸들러 인자는 `Ctx<T>`(주입), `Valid<T>`(본문), `Query<T>`(쿼리), `Path<T>`(경로 파라미터)입니다. 반환한 `OrbicResult<T>`는 `hyperish/docs` `03-api-protocol.md` §4의 봉투로 변환됩니다.
- **쿼리와 경로 파라미터 (제안, 2026-11-24)**: `Query<T>`와 `Path<T>`는 `T`가 모델이면 필드를 JSON 이름(`camelCase`)의 파라미터로 받고, 목록 필드는 파라미터를 반복합니다(`?tag=a&tag=b`). 모델이 아닌 값 하나(`Path<Id<Project>>`)는 그 자리의 파라미터 하나를 읽습니다. 값은 필드의 스키마에 따라 정수·실수·`true`/`false`·문자열로 읽고, 읽을 수 없으면 파라미터 이름이 담긴 `400 invalid_request`입니다. 파라미터에는 중첩 모델을 담지 않습니다.
- **리터럴**: `ttl!`, `key!`, `tag!`의 리터럴은 컴파일 타임에, `.every("1h")` 같은 조립 인자는 조립 시점에 검증합니다. 해석할 수 없는 값을 기본값으로 대체하지 않습니다. `key!`와 `tag!`는 구성 요소가 구분자와 충돌하지 않게 인코딩합니다(제안, 2026-11-24: 구성 요소를 `:`로 잇고, 구성 요소 안의 `\`와 `:`는 `\\`와 `\:`로, 값이 없는 `Option`은 이스케이프 결과에 나올 수 없는 `\-`로 씁니다).
- **OpenAPI**: 생성되는 문서는 실제 와이어 형식을 그대로 기술합니다. 응답 봉투, 오류 응답, 인증 요구가 포함됩니다.
  - **구현 세부 (제안, 2026-11-24)**: `.openapi(path)`는 `orbic-openapi`의 확장 트레이트(prelude에 포함)이고, 문서 제목과 버전은 앱 루트에 `provide`하는 `OpenApiInfo`로 정합니다(기본 `API`, `0.0.0`). 라우트가 무엇을 읽고 답하는지(경로·쿼리 파라미터, 본문, 응답 모델)와 가드가 더하는 것(인증 여부, 낼 수 있는 오류 코드)은 `orbic-core`가 `hyve-validate` 스키마로 기술하고, `orbic-openapi`가 HTTP 어댑터의 규칙으로 옮깁니다. 성공은 `{"data": ...}`(목록 `Page<T>`는 `data`와 `page`), 값 없는 핸들러는 `204`입니다. 오류는 라우트가 낼 수 있는 프레임워크·가드·게이트 코드를 상태 코드별로 적고(`Retry-After`·`RateLimit-*` 헤더 포함), 핸들러 자신의 `Domain` 오류는 `default` 응답입니다. 인증하는 가드 아래의 라우트에는 쿠키 세션(`session`)과 Bearer(`bearer`) 보안 요구와 상태를 바꾸는 메서드의 `X-CSRF-Token` 헤더를, `POST`에는 `Idempotency-Key` 헤더를 적습니다. 스키마는 JSON 코덱 규칙을 따릅니다(64비트 정수는 10진 문자열일 수 있고, 무한대·NaN은 문자열). `operationId`는 핸들러 경로 이름(`crate.module.fn`)입니다.
- **심볼의 소유**: `Id<T>`, `HyveResult<T>`, `Ctx<T>`, `Valid<T>`, `Path<T>`, `Query<T>`, `Page<T>`, `CursorParams`는 `hyve`의 것이고 `orbic`은 재수출만 합니다. `Page<T>`와 `CursorParams`는 `hyperish/docs` `03-api-protocol.md` §4·§5의 목록 응답과 커서 요청에 대응하며, 공유 크레이트의 `#[server]` 함수도 쓸 수 있습니다. `OrbicResult<T>`는 `HyveResult<T>`의 별칭이라 계층 사이에 오류 변환이 없습니다. `CurrentUser`와 `User`는 앱이 정의하는 모델입니다. 앱이 인증 주체 타입(`CurrentUser`)을 `orbic-auth`에 등록하면 `orbic-auth`가 세션에 저장했다가 요청 스코프에 주입하고, `.as_user(...)`도 그 타입의 값을 받습니다.
- **공유 모델의 `#[table]`**: `#[table]`과 `#[pk]`는 `orbic-macros`의 속성이며 테이블 메타데이터만 만듭니다. 생성물은 `#[server]`와 같은 기준으로 `server` 피처가 켜진 빌드에만 남습니다(§2). 공유 크레이트는 진입 크레이트 `orbic`을 기본 피처를 끈 채 의존합니다. 이 상태의 `orbic`은 매크로와 `OrbicResult`만 내보내고(테이블 메타데이터는 `server` 빌드에만 생기므로 그 타입도 런타임 쪽에 있습니다) `wasm32`에서도 빌드되며, 런타임(DB, HTTP, `tokio`)은 `orbic`의 `server` 피처에 있습니다. 그래서 클라이언트 빌드는 DB 코드를 링크하지 않습니다.
- **`RateLimit`의 키**: 인증 주체가 있으면 그 주체, 없으면 클라이언트 주소를 키로 하고, 범위는 `wrap`한 컴포넌트입니다. `AuthGuard`보다 먼저 `wrap`하면 인증 전이므로 주소 기준으로만 셉니다.
- **인증 엔드포인트**: `orbic-auth`는 세션 발급·조회·회전·철회와 `AuthGuard`를 제공하고, 로그인·로그아웃 라우트는 앱이 정의해 이를 호출합니다. Phase 1은 라우트를 내장하지 않습니다. 세션 쿠키 이름은 `orbic-auth` 설정으로 정하며 기본값은 구현이 문서화합니다.
  - **구현 세부 (제안, 2026-11-24)**: 앱은 인증 주체 타입을 한 번 등록합니다: `impl SessionUser for CurrentUser`(인증 주체의 안정된 식별자를 돌려줌, 토큰이 아님)와 `type AuthGuard = orbic::AuthGuard<CurrentUser>;`. 그래서 스케치의 `AuthGuard::require_user()`가 그대로 `Ctx<CurrentUser>`를 제공합니다(조립 시점 검사 대상). `AuthGuard::require(|u| ...)`는 규칙이 거짓이면 `403 forbidden`입니다. 핸들러는 `orbic_auth::sign_in(&user, Client::Web | Client::Bearer)`, `sign_out()`, `refresh::<U>(token)`, `current()`(세션 조회: 웹은 CSRF 토큰을 다시 받음, 토큰은 돌려주지 않음)를 부릅니다. 웹은 세션 토큰을 쿠키로(어댑터가 설정), CSRF 토큰을 본문으로 받고, Bearer는 액세스·리프레시 토큰을 본문으로 받습니다. 토큰은 OS CSPRNG의 256비트를 base64url(43자)로 씁니다. 세션 토큰과 리프레시 토큰은 SHA-256으로만 저장소 키가 되고 원문은 저장하지 않습니다. CSRF 토큰은 세션 기록 안에 원문으로 두며, 세션 토큰 없이는 쓸 수 없습니다. 세션은 30분 유휴 시 만료(요청마다 연장)되고 로그인 7일 뒤 무조건 만료되며, 리프레시 토큰은 30일이고 한 번만 쓰입니다(`AuthConfig`를 `provide`해 바꿈). 쓴 리프레시 토큰을 다시 내면 그 계열(세션과 리프레시 토큰) 전체를 폐기합니다. 쿠키 이름은 어댑터가 읽고 쓰는 HTTP 항목이라(코어 무오염 원칙, `02-architecture.md` §2) `orbic-auth`가 아니라 앱 루트에 `provide`하는 `orbic-adapter-http`의 `HttpConfig::session_cookie(name)`로 정하고 기본값은 `hyperish_session`입니다(위 문장의 "orbic-auth 설정"을 대신함). 세션 변경은 핸들러가 성공했을 때만 응답 메타데이터(`credential_update`)에 실립니다.
- **낙관적 잠금**: Phase 1은 전용 API를 두지 않습니다. 요청 모델의 `expectedVersion`과 체이닝 쿼리의 조건부 갱신으로 구현하고, 갱신된 행이 없으면 핸들러가 `Domain` 오류 `version_conflict`를 돌려줍니다.
- **`TestClient`의 `.as_user()`**: 가드를 건너뛰지 않습니다. 그 사용자의 유효한 세션을 저장소에 만들고 요청에 자격 증명을 붙여, 실제 요청과 같은 경로로 인증됩니다.

### 백엔드 컴포넌트 · DB 투트랙 쿼리 · 캐시 · 인메모리 TestClient 스케치 (제안)

```rust
use hyve::prelude::*;
use orbic::prelude::*;

// 1) hyve 모델 + DB 테이블 직결 (별도 ORM 엔티티 보일러플레이트 없음)
#[model]
#[table("projects")]
pub struct Project {
    #[pk] pub id: Id<Project>,
    pub owner_id: Id<User>,
    #[validate(len = 1..=80)]
    pub title: String,
    pub is_public: bool,
    pub version: i64,
}

#[model]
pub struct CreateProjectInput {
    #[validate(len = 1..=80)]
    pub title: String,
    pub is_public: bool,
}

// 2) 도메인 컴포넌트: 상태·가드·라우트·실시간 룸·Cron 워커를 한 단위로 캡슐화
pub fn project_component() -> BackendComponent {
    component()
        .wrap(AuthGuard::require_user())
        .wrap(RateLimit::per_minute(60))
        .post("/projects", create_project)
        .get("/projects", list_projects)
        .room("/projects/{id}/live", project_live_room)
        // 분산 리더 락으로 멀티 레플리카 중복 실행을 방지하며 TestClient에서는 자동 비활성화됨
        .every("1h", cleanup_orphan_uploads)
}

// 3) DB 체이닝 + 컴파일 타임 sql! 매크로 + L1/L2 Singleflight 캐시 활용
async fn list_projects(
    db: Ctx<Db>,
    cache: Ctx<Cache>,
    user: Ctx<CurrentUser>,
    cursor: Query<CursorParams>,
) -> OrbicResult<Page<Project>> {
    // Singleflight: 동시 캐시 미스 시 DB 쿼리는 단 1회만 실행하고 태그를 함께 등록
    cache.get_or_try_init(
        key!("projects", user.id, cursor.after),
        CacheOpts::ttl(ttl!("30s")).tag(tag!("projects", user.id)),
        || async {
            db.query::<Project>()
                .where_eq(Project::OWNER_ID, user.id)
                .cursor_after(cursor.after.clone(), 50)
                .fetch_page()
                .await
        },
    ).await
}

async fn create_project(
    db: Ctx<Db>,
    cache: Ctx<Cache>,
    user: Ctx<CurrentUser>,
    input: Valid<CreateProjectInput>,
) -> OrbicResult<Project> {
    let created = db.tx(|tx| async move {
        let data = input.into_inner();
        // 컴파일 타임 sql!은 레포에 커밋된 오프라인 메타데이터 캐시로 검증됨 (§4)
        // (운영 방언 전용 쿼리는 테스트 시 임시 Postgres 하네스 사용, SQLite에서도 검증된 쿼리와 체이닝 쿼리는 Db::in_memory() 호환)
        sql!(
            Project,
            "INSERT INTO projects (id, owner_id, title, is_public, version)
             VALUES ($1, $2, $3, $4, 1) RETURNING *",
            Id::<Project>::new(), user.id, data.title, data.is_public
        )
        .fetch_one(tx)
        .await
    }).await?;

    cache.invalidate_tag(tag!("projects", user.id)).await?;
    Ok(created)
}

// 4) 루트 앱 조립 및 포트 없는 인메모리 TestClient 검증
pub fn app(db: Db, cache: Cache) -> OrbicApp {
    OrbicApp::new()
        .provide(db)
        .provide(cache)
        .openapi("/docs")
        .mount("/v1", project_component())
}

#[hyve::test]
async fn create_project_rejects_empty_title() {
    let client = TestClient::new(app(Db::in_memory().await, Cache::in_memory()));

    let res = client
        .post("/v1/projects")
        .as_user(test_user())
        .json(&CreateProjectInput { title: "".into(), is_public: false })
        .send()
        .await;

    assert_eq!(res.status(), 400);
}
```

## 2. 인라인 서버 함수(`#[server]`) ↔ `BackendComponent` 결합 계약 (확정, 2026-11-19)

`serge` 프론트엔드에서 백엔드 핸들러를 일반 Rust 비동기 함수처럼 호출할 때도 **숨겨진 전역 자동 등록을 금지**합니다.

- **서버 빌드(`feature = "server"`)**: `#[server(POST "/projects")]`는 핸들러 본문과 계약 디스크립터를 남겨 `orbic::component().wrap(AuthGuard::require_user()).handle(create_project)`로 **명시 등록**합니다(권한 가드·레이트리밋·`/v1` 마운트·OpenAPI·`TestClient` 100% 일관 적용).
- **클라이언트 빌드(`not(feature = "server")`)**: `#[server_ctx]` 추출자(`Ctx<Db>`, `Ctx<CurrentUser>`)와 함수 본문을 컴파일 타임에 완전히 제거하고 `serge-adapter-client`(Dual-Rail + 뷰 언마운트 시 `CancellationToken` 전파) 호출 스텁만 남깁니다.
- **스텁은 어느 빌드에나 있습니다 (확정, 2026-11-24)**: 함수 이름으로 부르는 것은 서버 빌드에서도 스텁입니다. 같은 프로세스 안의 호출은 `orbic`의 인프로세스 전송이 받아 같은 요청 파이프라인(가드 포함)에 넣습니다. 포트 없는 풀스택 테스트와 `TestClient`의 두 레일 검증이 이 경로를 씁니다(`hyve/docs/03-adapters-and-pipeline.md` §3).
- **경로와 마운트 (제안, 2026-11-24)**: `#[server]`의 경로는 `.handle(...)`로 등록한 컴포넌트의 마운트 접두어 아래에 놓입니다. 클라이언트는 접두어를 기준 URL 설정으로 받습니다. `#[server]` 함수를 서로 다른 접두어 아래에 등록하려면 전송을 접두어별로 제공해야 하므로, Phase 1은 한 앱의 `#[server]` 함수를 한 접두어 아래에 둡니다.
- **기준은 타깃이 아니라 피처**: 제거 여부는 함수가 선언된 공유 크레이트의 `server` 피처로만 갈립니다. 서버 바이너리와, 서버와 UI를 한 프로세스에 두는 빌드(포트 없는 풀스택 테스트, 단일 바이너리)가 이 피처를 켭니다. 배포하는 웹(WASM)과 네이티브 클라이언트는 끈 상태로 빌드해 서버 본문과 DB 의존성을 링크하지 않습니다.
- **소유와 해석**: `#[server]`/`#[server_ctx]` 매크로는 `hyve-macros`가 소유합니다. `POST "/projects"` 인자는 `hyve`가 해석하지 않는 바인딩 메타데이터이고, 이를 HTTP 라우트와 OpenAPI로 해석하는 것은 `orbic`입니다(`hyve/docs/02-architecture.md` §2.4).
- **인자와 오류 (확정, 2026-11-24)**: 스텁이 보내는 요청 모델과 오류 복원 규칙은 `hyve/docs/02-architecture.md` §2.4를 따릅니다. `orbic`은 같은 요청 모델을 고속 레일(바이너리)과 표준 레일(JSON) 양쪽에서 받습니다. 전송 인자가 1개면 그 모델이 곧 요청 본문이라 `.post()`로 등록한 일반 핸들러·OpenAPI·`TestClient`와 형태가 같습니다. 표준 레일의 오류는 `hyperish/docs` `03-api-protocol.md` §4의 봉투(`requestId` 포함)를 그대로 쓰며 필드를 추가하지 않습니다.
- **본문 없는 메서드 (제안, 2026-11-24)**: `GET`이나 `DELETE`로 바인딩하면 표준 레일에서는 요청 모델의 필드를 쿼리 파라미터로 받고, 경로의 `{이름}` 자리표시자와 이름이 같은 필드는 경로에서 채웁니다. 전송 인자가 모델이 아닌 값 하나면 필드 이름 대신 그 인자의 이름을 씁니다(`GET "/projects/{id}"`에 `id: Id<Project>`). 고속 레일은 바인딩된 메서드와 무관하게 `POST` 본문에 요청 모델을 그대로 싣습니다(`hyve/docs/03-adapters-and-pipeline.md` §3.3).

```rust
// shared/projects.rs — 단일 모델 + 인라인 서버 함수
#[model]
pub struct CreateProject { #[validate(len = 1..=80)] pub title: String }

#[server(POST "/projects")]
pub async fn create_project_rpc(
    #[server_ctx] db: Ctx<Db>,
    #[server_ctx] user: Ctx<CurrentUser>,
    input: Valid<CreateProject>,
) -> HyveResult<Project> {
    db.insert(Project { id: Id::new(), owner_id: user.id, title: input.into_inner().title, is_public: false, version: 1 }).await
}

// server — 전역 매직 없이 BackendComponent에 명시 등록
pub fn project_rpc_component() -> BackendComponent {
    orbic::component()
        .wrap(AuthGuard::require_user())
        .handle(create_project_rpc)
}
```

## 3. `hyve` CLI와 통합된 백엔드 개발 흐름

`loco.rs`가 제공하는 제너레이터·마이그레이션·워커 실행 경험을 `hyve` CLI 서브커맨드/워크플로 안으로 통합합니다:

- **스캐폴딩**: `hyve init --template orbic-service` 또는 리소스·마이그레이션·워커 생성.
- **로컬 개발·프리뷰 (`hyve preview`)**: 코드 변경 시 서버 재기동 및 로컬 DB/캐시 테스트 하네스 자동 연결.
- **정적 검증 (`hyve lint`)**: 누락된 권한 검사(`02-core-contracts.md` §6), 취소 토큰 미전파(`§3`), 어댑터 타입의 코어 유입(`§5`)을 정적 검사로 차단.
- **계약·통합 테스트 (`hyve test`)**: 인메모리 DB/캐시 하네스를 사용해 HTTP/JSON(`03-api-protocol.md`) 및 WS/WT(`04-realtime-communication.md`)의 멱등성·낙관적 잠금·재연결 시나리오를 자동 검증.

## 4. `sql!` 오프라인 검증과 방언 (확정, 2026-11-24)

`sql!`은 레포에 커밋한 **오프라인 메타데이터 캐시**로 검증합니다. 마이그레이션을 매크로 안에서 직접 해석하는 방식은 쓰지 않습니다. `sql!`이 맡는 복잡한 조인·집계일수록 DB 엔진의 분석이 가장 정확하고, 매크로에 두 방언의 타입 추론을 다시 구현하지 않아도 되기 때문입니다.

- **생성**: `hyve codegen` 단계(`hyve build`·`hyve preview`에 포함)가 마이그레이션을 적용한 임시 DB에서 각 `sql!`을 분석시키고, 파라미터와 결과 컬럼의 타입·널 가능성을 쿼리별로 저장합니다. 캐시는 버전 관리에 포함합니다.
- **컴파일**: 매크로는 캐시만 읽어 인자 타입과 결과 모델을 대조하며, 컴파일 중에는 어떤 경우에도 DB에 접속하지 않습니다. `sqlx`의 매크로가 `DATABASE_URL`이 있으면 DB 접속을 우선하는 것과 다른 점입니다(`sqlx-cli` README v0.9.0, 2026-11-24 확인: https://github.com/transact-rs/sqlx/blob/v0.9.0/sqlx-cli/README.md). 캐시에 없는 쿼리는 `hyve codegen`을 실행하라는 안내가 담긴 컴파일 오류가 됩니다.
- **필요한 DB**: SQLite 분석은 인프로세스로 끝납니다. PostgreSQL 인스턴스는 캐시를 만들거나 점검하는 작업에만 필요하고, 캐시가 커밋되어 있으면 IDE와 CI의 빌드·테스트 작업은 DB 없이 통과합니다.
- **낡은 캐시**: CI에서 `hyve codegen`의 점검으로 캐시가 현재 마이그레이션·쿼리와 일치하는지 확인합니다. 컴파일 타임에 스키마 지문을 비교해 낡은 캐시를 막는 방식은 **제안**입니다.
- **방언**: `Db::in_memory()`는 SQLite 인메모리 DB입니다. 체이닝 쿼리는 지원 방언 전체에 이식됩니다. `sql!`은 프로젝트의 운영 방언에서 반드시 검증되고, SQLite에서도 검증을 시도해 통과한 방언을 캐시에 기록합니다. 검증되지 않은 방언의 DB에서 `sql!`을 실행하면 쿼리를 엔진에 보내지 않고 명시적 오류를 돌려줍니다. 운영 방언 전용 쿼리의 테스트는 임시 PostgreSQL 하네스에서 돌립니다. 이 구분에 새 `sql!` 문법은 필요하지 않습니다.
- **설정과 위치 (제안)**: 운영 방언은 `hyve.toml`의 `[orbic] dialect`(`postgres` 또는 `sqlite`)에 선언합니다. 캐시는 워크스페이스 루트의 `.orbic/sql/`에 쿼리별 파일로 두며, 파일 형식은 `orbic` 내부 규약입니다. 마이그레이션은 워크스페이스 루트의 `migrations/`에 둔 `.sql` 파일을 이름순으로 적용합니다. (제안, 2026-11-24) 워크스페이스 루트는 `hyve.toml`이 있는 가장 가까운 상위 디렉터리입니다. `0002_x.postgres.sql`·`0002_x.sqlite.sql`처럼 방언 접미어가 붙은 파일은 그 방언에만 적용해 두 엔진의 DDL 차이(`BYTEA`/`BLOB` 등)를 담습니다. 적용한 마이그레이션은 `_orbic_migrations`에 체크섬과 함께 기록하고, 적용 뒤 내용이 바뀐 파일은 오류입니다. `Db::in_memory()`는 작업 디렉터리에서 찾은 워크스페이스의 마이그레이션을 적용합니다.
- **값과 오류 (제안, 2026-11-24)**: SQLite에서 `Timestamp`는 정렬과 비교가 맞도록 Unix 마이크로초 정수로 저장하고, PostgreSQL에서는 `timestamptz`입니다. 엔진 타입 대응은 SQLite `BOOLEAN`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`DATETIME`, PostgreSQL `BOOL`/`INT2·4·8`/`FLOAT4·8`/`TEXT`·`VARCHAR`·`BPCHAR`/`BYTEA`/`TIMESTAMPTZ`·`TIMESTAMP`이며, 그 밖의 타입은 `null`(미확인)입니다. 드라이버 오류는 연결·풀 장애가 재시도 가능한 `Infra` `service_unavailable`, 유일성 위반이 `Domain` `already_exists`, 외래 키·검사 제약 위반이 `Domain` `constraint_violated`, 나머지는 `Internal`입니다. 행이 없는 `fetch_one`은 `<테이블>_not_found`입니다.
- **캐시 파일과 매크로 (제안, 2026-11-24)**: 쿼리 하나는 `.orbic/sql/<쿼리 원문 FNV-1a 해시 16자리>.json` 파일 하나이며, 원문·파라미터 타입·결과 컬럼(이름, 타입, 널 가능성)·검증된 방언을 담습니다. 형식은 `orbic-sql-cache` 크레이트에 있고 매크로와 `hyve-orbic`이 함께 씁니다. 타입은 `bool`·`int`·`float`·`text`·`bytes`·`timestamp`이고, 검증한 엔진이 알려주지 않으면 `null`입니다(SQLite는 파라미터 타입을, PostgreSQL은 대부분의 식 컬럼의 널 가능성을 알려주지 않음 — 2026-11-24 `sqlx` 0.9.1 `describe`로 확인). 매크로는 인자를 캐시가 기록한 파라미터 타입으로 묶고(맞지 않는 Rust 타입은 컴파일 오류), 타입이 `null`이면 인자의 Rust 타입이 정하는 타입으로 묶습니다(Rust 타입마다 하나). 모델이 있으면 결과 컬럼이 모델 필드를 빠짐없이 덮는지, 타입이 같은지, 널 가능한 컬럼이 `Option` 필드인지를 상수 평가로 확인하며, `null`인 사실은 컴파일 때 확인하지 않고 행을 디코딩할 때 확인합니다(어긋나면 컬럼 이름이 담긴 `Internal` 오류). 결과 컬럼이 있는 쿼리에 모델을 적지 않으면 오류입니다. 캐시 파일이 바뀌면 그 파일을 쓰는 크레이트가 다시 컴파일됩니다.
- **실행 주체 (제안)**: 캐시 생성과 점검은 `orbic`이 제공하는 `hyve-orbic` 실행 파일의 `codegen` 단계이고, `hyve codegen`이 이를 호출합니다(`hyve/docs/01-dx-and-dsl.md` §2). 쿼리 목록은 빌드를 구동하지 않고 워크스페이스의 `.rs` 소스를 토큰 단위로 훑어 얻습니다(2026-11-24 변경: 빌드를 구동하면 증분 빌드가 매크로 확장을 다시 하지 않아 목록이 빠지고, 매번 새 대상 디렉터리로 빌드하면 느림). `macro_rules!` 본문 안의 `sql!`도 찾고, 문서 주석과 문자열 안의 것은 무시합니다. `[orbic] codegen_exclude`(워크스페이스 루트 기준 경로 목록)에 든 경로는 훑지 않습니다(컴파일 실패 테스트용). `hyve-orbic codegen`은 캐시를 쓰고, `--check`는 쓰지 않고 빠지거나 다른 파일이 있으면 실패하며, `--prune`은 어떤 `sql!`도 쓰지 않는 파일을 지웁니다. 운영 방언에서 실패한 쿼리는 위치(`파일:줄`)와 엔진의 메시지로 보고되고 단계가 실패합니다. 운영 방언이 PostgreSQL이면 SQLite에서도 분석해 두 엔진이 파라미터 수·컬럼 이름과 순서·알려준 타입에서 합의할 때만 `sqlite`를 검증된 방언으로 기록합니다(한쪽이 모르는 타입은 다른 쪽 값을 쓰고, 널 가능성은 어느 쪽이든 널 가능하면 널 가능, 어느 쪽이든 모르면 모름). 임시 PostgreSQL의 접속 정보는 환경 변수 `HYPERISH_TEST_POSTGRES_URL`로 받고, 하네스는 그 서버에 실행마다 격리된 임시 데이터베이스를 만들었다가 지웁니다. 변수가 없으면 PostgreSQL이 필요한 단계와 테스트는 건너뛰지 않고 실패합니다.
