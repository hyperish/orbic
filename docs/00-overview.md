# 00. 개요와 범위 (orbic)

> 상태: 역할·도메인 범위·레퍼런스(`loco.rs`)는 **확정**, 세부 모듈 구성 및 구현 Phase 컷은 **제안** · 기준일: 2026-11-19

`orbic`은 하이퍼리쉬 프레임워크 패밀리의 **백엔드 프레임워크** 레포입니다. 공통 추상 계약은 [`hyperish/docs`](https://github.com/hyperish/docs)를 따르며, 코어 모델·상태·직렬화·API 규격 인터페이스는 [`hyve`](https://github.com/hyperish/hyve)를 단일 기준(`ref`)으로 참조합니다.

## 1. 확정된 역할과 도메인 범위

`orbic`은 고정된 폴더 구조를 강제하지 않는 **Rust 스타일 컴포넌트(`BackendComponent`) 조립 아키텍처** 위에서 백엔드 개발과 운영에 필요한 도메인 전반을 다룹니다:

1. **컴포넌트 기반 아키텍처 & 운영 안전장치 (`orbic-core`)**:
   - 상태·의존성 주입(`provide`/`Ctx<T>`), 미들웨어·가드 래핑(`wrap`), 라우트 핸들러, 실시간 룸, 백그라운드 워커를 도메인/기능 단위의 `BackendComponent`로 묶어 조립(`.mount()`)합니다.
   - CORS 화이트리스트, 보안 헤더(CSP, HSTS), 요청 크기 제한(`413`), 헬스체크(`/health`), 메트릭(`/metrics`), 진행 중인 트랜잭션·요청 완료 후 닫는 **Graceful Shutdown**을 기본 내장합니다.
2. **데이터베이스(DB) & SQL 투트랙 쿼리 (`orbic-db`)**:
   - 별도 ORM 엔티티 보일러플레이트 없이 `hyve`의 `#[model]` 구조체에 `#[table("...")]`만 붙여 직결합니다.
   - **경량 타입 안전 체이닝**(일상적인 CRUD·동적 필터·커서 페이지네이션)과 **컴파일 타임 스키마 검증 `sql!` 매크로**(복잡한 조인·집계·튜닝 쿼리 탈출구; 라이브 DB 없는 IDE·CI를 위해 마이그레이션 스키마/오프라인 메타데이터 검증 지원) 투트랙을 제공하며, **PostgreSQL**과 **SQLite / LibSQL**을 1급 지원합니다.
   - `db.tx(...)` 클로저 기반 트랜잭션과 요청 `CancellationToken` 발동 시 자동 롤백을 내장합니다.
3. **계층형 캐시 서버 (`orbic-cache`)**:
   - **In-Memory(`Moka` 계열 L1)**, **Redis / Valkey(RESP3 파이프라이닝 L2)**, 그리고 읽기는 L1에서 처리하고 쓰기/삭제 시 Redis Pub/Sub·Invalidation으로 L1을 자동 무효화하는 **Tiered(L1+L2) 캐시**를 지원합니다.
   - 캐시 만료 시 동시 DB 폭주(Stampede)를 막는 **Singleflight(`get_or_try_init`)**, **태그(`tag!`) 기반 일괄 무효화**, 멱등 키(`Idempotency-Key`) 보관소 및 분산 레이트리미터(`RateLimit`)를 기본 내장합니다.
4. **플랫폼 통합 세션 · 쿠키 · 인증 (`orbic-auth`)**:
   - **웹(`serge-web`)**은 `HttpOnly + Secure + SameSite=Lax` 쿠키 + `X-CSRF-Token` 검증으로, **네이티브(`serge-native`)·CLI**는 OS 보안 키체인 기반 `Bearer` 토큰으로 전송하되 핸들러는 동일한 `Ctx<CurrentUser>`로 주입받습니다.
   - 즉시 철회(`02-core-contracts.md` §6)를 보장하기 위해 Stateless JWT 대신 `orbic-cache` 기반 고엔트로피 **불투명 세션(Opaque Session)**을 기본으로 채택하며, 슬라이딩 연장 및 리프레시 토큰 회전(재사용 감지 시 전체 폐기)을 내장합니다. **인증·철회 검사는 L1 비동기 무효화 지연으로 인한 유출을 막기 위해 L2(Redis/Valkey) 직접 조회를 기본**으로 하며, L1 캐싱을 옵트인할 경우 초단기 TTL·Pub/Sub 단절 시 L1 전체 플러시·최대 노출 창을 명시해야 합니다.
5. **Dual-Rail 네트워크 · 인라인 서버 함수 · Swagger & `TestClient`**:
   - **표준 호환 레일**: 외부 호환용 HTTPS/JSON + WebSocket(`03`, `04` 규약 준수) 및 `.openapi("/docs")` Swagger 자동 서빙.
   - **수직 통합 고속 레일**: `serge` ↔ `orbic` 및 `orbic` 내부 MSA 간 통신은 **`hyve-serde` 제로카피 바이너리 + WebTransport(QUIC, UDP 차단 시 WS 자동 폴백)**로 직결하며, 프론트에서 백엔드 핸들러를 일반 Rust 비동기 함수처럼 부르는 **인라인 서버 함수 호출**과 종단 간 취소 전파를 지원합니다.
6. **파일 스토리지(`orbic-storage`) · 잡 큐/Cron(`orbic-jobs`) · 외부 알림/웹훅(`orbic-notify`) · `serge` 서빙**:
   - 로컬 디스크 및 S3 / Cloudflare R2 추상화, Presigned 멀티파트 이어 올리기(`upload-parts`) 및 크기·체크섬 검증 후 활성화.
   - 지연 작업·지수 백오프 재시도·데드레터 큐(DLQ) 워커 및 분산 리더 락 기반 `.every(...)` / Cron 주기적 스케줄러(멀티 레플리카 중복 실행 방지).
   - 이메일(SMTP/API)·모바일/웹 푸시(APNs/FCM) 발송 어댑터 및 외부 웹훅 수신 시 HMAC 서명 검증.
   - [`serge`](https://github.com/hyperish/serge) 웹 빌드의 프로덕션 서빙·SSR 호스팅(`orbic-adapter-serge`).

## 2. 레퍼런스와 구현 전략 (확정)

| 축 | 레퍼런스 / 전략 | 채용 이유 |
|---|---|---|
| **백엔드 아키텍처 & DX** | **Loco (`loco.rs`)** | 라우팅·컨트롤러·모델/마이그레이션·백그라운드 워커·태스크가 하나로 정돈된 배터리 포함(batteries-included) 백엔드 생산성 |
| **컴포넌트 조립 & 코어 매크로** | **순수 Rust 체이닝(조립 고정) + `hyve` 모델·`sql!` 매크로** | 컴포넌트·라우트·가드·워커 조립은 순수 Rust 체이닝으로 고정해 가독성과 아키텍처 고점을 확보하고, 모델·테이블·정적 SQL 검증 등 보일러플레이트가 큰 영역만 매크로를 사용 |
| **코어 수직화** | **`hyve` 코어(`ref`) 기반 + 호환 어댑터 분리** | `hyve-core`, `hyve-serde`, `hyve-api`를 그대로 참조해 크로스플랫폼 UI(`serge`, 웹·네이티브)와 타입·계약을 무변환 수직 통합 |
| **성능 확보 (`unit optimization`)** | **고성능 검증 크레이트 포크 + 단계적 `unit optimization`** | 1단계로 불필요한 범용 추상화·미사용 피처를 제거하고 `hyve` 코어에 직결하며, 추후 병목 구간에 2단계(메모리 레이아웃·SIMD 등) 심화 개조 여지 확보 |

## 3. 책임 경계: 소유하는 것과 소유하지 않는 것

- **소유함**:
  - 서버 애플리케이션 수명주기(부트스트랩, 설정, 헬스체크, 그레이스풀 셧다운), DB/Cache/MSA/WS·WT 런타임 추상화, 백그라운드 잡 스케줄러, 서버 측 외부 호환 어댑터(HTTP/JSON, WebSocket, WebTransport, `serge` 웹 프로덕션 서빙 등).
- **소유하지 않음**:
  - 범용 직렬화·파서·상태 프리미티브와 공통 API 계약 매크로 정의(`hyve` 소관)
  - 브라우저 DOM 및 데스크톱/모바일 네이티브 클라이언트 UI 렌더링(`serge` 소관)
- **의존 방향**:
  - `orbic` → `hyve` (단방향). `orbic`의 코어 도메인 로직은 특정 클라우드 벤더나 외부 프로토콜에 종속되지 않으며, 어댑터 계층(`orbic-adapter-*`)을 통해 외부 세계와 연결됩니다.
