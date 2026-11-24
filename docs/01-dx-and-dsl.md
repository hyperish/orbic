# 01. DX 및 컴포넌트 설계 (orbic)

> 상태: Rust 스타일 컴포넌트 조립 아키텍처·`loco.rs` 도메인 생산성·영역별 표현 방식 고정(조립은 순수 Rust 체이닝)은 **확정**, 세부 API 시그니처는 **제안** · 기준일: 2026-11-24

`orbic`의 DX 목표는 **`loco.rs`처럼 백엔드에 필요한 모든 부품(ORM/Query, Cache, Auth, Middleware, 워커, 실시간 룸, Swagger, TestClient)이 처음부터 준비되어 있으면서**, 고정된 폴더 구조를 강요하지 않고 **깔끔한 Rust 스타일 컴포넌트(`BackendComponent`)로 개발자가 원하는 아키텍처를 직접 조립**하게 하는 것입니다.

## 1. Rust 스타일 백엔드 컴포넌트 아키텍처 (확정)

- **개념을 묶는 단위로서의 Component**: 라우트, 의존성 상태(`provide`), 미들웨어/권한 가드(`wrap`), 실시간 룸(`room`), 백그라운드 워커를 도메인별 `BackendComponent`로 묶습니다.
- **깔끔한 독립 Rust 핸들러 함수**: JS 프레임워크식의 깊은 인라인 클로저 중첩 대신, 가독성 높은 일반 `async fn` 핸들러와 타입 추출자(`Ctx<Db>`, `Ctx<CurrentUser>`, `Valid<Input>`)로 비즈니스 로직을 깔끔하게 분리합니다.
- **검증(Validation) · Swagger · TestClient 자동 연동**:
  - 입력 타입에 `Valid<T>`(`hyve-validate`)를 쓰면 핸들러 진입 전에 자동 검증되고 실패 시 `400 invalid_request`가 반환됩니다.
  - 컴포넌트에 등록된 핸들러와 `Valid<T>` 스키마는 자동으로 OpenAPI 트리에 수집되어 `.openapi("/docs")`로 서빙됩니다.

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
            Id::new(), user.id, data.title, data.is_public
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

- **서버 빌드(`feature = "server"`)**: `#[server(POST "/projects")]`는 원본 핸들러와 라우트 디스크립터를 남겨 `orbic::component().wrap(AuthGuard::require_user()).handle(create_project)`로 **명시 등록**합니다(권한 가드·레이트리밋·`/v1` 마운트·OpenAPI·`TestClient` 100% 일관 적용).
- **클라이언트 빌드(`not(feature = "server")`)**: `#[server_ctx]` 추출자(`Ctx<Db>`, `Ctx<CurrentUser>`)와 함수 본문을 컴파일 타임에 완전히 제거하고 `serge-adapter-client`(Dual-Rail + 뷰 언마운트 시 `CancellationToken` 전파) 호출 스텁만 남깁니다.
- **기준은 타깃이 아니라 피처**: 제거 여부는 함수가 선언된 공유 크레이트의 `server` 피처로만 갈립니다. 서버 바이너리만 이 피처를 켜고, 웹(WASM)과 네이티브 클라이언트는 모두 끈 상태로 빌드해 서버 본문과 DB 의존성을 링크하지 않습니다.
- **소유와 해석**: `#[server]`/`#[server_ctx]` 매크로는 `hyve-macros`가 소유합니다. `POST "/projects"` 인자는 `hyve`가 해석하지 않는 바인딩 메타데이터이고, 이를 HTTP 라우트와 OpenAPI로 해석하는 것은 `orbic`입니다(`hyve/docs/02-architecture.md` §2.4).
- **인자와 오류 (확정, 2026-11-24)**: 스텁이 보내는 요청 모델과 오류 복원 규칙은 `hyve/docs/02-architecture.md` §2.4를 따릅니다. `orbic`은 같은 요청 모델을 고속 레일(바이너리)과 표준 레일(JSON) 양쪽에서 받습니다. 전송 인자가 1개면 그 모델이 곧 요청 본문이라 `.post()`로 등록한 일반 핸들러·OpenAPI·`TestClient`와 형태가 같습니다. 표준 레일의 오류는 `hyperish/docs` `03-api-protocol.md` §4의 봉투를 그대로 쓰며 필드를 추가하지 않습니다.

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
