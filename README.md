# hyperish/ring

하이퍼리쉬(Hyperish) 프레임워크 패밀리의 **백엔드(Backend) 프레임워크** 레포입니다.

데이터베이스(DB), 캐시(Cache), 마이크로서비스(MSA)·서비스 간 통신, 실시간 통신(WebSocket / WebTransport), 인증·작업 큐 등 **백엔드 전반의 도메인**을 다룹니다. **`loco.rs`**를 레퍼런스로 삼아 컨벤션 기반의 높은 백엔드 생산성을 제공하되, 수직 통합을 위해 [`hyper`](https://github.com/hyperish/hyper)의 코어(`ref`)를 기준으로 구현하고 그 위에 외부·인프라 호환 어댑터를 배치합니다.

상위 범용 명세는 [`hyperish/docs`](https://github.com/hyperish/docs)를 따릅니다.

## 문서 상태 규칙

| 상태 | 의미 |
|---|---|
| **확정** | 명시적으로 결정된 사실. 바꾸려면 리뷰 승인과 새 결정 기록이 필요하다 |
| **제안** | 논의를 위한 설계안. 바꾸기 쉽게 적은 것이며, 확정이 아니다 |
| **미정** | 아직 결정되지 않은 사항. [`docs/04-open-questions.md`](docs/04-open-questions.md)에서 추적한다 |

## 문서 목록 (3개 코어 레포 공통 아키텍처)

| 번호 | 문서 | 내용 |
|---|---|---|
| 00 | [개요와 범위](docs/00-overview.md) | `ring`의 정체성, 백엔드 도메인 범위(DB·Cache·MSA·WS/WT), `loco.rs` 레퍼런스, 코어 `ref` 원칙 |
| 01 | [DX 및 DSL 설계](docs/01-dx-and-dsl.md) | `loco.rs` + Dioxus 매크로 감각을 결합한 백엔드 선언적 DX(리소스·마이그레이션·라우트·워커) |
| 02 | [아키텍처와 크레이트 구조](docs/02-architecture.md) | `apps/`, `crates/` 모노레포 구성, `hyper` 코어 참조 구조, 백엔드 도메인 크레이트 및 내부 포크 |
| 03 | [파이프라인과 호환 어댑터](docs/03-adapters-and-pipeline.md) | DB/Cache/MSA/WS·WT 파이프라인 및 HTTP/JSON·WebSocket 등 외부 호환 어댑터 경계 |
| 04 | [열린 결정](docs/04-open-questions.md) | `ring` 레포의 미정 사항 목록과 결정 기록 |
