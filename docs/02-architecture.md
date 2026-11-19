# 02. 아키텍처와 크레이트 구조 (orbic)

> 상태: `apps/`·`crates/` 모노레포 및 `hyve` 코어 `ref` 구조는 **확정**, 세부 크레이트 분할은 **제안** · 기준일: 2026-11-19

`orbic` 레포는 Cargo 워크스페이스 기반의 모노레포(`apps/`, `crates/`)로 구성합니다(`hyperish/docs`의 `05-development-rules.md` §1).

## 1. 워크스페이스 디렉터리 구조 (제안)

```
orbic/
  apps/
    orbic-bench/           # 백엔드 처리량·지연시간·룸 동시성 벤치마크 및 검증 러너
  crates/
    orbic/                 # 사용자 진입 크레이트 (단일 의존성 재수출 + prelude + TestClient)
    orbic-core/            # BackendComponent 트리(`provide`, `wrap`, `mount`), `Ctx<T>`, 보안 헤더·CORS·Graceful Shutdown
    orbic-macros/          # 라우트·컴포넌트·테이블(`#[table]`)·`sql!`·워커·룸 보조 proc macro
    orbic-auth/            # 웹(HttpOnly 쿠키+CSRF) & 네이티브/CLI(Bearer) 통합 Opaque 세션, RBAC `AuthGuard`, L2 직결 즉시 철회
    orbic-db/              # 경량 타입 안전 체이닝 + 오프라인 검증 `sql!` 매크로, 커넥션 풀, 트랜잭션, 마이그레이션 (Postgres, SQLite/LibSQL)
    orbic-cache/           # L1(인메모리) + L2(Redis/Valkey) Tiered 캐시, Singleflight, 태그 무효화, 멱등 키, `RateLimit`
    orbic-openapi/         # `hyve`의 범용 스키마와 `orbic` 라우트를 HTTP OpenAPI(Swagger) 규격으로 변환·서빙(`.openapi("/docs")`)
    orbic-msa/             # 서비스 간 내부 통신(`hyve-serde` 바이너리 RPC/이벤트 버스), 분산 트레이싱, 헬스체크
    orbic-realtime/        # WebTransport(QUIC) 및 WebSocket(WS) 룸·프레즌스·seq 상태 조정자
    orbic-jobs/            # 백그라운드 작업 큐(지연·재시도·DLQ), 분산 리더 락 기반 `.every`/Cron 스케줄러, 보상(SAGA) 관리
    orbic-notify/          # 이메일(SMTP/API), 모바일·웹 푸시(APNs Live Activity / FCM), HMAC 서명 검증 외부 웹훅 게이트웨이
    orbic-storage/         # 로컬/S3/R2 대용량 스토리지 바인딩, Presigned 멀티파트 업로드 및 무결성 검증
    orbic-adapter-http/    # 외부 표준 레일(HTTP/JSON, 03 규약) 및 내부 고속 레일(`hyve-serde` 바이너리) 어댑터
    orbic-adapter-ws/      # WebTransport(HTTP/3 QUIC) 우선 연결 + WebSocket(04 규약) 자동 폴백 실시간 어댑터
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

- **코어 무오염 원칙**: `orbic-core`와 비즈니스 서비스 코드는 외부 전송 규약(HTTP 헤더, JSON 문자열, 특정 클라우드 바인딩 타입)을 직접 다루지 않고 `hyve`의 모델·오류·취소 계약을 기준으로 동작합니다.

## 3. 확정 MIT 포크 스택 및 내부 코어 `unit optimization` 타깃 (2026-11-19 확정)

| 모듈 | 베이스 크레이트 (확인 버전 · 라이선스) | 포크 후 내부 코어 개조(1·2단계 `unit optimization`) 방향 |
|---|---|---|
| `orbic-adapter-http` · `orbic-core` | `hyper` (`1.12.0`, `MIT`) + `matchit` (`0.9.2`, `MIT AND BSD-3-Clause`, 외부 의존성) | `axum`/`tower`의 `BoxService`·레이어 중첩 오버헤드를 도입하지 않고 `hyper` HTTP 코어와 Radix 라우터를 `BackendComponent`에 직결 |
| `orbic-db` | `sqlx-core` (`0.9.1`, `MIT OR Apache-2.0`) | `Any` 드라이버·미사용 DB 코드 전면 제거 → `#[model]`/`#[table]` 컬럼 레이아웃에 **제로카피 Row → Struct 직접 디코딩** 및 Postgres 파이프라이닝 직결 |
| `orbic-cache` | L1 `quick_cache` (`0.7.0`, `MIT`), L2 `fred` (`10.1.0`, `MIT`) | `quick_cache`(S3-FIFO)에 Singleflight·태그 인덱스 직결; `fred`는 범용 클러스터/타입 변환 계층을 덜어내고 RESP3 파이프라이닝·`rkyv` 무복사 바이트 버퍼·Client-Side Invalidation만 남김 |
| `orbic-adapter-ws` | `quinn` (`0.11.12`) + `wtransport` (`0.7.2`, `MIT OR Apache-2.0`), `tokio-tungstenite` (`0.30.0`, `MIT`) | Dual-Rail WebTransport(QUIC) 1순위 + WS 자동 폴백, `hyve-serde`(`rkyv`) 프레임 직결 |
| `orbic-openapi` | `utoipa` (`6.0.0`, `MIT OR Apache-2.0`) | OpenAPI 3.1 스키마 모델만 경량 채택해 `hyve-validate` 리플렉션과 직결 |
