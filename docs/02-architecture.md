# 02. 아키텍처와 크레이트 구조 (ring)

> 상태: `apps/`·`crates/` 모노레포 및 `hyper` 코어 `ref` 구조는 **확정**, 세부 크레이트 분할은 **제안** · 기준일: 2026-11-17

`ring` 레포는 Cargo 워크스페이스 기반의 모노레포(`apps/`, `crates/`)로 구성합니다(`hyperish/docs`의 `05-development-rules.md` §1).

## 1. 워크스페이스 디렉터리 구조 (제안)

```
ring/
  apps/
    ring-bench/           # 백엔드 처리량·지연시간·룸 동시성 벤치마크 및 검증 러너
  crates/
    ring/                 # 사용자 진입 크레이트 (단일 의존성 재수출 + prelude + TestClient)
    ring-core/            # BackendComponent 트리(`provide`, `wrap`, `mount`), 컨텍스트(`Ctx<T>`), 미들웨어, 취소 파이프
    ring-macros/          # 라우트·컴포넌트·모델·워커·룸 보조 proc macro
    ring-auth/            # 세션 쿠키+CSRF, Bearer/토큰 인증, RBAC 권한 가드(`AuthGuard`), 즉시 철회 검증
    ring-db/              # ORM/쿼리 빌더, 커넥션 풀, 트랜잭션, 마이그레이션 (`hyper` 모델 직결)
    ring-cache/           # 인메모리·분산 캐시, 멱등 키 저장소, 레이트리미터(`RateLimit`)
    ring-openapi/         # `hyper`의 범용 스키마와 `ring` 라우트를 HTTP OpenAPI(Swagger) 규격으로 변환·서빙(`.openapi("/docs")`)
    ring-msa/             # 서비스 간 내부 통신(RPC/이벤트 버스), 분산 트레이싱, 헬스체크
    ring-realtime/        # WebSocket(WS) 및 WebTransport(WT) 룸·프레즌스·seq 상태 조정자
    ring-jobs/            # 백그라운드 워커, 지연/반복 작업 큐, 실패 재시도·보상(SAGA) 관리
    ring-storage/         # 대용량 오브젝트/에셋 스토리지 바인딩 및 단기 서명 URL/검증
    ring-adapter-http/    # 외부 호환 어댑터: HTTP/JSON (hyperish/docs 03 규약 구현)
    ring-adapter-ws/      # 외부 호환 어댑터: WebSocket / WebTransport (hyperish/docs 04 규약 구현)
    ring-adapter-lace/    # `lace` 웹 빌드 산출물의 프로덕션 서빙 및 SSR 통합 어댑터
```

## 2. 계층 의존 방향과 `hyper` 코어(`ref`) 수직화

```
[ ring-adapter-http / ring-adapter-ws / ring-adapter-lace / 스토리지·DB 드라이버 ]
                             │
                             ▼ (단방향 의존)
       [ ring-core / ring-db / ring-cache / ring-msa / ring-realtime ]
                             │
                             ▼ (코어 ref)
              [ hyper (core / state / serde / api) ]
```

- **코어 무오염 원칙**: `ring-core`와 비즈니스 서비스 코드는 외부 전송 규약(HTTP 헤더, JSON 문자열, 특정 클라우드 바인딩 타입)을 직접 다루지 않고 `hyper`의 모델·오류·취소 계약을 기준으로 동작합니다.
- **내부 포크 및 `unit optimization`**: HTTP/QUIC 서버·쿼리 엔진 등 하위 포크 크레이트의 최적화 절차는 `hyperish/docs`의 `05-development-rules.md` §7을 따릅니다.
