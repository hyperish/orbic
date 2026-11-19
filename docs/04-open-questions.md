# 04. 열린 결정 (orbic)

> 기준일: 2026-11-19

`orbic` 레포 범위의 미정 사항 목록입니다. 범용 명세 전체의 결정 기록은 [`hyperish/docs/docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md)에서 추적합니다.

## 열려 있는 결정

| # | 결정 항목 | 왜 중요한가 | 지금의 방향(제안) |
|---|---|---|---|
| 1 | 네이티브 서버 런타임과 엣지(Cloudflare Workers/DO/D1/R2) 어댑터 지원 비중 | 배포 타깃에 따라 소켓·이벤트 루프 제어 범위와 스토리지 어댑터 우선순위가 달라짐 | `orbic-core`는 런타임 어댑터로 분리해 네이티브 멀티스레드 서버를 기본으로 두되, 필요 시 엣지/클라우드 바인딩 어댑터를 장착 가능하게 설계 |
| 2 | `sql!` 오프라인 컴파일 타임 검증 세부 방식 (마이그레이션 AST vs 메타데이터 캐시) 및 `Db::in_memory()` 방언 테스트 분리 | 라이브 DB 없이 IDE·CI 빌드가 통과해야 하며, Postgres 전용 SQL과 SQLite 기반 `Db::in_memory()` 간 테스트 불일치를 막아야 함 | 마이그레이션 AST 정적 검증 또는 `hyve codegen` 오프라인 메타데이터 캐시를 비교 선정하고, Postgres 방언 전용 테스트는 임시 Postgres 하네스 분리 지원 |

## 결정됨

- **Rust 스타일 `BackendComponent` 조립 아키텍처 및 `loco.rs` 레퍼런스 확정** (2026-11-17 ~ 2026-11-18): `00-overview.md`, `01-dx-and-dsl.md`
- **DB 투트랙 쿼리(`#[table]` 체이닝 + `sql!` 매크로), L1/L2 Tiered 캐시(Singleflight·태그 무효화), 웹 쿠키+네이티브 Bearer 통합 Opaque 세션(L2 직결 즉시 철회 기본), Dual-Rail 네트워크(표준 HTTP/WS + 고속 `hyve-serde`/WebTransport) 및 필수 백엔드 도메인 확정** (2026-11-18 ~ 2026-11-19): `00-overview.md`, `02-architecture.md`, `03-adapters-and-pipeline.md`
- **crates.io 비충돌 고유명 `orbic`(구 `ring`) 및 순수 `MIT` 라이선스 확정** (2026-11-19): 레포명·crates.io 패키지명·Rust 크레이트명을 `orbic`으로 1:1 일치
- **MIT 포크 스택(`hyper`+`matchit`·`sqlx-core`·`quick_cache`·`fred`·`quinn`/`wtransport`·`tokio-tungstenite`·`utoipa`), 내부 코어 개조 방향, `#[server]`↔`BackendComponent::handle` 명시 등록 계약 및 Phase 1~3 컷 확정** (2026-11-19):
  - Phase 1(`v0.1.0`): `orbic-core`·`orbic-adapter-http`·`orbic-db`·`orbic-auth`(L2 기본)·`orbic-openapi`·`TestClient`
  - Phase 2(`v0.2.0`): `orbic-cache`(L1+L2 Tiered)·`orbic-storage`·`orbic-jobs`(DLQ·분산 락 `.every`)·`orbic-adapter-serge`(SSR/SSG)
  - Phase 3(`v0.3.0+`): `orbic-adapter-ws`(WebTransport+WS 폴백)·`orbic-realtime`·`orbic-msa`·SAGA·`orbic-notify` (`01-dx-and-dsl.md` §2, `02-architecture.md` §3)
