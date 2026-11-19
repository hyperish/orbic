# 01. DX 및 컴포넌트 설계 (orbic)

> 상태: Rust 스타일 컴포넌트 조립 아키텍처·`loco.rs` 도메인 생산성·영역별 표현 방식 고정(조립은 순수 Rust 체이닝)은 **확정**, 세부 API 시그니처는 **제안** · 기준일: 2026-11-19

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
        // 컴파일 타임 sql!은 오프라인 스키마 메타데이터/마이그레이션 기준으로 검증됨
        // (Postgres 전용 방언 쿼리는 테스트 시 임시 Postgres 하네스 사용, 이식 가능한 체이닝 쿼리는 Db::in_memory() 호환)
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
- **클라이언트 빌드(`!feature = "server"`)**: `#[server_ctx]` 추출자(`Ctx<Db>`, `Ctx<CurrentUser>`)와 함수 본문을 컴파일 타임에 완전히 제거하고 `serge-adapter-client`(Dual-Rail + 뷰 언마운트 시 `CancellationToken` 전파) 호출 스텁만 남깁니다.

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
