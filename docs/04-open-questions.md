# 04. 열린 결정 (ring)

> 기준일: 2026-11-17

`ring` 레포 범위의 미정 사항 목록입니다. 범용 명세 전체에 걸친 미정 사항은 [`hyperish/docs/docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md)에서 추적합니다.

## 열려 있는 결정

| # | 결정 항목 | 왜 중요한가 | 지금의 방향(제안) |
|---|---|---|---|
| 1 | 포크·기반 대상 백엔드 크레이트 선정 (HTTP/QUIC·ORM/DB·워커) | `loco.rs` 레퍼런스 구조를 구현할 때 어떤 하위 크레이트를 포크해 `unit optimization`할지 결정 필요 | `axum` / `hyper`(crates.io의 외부 HTTP 크레이트, `hyperish/hyper`와 별개) / `quinn`(WebTransport) / `sea-orm` 또는 `sqlx` 등 후보 중 라이선스와 제로카피 통합성을 기준으로 선정 |
| 2 | 네이티브 서버 런타임과 엣지(Cloudflare Workers/DO/D1/R2) 어댑터 지원 비중 | 배포 타깃에 따라 소켓·이벤트 루프 제어 범위와 스토리지 어댑터 우선순위가 달라짐 | `ring-core`는 런타임 어댑터로 분리해 네이티브 멀티스레드 서버를 기본으로 두되, 필요 시 엣지/클라우드 바인딩 어댑터를 장착 가능하게 설계 |
| 3 | MSA 내부 통신 기본 프로토콜 | 서비스 간 통신의 지연시간과 직렬화 비용을 좌우함 | `hyper-serde` 기반 자체 고성능 바이너리 프레이밍(TCP/QUIC) 우선 검토 |
| 4 | WebTransport(WT)와 WebSocket(WS)의 폴백·공존 규칙 | 브라우저/네트워크 환경에 따라 QUIC(UDP) 차단 시 WS 폴백이 필요할 수 있음 | `ring-realtime`의 룸·메시지 모델을 전송 계층과 분리해 WT 우선 연결 후 WS 자동 폴백 가능하도록 설계 |

## 결정됨

- **결정**: `ring`은 DB, Cache, MSA, WS/WT 등 백엔드 전반의 도메인을 다루며, `loco.rs`를 레퍼런스로 삼고 `hyper` 코어를 `ref`로 수직 통합한 뒤 외부·인프라 호환은 어댑터로 분리함
  - **날짜**: 2026-11-17
  - **영향**: `ring` 전체 문서(`00`~`03`)
