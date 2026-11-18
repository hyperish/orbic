# 04. 열린 결정 (ring)

> 기준일: 2026-11-17

`ring` 레포 범위의 미정 사항 목록입니다. 범용 명세 전체에 걸친 미정 사항은 [`hyperish/docs/docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md)에서 추적합니다.

## 열려 있는 결정

| # | 결정 항목 | 왜 중요한가 | 지금의 방향(제안) |
|---|---|---|---|
| 1 | 포크·기반 대상 백엔드 크레이트 선정 (HTTP/QUIC·DB·캐시) | `ring` 코어 구조를 구현할 때 어떤 하위 크레이트를 포크해 1단계 `unit optimization`할지 결정 필요 | HTTP/라우팅(`axum` / crates.io의 외부 HTTP 크레이트 `hyper`), QUIC/WT(`quinn`), DB(`sqlx` 코어), L1 캐시(`moka`), L2 캐시(`fred`/`redis`) 중 라이선스와 제로카피 통합성 기준으로 선정 |
| 2 | 네이티브 서버 런타임과 엣지(Cloudflare Workers/DO/D1/R2) 어댑터 지원 비중 | 배포 타깃에 따라 소켓·이벤트 루프 제어 범위와 스토리지 어댑터 우선순위가 달라짐 | `ring-core`는 런타임 어댑터로 분리해 네이티브 멀티스레드 서버를 기본으로 두되, 필요 시 엣지/클라우드 바인딩 어댑터를 장착 가능하게 설계 |

## 결정됨

- **Rust 스타일 `BackendComponent` 조립 아키텍처 및 `loco.rs` 레퍼런스 확정** (2026-11-17 ~ 2026-11-18): `00-overview.md`, `01-dx-and-dsl.md`
- **DB 투트랙 쿼리(`#[table]` 체이닝 + `sql!` 매크로), L1/L2 Tiered 캐시(Singleflight·태그 무효화), 웹 쿠키+네이티브 Bearer 통합 Opaque 세션, Dual-Rail 네트워크(표준 HTTP/WS + 고속 `hyper-serde`/WebTransport) 및 필수 백엔드 도메인 확정** (2026-11-18): `00-overview.md`, `02-architecture.md`, `03-adapters-and-pipeline.md`
