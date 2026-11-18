# hyperish/ring

하이퍼리쉬(Hyperish) 프레임워크 패밀리의 **Rust 스타일 컴포넌트(Component) 조립식 백엔드 프레임워크** 레포입니다.

데이터베이스(DB/ORM 쿼리), 캐시(Cache), 미들웨어(Middleware), 인증·권한(Auth), 마이크로서비스(MSA), 실시간 통신(WebSocket / WebTransport), Swagger/OpenAPI 자동 서빙, 인메모리 `TestClient` 등 **백엔드 전반의 도메인**을 다룹니다.

```rust
use hyper::prelude::*;
use ring::prelude::*;

pub fn project_component() -> BackendComponent {
    component()
        .wrap(AuthGuard::require_user())
        .post("/projects", create_project)
        .room("/projects/{id}/live", project_live_room)
}
```

> 핸들러·루트 앱 조립·인메모리 `TestClient` 전체 예시는 [`docs/01-dx-and-dsl.md`](docs/01-dx-and-dsl.md)를 봅니다.

## 왜 `ring`인가?

| 기존 백엔드 스택의 한계 | `ring`의 해답 |
|---|---|
| **고정된 폴더 구조 강제 (Rails / Loco / NestJS)**: `controllers/`, `services/`, `models/` 구조를 강제해 프로젝트가 커지거나 아키텍처(DDD, Hexagonal, MSA)를 바꿀 때 발목을 잡음 | **아키텍처 고점을 여는 Rust 컴포넌트 조립**: 상태(`provide`), 미들웨어/권한(`wrap`), 라우트, WS/WT 룸, 워커를 `BackendComponent` 단위로 묶어 개발자가 원하는 아키텍처로 자유롭게 조립·분리 |
| **검증·문서·클라이언트 삼중 작업**: 요청 Validation 따로, OpenAPI(Swagger) 어노테이션 따로, 프론트/테스트 클라이언트 따로 작성해야 함 | **`hyper` 코어 직결로 3가지 자동 해결**: `Valid<T>`와 입출력 타입만 적으면 프론트/백엔드 공용 Validation + `/docs` Swagger UI + 포트 없이 즉시 도는 인메모리 `TestClient`가 한 번에 도출 |
| **파편화된 백엔드 인프라**: HTTP 서버 따로, ORM 따로, 캐시 따로, 실시간 룸·백그라운드 잡 따로 붙이느라 보일러플레이트 폭증 | **기본 내장 도메인**: DB/ORM 쿼리, Cache, Auth, Middleware, WS/WT 실시간 룸, 백그라운드 잡, `lace` 웹 프로덕션 서빙이 기본 내장 |

상위 범용 명세와 문서 상태 규칙(`확정` / `제안` / `미정`)은 [`hyperish/docs`](https://github.com/hyperish/docs)를 따릅니다.

## 문서 목록

| 번호 | 문서 | 내용 |
|---|---|---|
| 00 | [개요와 범위](docs/00-overview.md) | `ring`의 정체성, 백엔드 도메인 범위(DB·Cache·MSA·WS/WT), `loco.rs` 레퍼런스, 코어 `ref` 원칙 |
| 01 | [DX 및 DSL 설계](docs/01-dx-and-dsl.md) | `loco.rs` + Dioxus 매크로 감각을 결합한 백엔드 선언적 DX(리소스·마이그레이션·라우트·워커) |
| 02 | [아키텍처와 크레이트 구조](docs/02-architecture.md) | `apps/`, `crates/` 모노레포 구성, `hyper` 코어 참조 구조, 백엔드 도메인 크레이트 및 내부 포크 |
| 03 | [파이프라인과 호환 어댑터](docs/03-adapters-and-pipeline.md) | DB/Cache/MSA/WS·WT 파이프라인 및 HTTP/JSON·WebSocket 등 외부 호환 어댑터 경계 |
| 04 | [열린 결정](docs/04-open-questions.md) | `ring` 레포의 미정 사항 목록과 결정 기록 |
