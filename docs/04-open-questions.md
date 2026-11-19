# 04. 열린 결정 (ring)

> 기준일: 2026-11-19

`ring` 레포 범위의 미정 사항 목록입니다. 범용 명세 전체에 걸친 미정 사항(crates.io `ring` 선점명 네임스페이스 전략, 인라인 서버 함수 공통 계약 등)은 [`hyperish/docs/docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md)에서 추적합니다.

## 열려 있는 결정

| # | 결정 항목 | 왜 중요한가 | 지금의 방향(제안) |
|---|---|---|---|
| 1 | 포크·기반 대상 백엔드 크레이트 선정 (HTTP/QUIC·DB·캐시) | `ring` 코어 구조를 구현할 때 어떤 하위 크레이트를 포크해 1단계 `unit optimization`할지 결정 필요 | HTTP/라우팅(`axum` / crates.io의 외부 HTTP 크레이트 `hyper`), QUIC/WT(`quinn`), DB(`sqlx` 코어), L1 캐시(`moka`), L2 캐시(`fred`/`redis`) 중 라이선스와 제로카피 통합성 기준으로 선정 |
| 2 | 네이티브 서버 런타임과 엣지(Cloudflare Workers/DO/D1/R2) 어댑터 지원 비중 | 배포 타깃에 따라 소켓·이벤트 루프 제어 범위와 스토리지 어댑터 우선순위가 달라짐 | `ring-core`는 런타임 어댑터로 분리해 네이티브 멀티스레드 서버를 기본으로 두되, 필요 시 엣지/클라우드 바인딩 어댑터를 장착 가능하게 설계 |
| 3 | `sql!` 오프라인 컴파일 타임 검증 방식 및 `Db::in_memory()` 방언 정합성 | 라이브 DB 없이 IDE·CI 빌드가 통과해야 하며, Postgres 전용 SQL과 SQLite 기반 `Db::in_memory()` 간 테스트 불일치를 막아야 함 | 마이그레이션 AST 정적 검증 또는 `hyper codegen` 오프라인 메타데이터 캐시를 비교 선정하고, Postgres 방언 전용 테스트는 임시 Postgres 하네스 분리 지원 |
| 4 | 인라인 서버 함수와 `BackendComponent` 가드·마운트·Dual-Rail 결합 계약 | `ring`의 명시적 `.wrap(AuthGuard)`·`.mount("/v1", ...)` 구조와 인라인 서버 함수 호출이 충돌 없이 공존해야 함 | [`docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md) #3과 연동해 풀스택 단일 스니펫 기준으로 확정 |
| 5 | `ring` 구현 Phase 1~3 우선순위 컷 | 모든 백엔드 도메인을 동시에 구현하면 첫 수직 슬라이스가 지연됨 | Phase 1(`ring-core`·HTTP 어댑터·`ring-db`·`ring-auth`·`TestClient`·OpenAPI) → Phase 2(`ring-cache`·`ring-storage`·`ring-jobs`·`ring-adapter-lace` SSR) → Phase 3(WebTransport·`ring-msa`·SAGA·`ring-notify`) 제안 |

## 결정됨

- **Rust 스타일 `BackendComponent` 조립 아키텍처 및 `loco.rs` 레퍼런스 확정** (2026-11-17 ~ 2026-11-18): `00-overview.md`, `01-dx-and-dsl.md`
- **DB 투트랙 쿼리(`#[table]` 체이닝 + `sql!` 매크로), L1/L2 Tiered 캐시(Singleflight·태그 무효화), 웹 쿠키+네이티브 Bearer 통합 Opaque 세션(L2 직결 즉시 철회 기본), Dual-Rail 네트워크(표준 HTTP/WS + 고속 `hyper-serde`/WebTransport) 및 필수 백엔드 도메인 확정** (2026-11-18 ~ 2026-11-19): `00-overview.md`, `02-architecture.md`, `03-adapters-and-pipeline.md`
