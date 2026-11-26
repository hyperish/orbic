# 02. 아키텍처와 크레이트 구조 (orbic)

> 상태: `apps/`·`crates/` 모노레포 및 `hyve` 코어 `ref` 구조는 **확정**, 세부 크레이트 분할은 **제안** · 기준일: 2026-11-24

`orbic` 레포는 Cargo 워크스페이스 기반의 모노레포(`apps/`, `crates/`)로 구성합니다(`hyperish/docs`의 `05-development-rules.md` §1).

## 1. 워크스페이스 디렉터리 구조 (제안)

```
orbic/
  apps/
    orbic-bench/           # 백엔드 처리량·지연시간·룸 동시성 벤치마크 및 검증 러너
    orbic-cli/             # `hyve-orbic` 실행 파일 (`hyve`가 호출하는 `codegen` 등 도구 단계)
  crates/
    orbic/                 # 사용자 진입 크레이트 (단일 의존성 재수출 + prelude + TestClient)
    orbic-core/            # BackendComponent 트리(`provide`, `wrap`, `mount`), `Ctx<T>`, 전송 중립 요청 파이프라인, Graceful Shutdown 조율
    orbic-macros/          # `#[table]`·`#[pk]`·`sql!`·`ttl!`·`key!`·`tag!` (조립은 매크로가 아니라 체이닝)
    orbic-sql-cache/       # `sql!` 오프라인 캐시의 파일 형식 (`orbic-macros`와 `hyve-orbic codegen`이 공유)
    orbic-auth/            # 웹(HttpOnly 쿠키+CSRF) & 네이티브/CLI(Bearer) 통합 Opaque 세션, RBAC `AuthGuard`, 권위 저장소 직결 즉시 철회
    orbic-db/              # 경량 타입 안전 체이닝 + 오프라인 검증 `sql!` 매크로, 커넥션 풀, 트랜잭션, 마이그레이션 (Postgres, SQLite/LibSQL)
    orbic-cache/           # L1(인메모리) + L2(Redis/Valkey) Tiered 캐시, Singleflight, 태그 무효화, 멱등 키, `RateLimit`
    orbic-openapi/         # `hyve`의 범용 스키마와 `orbic` 라우트를 HTTP OpenAPI(Swagger) 규격으로 변환·서빙(`.openapi("/docs")`)
    orbic-msa/             # 서비스 간 내부 통신(`hyve-serde` 바이너리 RPC/이벤트 버스), 분산 트레이싱, 헬스체크
    orbic-realtime/        # WebTransport(QUIC) 및 WebSocket(WS) 룸·프레즌스·seq 상태 조정자
    orbic-jobs/            # 백그라운드 작업 큐(지연·재시도·DLQ), 분산 리더 락 기반 `.every`/Cron 스케줄러, 보상(SAGA) 관리
    orbic-notify/          # 이메일(SMTP/API), 모바일·웹 푸시(APNs Live Activity / FCM), HMAC 서명 검증 외부 웹훅 게이트웨이
    orbic-storage/         # 선택형 파일 저장 어댑터(로컬 디스크, S3 호환 API), 멀티파트 업로드와 무결성 검증
    orbic-adapter-http/    # 외부 표준 레일(HTTP/JSON, 03 규약) 및 내부 고속 레일(`hyve-serde` 바이너리) 어댑터, CORS·보안 헤더·요청 크기 제한·`/health`·`/metrics` 노출
    orbic-adapter-ws/      # 실시간 어댑터: 고속 레일은 WebTransport(HTTP/3 QUIC) 우선·WebSocket 바이너리 프레임 폴백, 표준 레일은 WebSocket(04 규약, JSON)
    orbic-adapter-serge/    # `serge` 웹 빌드 산출물의 프로덕션 서빙 및 SSR 통합 어댑터
```

## 2. 계층 의존 방향과 `hyve` 코어(`ref`) 수직화

```
[ orbic-adapter-http / orbic-adapter-ws / orbic-adapter-serge / 스토리지·DB 드라이버 ]
                             │
                             ▼ (단방향 의존)
       [ orbic-core / orbic-db / orbic-cache / orbic-msa / orbic-realtime ]
                             │
                             ▼ (코어 ref)
              [ hyve (core / state / serde / api) ]
```

- **`orbic-core` → `orbic-cache` (2026-11-24)**: 저장소 계층인 `orbic-cache`는 파이프라인을 모르고, `orbic-core`의 멱등성 게이트와 `RateLimit` 가드가 앱이 `provide`한 `Cache`에서 저장소를 얻습니다(`04-open-questions.md` 결정됨).
- **코어 무오염 원칙**: `orbic-core`와 비즈니스 서비스 코드는 외부 전송 규약(HTTP 헤더, JSON 문자열, 특정 클라우드 바인딩 타입)을 직접 다루지 않고 `hyve`의 모델·오류·취소 계약을 기준으로 동작합니다.

## 3. 확정 MIT 포크 스택 및 내부 코어 `unit optimization` 타깃 (2026-11-19 확정)

표의 버전·라이선스는 crates.io 레지스트리(`cargo info <크레이트>@<버전>`)에서 2026-11-24에 확인한 스냅샷입니다. 확정된 것은 베이스 크레이트의 선택이며, 포크 기준 리비전은 착수 시점에 다시 확인해 고정합니다(`hyperish/docs` `05-development-rules.md` §7).

| 모듈 | 베이스 크레이트 (확인 버전 · 라이선스) | 포크 후 내부 코어 개조(1·2단계 `unit optimization`) 방향 |
|---|---|---|
| `orbic-adapter-http` · `orbic-core` | `hyper` (`1.12.0`, `MIT`) + `matchit` (`0.9.2`, `MIT AND BSD-3-Clause`, 외부 의존성) | `axum`/`tower`의 `BoxService`·레이어 중첩 오버헤드를 도입하지 않고 `hyper` HTTP 코어(`orbic-adapter-http`)와 Radix 라우터(`orbic-core` 라우트 트리)를 `BackendComponent`에 직결 |
| `orbic-db` | `sqlx-core` (`0.9.1`, `MIT OR Apache-2.0`) + 같은 저장소의 드라이버 `sqlx-postgres`·`sqlx-sqlite` (각 `0.9.1`, `MIT OR Apache-2.0`) | `Any` 드라이버·미사용 DB 코드 전면 제거 → `#[model]`/`#[table]` 컬럼 레이아웃에 **제로카피 Row → Struct 직접 디코딩** 및 Postgres 파이프라이닝 직결 |
| `orbic-cache` | L1 `quick_cache` (`0.7.0`, `MIT`), L2 `fred` (`10.1.0`, `MIT`) | `quick_cache`(S3-FIFO)에 Singleflight·태그 인덱스 직결; `fred`는 범용 클러스터/타입 변환 계층을 덜어내고 RESP3 파이프라이닝·`rkyv` 무복사 바이트 버퍼·Client-Side Invalidation만 남김 |
| `orbic-adapter-ws` | `quinn` (`0.11.12`, `MIT OR Apache-2.0`) + `wtransport` (`0.7.2`, `MIT OR Apache-2.0`), `tokio-tungstenite` (`0.30.0`, `MIT`) | 고속 레일은 WebTransport(QUIC) 1순위이고 WS 폴백에서도 `hyve-serde`(`rkyv`) 바이너리 프레임을 유지, 표준 레일 WS는 04 규약의 JSON 텍스트 프레임 |
| `orbic-openapi` | `utoipa` (`6.0.0`, `MIT OR Apache-2.0`) | OpenAPI 3.1 스키마 모델만 경량 채택해 `hyve-validate` 리플렉션과 직결 |

- **실행 기록 (2026-11-24, 확정의 실행 방식을 바꿈)**: Phase 1의 `orbic-cache`는 `quick_cache`를 포크하지 않고 `=0.7.0`으로 고정해 의존합니다. Singleflight는 `quick_cache`의 자리 표시 가드로, 태그 무효화는 태그 인덱스 대신 **태그 세대**로 구현합니다. 무효화는 태그에 증가하는 순번을 기록하고, 항목은 적재를 시작할 때의 순번을 지니며, 읽을 때 자기 태그 중 더 늦게 무효화된 것이 있으면 버립니다. 그래서 무효화가 O(1)이고, 적재 도중에 무효화된 값도 그 뒤에 제공되지 않으며, 밀어낸 키가 인덱스에 남지 않습니다. 포크는 `hyve`와 같은 기준(측정으로 필요가 확인될 때)으로 미룹니다.
- **실행 기록 (2026-11-24, 확정의 실행 방식을 바꿈)**: Phase 1의 `orbic-db`는 `sqlx` `=0.9.1`(`runtime-tokio`, `sqlite-bundled`, `sqlite-unlock-notify`, `postgres`; `any`·`macros`·`migrate`는 켜지 않음)을 포크하지 않고 고정해 의존합니다. `hyve-orbic codegen`만 `sqlx-core`·드라이버의 `offline` 피처로 쿼리 분석(`describe`)을 씁니다. 마이그레이션 실행기, 행 디코딩, `Timestamp`의 PostgreSQL 이진 코덱은 `orbic-db`가 직접 구현합니다. `sqlx` 0.9.1의 최소 Rust 버전이 1.94이므로 `hyperish/docs` `05-development-rules.md` §8에 따라 `orbic` 워크스페이스의 `rust-version`을 1.94로 올렸습니다(`hyve`는 1.85 그대로). `orbic`에 의존하는 앱과 공유 크레이트(클라이언트·`wasm32` 빌드 포함)도 1.94 이상이 필요합니다.

2026-11-26에 SQLx 0.9.1의 `Row::try_get` 타입 검사와 오류 변환을 `orbic-db`에 편입했습니다. SQLite와 PostgreSQL 드라이버는 한 번 읽은 원시 값을 검사·디코딩하고, 같은 결과 레이아웃에서는 열 위치를 재사용합니다. 생성된 테이블·프로젝션은 디코더에서 필드를 순서대로 받아 중간 행 벡터를 생략합니다. 기존 수동 `Table` 구현은 기본 변환 경로를 유지합니다. [편입한 소스와 MIT 고지](../crates/orbic-db/src/sqlx_row/) · [드라이버](../crates/orbic-db/src/driver.rs)

- **실행 기록 (2026-11-24, 확정의 실행 방식을 바꿈)**: Phase 1의 `orbic-adapter-http`는 `hyper`를 포크하지 않고 `=1.12.0`(`http1`, `server`; 표에 기록한 버전, 2026-10-06 배포)으로 고정해 의존합니다. 착수 시점의 최신은 `1.13.0`이었으나 하루 전(2026-11-23)에 나온 버전이라 쓰지 않았습니다. `axum`/`tower` 없이 `hyper`의 연결 API에 `HttpApp`을 직접 연결합니다. 포크는 `hyve`와 같은 기준(측정으로 필요가 확인될 때)으로 미룹니다.

- **실행 기록 (2026-11-24, 확정의 실행 방식을 바꿈)**: Phase 1의 `orbic-openapi`는 `utoipa`를 포크하지 않고 `=6.0.0`(2026-09-22 배포, 최소 Rust 1.88)으로 고정해 OpenAPI 3.1 모델 타입만 씁니다. 파생 매크로는 쓰지 않지만 `macros` 피처를 켭니다. 이 피처 없이는 모델 자체가 컴파일되지 않기 때문입니다(내부 경로가 이 피처 뒤에 있음, 6.0.0과 5.5.0에서 확인). 이로써 `utoipa-gen` `6.0.1`(`MIT OR Apache-2.0`)이 더해지며, 그 의존성(`proc-macro2`·`quote`·`syn` 3)과 `utoipa`의 `serde`·`serde_json`·`indexmap`은 이미 트리에 있던 버전입니다. 소스를 편입해 이 피처 의존을 걷어내는 것은 포크 착수 기준(측정·패치 필요)을 따릅니다.

외부 의존성(포크하지 않음, 2026-11-24, crates.io 확인): `orbic-adapter-http`의 `hyper-util` (`=0.1.21`, `MIT`, `tokio` 피처의 입출력 어댑터만), `http-body-util` (`=0.1.5`, `MIT`), `http` (`=1.5.0`, `MIT OR Apache-2.0`), `bytes` (`=1.12.2`, `MIT`, 이미 트리에 있던 버전).

외부 의존성(포크하지 않음, 2026-11-24): `orbic-auth`의 `sha2` (`=0.11.1`, `MIT OR Apache-2.0`)와 `getrandom` (`=0.4.3`, `MIT OR Apache-2.0`)은 `sqlx-postgres`가 이미 트리에 두는 같은 버전을 씁니다(`getrandom`은 `hyve`의 고정 버전과도 같음).

외부 의존성(포크하지 않음, 2026-11-24 `cargo info` 확인): 패밀리 네이티브 런타임 `tokio` (`1.54.1`, `MIT` — `hyve/docs/02-architecture.md` §2.5), 라우터 `matchit`(위 표). 이 목록에 없는 의존성은 `hyperish/docs` `05-development-rules.md` §7의 외부 의존성 규칙에 따라 추가 기록합니다.


### 선택형 서버 기능의 의존성

`cache-redis`는 `fred 10.1.0`의 RESP3와 rustls를 사용합니다. 데이터는 태그·키 무효화를 발행하고, L1 값을 재사용할 때 서버에 저장된 리비전을 대조합니다. 인증과 멱등 결과는 직접 조회합니다. `storage-s3`는 `rust-s3 0.38.0`로 multipart 요청을 서명합니다. `jobs`는 `cron 0.17.0`과 `chrono 0.4.45`로 UTC 실행 시각을 계산합니다. [캐시 통합 테스트](../scripts/test-cache.sh) · [스토리지 통합 테스트](../scripts/test-storage-s3.sh) · [잡 큐 테스트](../crates/orbic-jobs/tests/queue.rs)

L1은 로드 시 태그 이력 보관 시간과 세대를 같은 잠금 아래 기록합니다. 태그가 없거나 마지막 검사 이후 세대 변경이 없으면 조회 시 태그 테이블을 잠그지 않습니다. 만료된 항목은 `quick_cache`의 `entry_async`가 같은 항목인지 확인한 뒤 하나의 로더에 교체 권한을 부여합니다. Redis/Valkey의 원격 리비전 확인은 모든 L2 히트에 적용합니다. [L1 구현](../crates/orbic-cache/src/cache.rs) · [분산 인수](../scripts/test-cache.sh)

OpenAPI 문서의 경로 조립에는 utoipa 6.0.0의 단일 메서드 전용 경로를 편입했습니다. 각 라우트에서 생성한 Operation을 GET·POST·PATCH·DELETE 슬롯에 직접 옮기고 동일 경로의 다른 메서드를 보존합니다. 반환 타입은 utoipa의 OpenApi로 유지합니다. [경로 조립과 MIT 고지](../crates/orbic-openapi/src/paths/)

TTL 0의 조회가 겹치면 L1은 적재 결과를 이미 대기 중인 호출에만 전달합니다. Valkey에서도 적재가 끝나기 전에 등록한 복제본만 결과를 회수할 수 있습니다. 마지막 수신자가 읽으면 전달 레코드를 제거하며 취소된 수신자의 레코드는 30초 후 만료됩니다. [L1 인수](../crates/orbic-cache/tests/cache.rs) · [복제본 인수](../crates/orbic-cache/tests/redis.rs)
