# 00. 개요와 범위 (ring)

> 상태: 역할·도메인 범위·레퍼런스(`loco.rs`)는 **확정**, 세부 모듈 구성은 **제안** · 기준일: 2026-11-17

`ring`은 하이퍼리쉬 프레임워크 패밀리의 **백엔드 프레임워크** 레포입니다. 공통 추상 계약은 [`hyperish/docs`](https://github.com/hyperish/docs)를 따르며, 코어 모델·상태·직렬화·API 규격 인터페이스는 [`hyper`](https://github.com/hyperish/hyper)를 단일 기준(`ref`)으로 참조합니다.

## 1. 확정된 역할과 도메인 범위

`ring`은 고정된 폴더 구조를 강제하지 않는 **Rust 스타일 컴포넌트(`BackendComponent`) 조립 아키텍처** 위에서 백엔드 개발과 운영에 필요한 도메인 전반을 다룹니다:

1. **컴포넌트 기반 아키텍처 & 미들웨어(`ring-core`)**:
   - 상태·의존성 주입(`provide`/`Ctx<T>`), 미들웨어·가드 래핑(`wrap`), 라우트 핸들러, 실시간 룸, 백그라운드 워커를 도메인/기능 단위의 `BackendComponent`로 묶어 조립(`.mount()`)합니다.
   - 1인 개발자의 단일 파일 구성부터 대규모 팀의 DDD·헥사고날·MSA 분리까지 개발자가 아키텍처를 직접 설계할 수 있도록 고점을 높입니다.
2. **데이터베이스(DB) & ORM 쿼리 (`ring-db`)**: 스키마 정의, 마이그레이션, 타입 안전 ORM/쿼리 빌더, 트랜잭션과 커밋/보상 지점 관리(`hyper` 모델과 매핑 보일러플레이트 없이 직결).
3. **캐시(Cache) & 상태 조정 (`ring-cache`)**: 인메모리·분산 캐시 계층, 세션·레이트리밋 상태, 멱등 키(Idempotency-Key) 보관소.
4. **인증·권한 (Auth & Guard — `ring-auth`)**: 쿠키 세션+CSRF, Bearer/토큰 인증, RBAC 권한 가드(`AuthGuard`), 요청별 접근 권한 검증·철회 즉시 반영(`hyperish/docs` `02-core-contracts.md` §6).
5. **Swagger/OpenAPI 변환·서빙 & 인메모리 `TestClient` (`ring` 전용)**:
   - OpenAPI/Swagger는 HTTP 백엔드 도메인 규격이므로 `hyper`가 아닌 `ring`(`ring-openapi`)이 전담합니다. `ring`은 `hyper` 모델의 범용 스키마(`#[validate]`)와 `ring` 핸들러 시그니처를 읽어 OpenAPI 문서로 변환하고 `.openapi("/docs")` 한 줄로 서빙합니다.
   - `TestClient::new(app)`로 실제 네트워크 포트를 띄우지 않고도 컴포넌트/앱 전체에 요청을 보내 응답과 DB 상태를 즉시 검증합니다.
6. **MSA · 실시간 통신 (WS / WT) · `lace` 프로덕션 서빙**: 서비스 간 고속 내부 RPC/메시지 버스(`ring-msa`), WebSocket/WebTransport 실시간 룸·`seq` 조율(`ring-realtime`), 백그라운드 작업 큐(`ring-jobs`), 그리고 [`lace`](https://github.com/hyperish/lace) 웹 빌드의 프로덕션 서빙·SSR 호스팅(`ring-adapter-lace`).

## 2. 레퍼런스와 구현 전략 (확정)

| 축 | 레퍼런스 / 전략 | 채용 이유 |
|---|---|---|
| **백엔드 아키텍처 & DX** | **Loco (`loco.rs`)** | 라우팅·컨트롤러·모델/마이그레이션·백그라운드 워커·태스크가 하나로 정돈된 배터리 포함(batteries-included) 백엔드 생산성 |
| **선언적 매크로 & 순수 Rust 빌더** | **`hyper` DSL + GPUI식 가독성 높은 Rust 빌더** | 선언적 어트리뷰트/매크로로 간결하게 쓰되, 순수 Rust 빌더·체이닝만으로도 가독성이 좋으면 매크로 없이도 작성 가능 |
| **코어 수직화** | **`hyper` 코어(`ref`) 기반 + 호환 어댑터 분리** | `hyper-core`, `hyper-serde`, `hyper-api`를 그대로 참조해 크로스플랫폼 UI(`lace`, 웹·네이티브)와 타입·계약을 무변환 수직 통합 |
| **성능 확보 (`unit optimization`)** | **고성능 검증 크레이트 포크 + 단계적 `unit optimization`** | 1단계로 불필요한 범용 추상화·미사용 피처를 제거하고 `hyper` 코어에 직결하며, 추후 병목 구간에 2단계(메모리 레이아웃·SIMD 등) 심화 개조 여지 확보 |

## 3. 책임 경계: 소유하는 것과 소유하지 않는 것

- **소유함**:
  - 서버 애플리케이션 수명주기(부트스트랩, 설정, 헬스체크, 그레이스풀 셧다운), DB/Cache/MSA/WS·WT 런타임 추상화, 백그라운드 잡 스케줄러, 서버 측 외부 호환 어댑터(HTTP/JSON, WebSocket, WebTransport, `lace` 웹 프로덕션 서빙 등).
- **소유하지 않음**:
  - 범용 직렬화·파서·상태 프리미티브와 공통 API 계약 매크로 정의(`hyper` 소관)
  - 브라우저 DOM 및 데스크톱/모바일 네이티브 클라이언트 UI 렌더링(`lace` 소관)
- **의존 방향**:
  - `ring` → `hyper` (단방향). `ring`의 코어 도메인 로직은 특정 클라우드 벤더나 외부 프로토콜에 종속되지 않으며, 어댑터 계층(`ring-adapter-*`)을 통해 외부 세계와 연결됩니다.
