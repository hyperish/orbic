# 01. DX 및 DSL 설계 (ring)

> 상태: `loco.rs` 및 Dioxus 매크로 레퍼런스는 **확정**, 구체 매크로 문법은 **제안/미정** · 기준일: 2026-11-17

`ring`의 DX 목표는 **`loco.rs`처럼 백엔드에 필요한 모든 부품(DB, 캐시, 워커, 실시간 룸, MSA)이 처음부터 제자리에 준비되어 있으면서**, Dioxus 매크로 스타일의 선언적 DSL로 읽기 쉽고 안전하게 작성하는 것입니다.

## 1. 생산성 중심 백엔드 표현 원칙 (확정)

- **DSL은 개발 생산성을 위한 수단**: 어트리뷰트·선언 매크로 DSL을 쓰는 이유는 오직 개발 생산성 때문이며, 순수 Rust 빌더·트레이트·코드젠 등 **생산성과 가독성이 충족되거나 더 효율적인 방법이 있다면 어떤 방법을 사용해도 무관**합니다.
- **한눈에 읽히는 서비스·리소스 정의**: 엔드포인트, 권한 조건, 멱등성 여부, 트랜잭션 경계, 캐시 정책을 간결하게 표현합니다.
- **컴파일 타임 스키마·쿼리·계약 검증**: `hyper-api` 계약과 핸들러 시그니처가 어긋나거나, 모델 필드·마이그레이션 타입이 맞지 않으면 컴파일 타임에 진단합니다.
- **Rust 탈출구 보장**: 복잡한 쿼리, 커스텀 소켓 제어, 저수준 스트리밍이 필요할 때는 언제든 하위 `ring_core` / `ring_db` / `ring_realtime` Rust API로 직접 내려갈 수 있습니다.

### 개념 스케치 (제안 — 구체 문법은 미정)

```rust
use hyper::prelude::*;
use ring::prelude::*;

#[ring::service(api = ProjectService)]
pub struct ProjectBackend {
    #[inject] db: DbPool,
    #[inject] cache: CacheClient,
}

#[ring::routes]
impl ProjectBackend {
    #[get("/v1/projects/{id}", auth = RequireViewer, cache_ttl = "30s")]
    pub async fn get_project(&self, ctx: &RequestCtx, id: Id<Project>) -> RingResult<ProjectSummary> {
        self.cache
            .get_or_try_init(&id, || self.db.find_project(ctx.cancel_token(), id))
            .await
    }
}

#[ring::room("projects/{id}")]
pub mod project_room {
    // WS / WebTransport 공통 룸 핸들러 (seq 단조 증가 및 재연결 복구 내장)
}
```

> 위 코드 블록은 `loco.rs` + `hyper` 매크로 결합 시의 사용성을 보여주는 스케치이며, 구체 문법은 [`04-open-questions.md`](04-open-questions.md)에서 확정합니다.

## 2. `hyper` CLI와 통합된 백엔드 개발 흐름

`loco.rs`가 제공하는 제너레이터·마이그레이션·워커 실행 경험을 `hyper` CLI 서브커맨드/워크플로 안으로 통합합니다:

- **스캐폴딩**: `hyper init --template ring-service` 또는 리소스·마이그레이션·워커 생성.
- **로컬 개발·프리뷰 (`hyper preview`)**: 코드 변경 시 서버 재기동 및 로컬 DB/캐시 테스트 하네스 자동 연결.
- **정적 검증 (`hyper lint`)**: 누락된 권한 검사(`02-core-contracts.md` §6), 취소 토큰 미전파(`§3`), 어댑터 타입의 코어 유입(`§5`)을 정적 검사로 차단.
- **계약·통합 테스트 (`hyper test`)**: 인메모리 DB/캐시 하네스를 사용해 HTTP/JSON(`03-api-protocol.md`) 및 WS/WT(`04-realtime-communication.md`)의 멱등성·낙관적 잠금·재연결 시나리오를 자동 검증.
