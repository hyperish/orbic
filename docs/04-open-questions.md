# 04. 열린 결정 (orbic)

> 상태: "열려 있는 결정"은 **미정**(방향은 **제안**), "결정됨"은 **확정** 기록(제안으로 추가한 내용은 항목 안에 표시) · 기준일: 2026-11-24

`orbic` 레포 범위의 미정 사항 목록입니다. 범용 명세 전체의 결정 기록은 [`hyperish/docs/docs/07-open-questions.md`](https://github.com/hyperish/docs/blob/main/docs/07-open-questions.md)에서 추적합니다.

## 열려 있는 결정

| # | 결정 항목 | 왜 중요한가 | 지금의 방향(제안) |
|---|---|---|---|
| 1 | 네이티브 서버 런타임과 엣지(Cloudflare Workers/DO/D1/R2) 어댑터 지원 비중 | 배포 타깃에 따라 소켓·이벤트 루프 제어 범위와 스토리지 어댑터 우선순위가 달라짐 | `orbic-core`는 런타임 어댑터로 분리해 네이티브 멀티스레드 서버를 기본으로 두되, 필요 시 엣지/클라우드 바인딩 어댑터를 장착 가능하게 설계 |

## 결정됨

- **Rust 스타일 `BackendComponent` 조립 아키텍처 및 `loco.rs` 레퍼런스 확정** (2026-11-17 ~ 2026-11-18): `00-overview.md`, `01-dx-and-dsl.md`
- **DB 투트랙 쿼리(`#[table]` 체이닝 + `sql!` 매크로), L1/L2 Tiered 캐시(Singleflight·태그 무효화), 웹 쿠키+네이티브 Bearer 통합 Opaque 세션(권위 저장소 직결 즉시 철회 기본 — 2026-11-24 보정), Dual-Rail 네트워크(표준 HTTP/WS + 고속 `hyve-serde`/WebTransport) 및 필수 백엔드 도메인 확정** (2026-11-18 ~ 2026-11-19): `00-overview.md`, `02-architecture.md`, `03-adapters-and-pipeline.md`
- **crates.io 비충돌 고유명 `orbic`(구 `ring`) 및 순수 `MIT` 라이선스 확정** (2026-11-19): 레포명·crates.io 패키지명·Rust 크레이트명을 `orbic`으로 1:1 일치
- **MIT 포크 스택(`hyper`+`matchit`·`sqlx-core`·`quick_cache`·`fred`·`quinn`/`wtransport`·`tokio-tungstenite`·`utoipa`), 내부 코어 개조 방향, `#[server]`↔`BackendComponent::handle` 명시 등록 계약 및 Phase 1~3 컷 확정** (2026-11-19):
  - Phase 1(`v0.1.0`): `orbic-core`·`orbic-macros`(`#[table]`·`sql!`)·`orbic-adapter-http`(표준 레일 + 고속 레일 요청-응답)·`orbic-db`·`orbic-cache` 인메모리 저장소(세션·멱등 키·`RateLimit`)·`orbic-auth`(권위 저장소 직접 조회)·`orbic-openapi`·`TestClient`
  - Phase 2(`v0.2.0`): `orbic-cache` L2(Redis/Valkey)·Tiered(L1+L2)·`orbic-storage`·`orbic-jobs`(DLQ·분산 락 `.every`)·`orbic-adapter-serge`(SSR/SSG)
  - Phase 3(`v0.3.0+`): `orbic-adapter-ws`(WebTransport+WS 폴백)·`orbic-realtime`·`orbic-msa`·SAGA·`orbic-notify` (`01-dx-and-dsl.md` §2, `02-architecture.md` §3)
- **Phase 1 의존 폐포 보정 및 세션 권위 저장소 정리** (2026-11-24): `orbic-db`가 쓰는 `orbic-macros`와 세션·멱등 키·`RateLimit`가 쓰는 `orbic-cache` 인메모리 저장소를 Phase 1에 포함(위 목록에 반영). 즉시 철회의 확정 내용은 "권위 저장소 직접 조회"이며, 단일 노드는 인프로세스 저장소가, 멀티 레플리카는 L2(Redis/Valkey)가 권위 저장소 (`00-overview.md` §1, `03-adapters-and-pipeline.md` §2)
- **Dual-Rail 폴백에서도 고속 레일은 `hyve-serde` 바이너리 유지** (2026-11-24): 고속 레일의 요청-응답은 `orbic-adapter-http` 바이너리 본문, 스트림·실시간은 WebTransport이며 WS로 폴백해도 바이너리 프레임을 유지. JSON 텍스트 프레임은 표준 레일 전용 (`00-overview.md` §1, `02-architecture.md` §1, §3, `03-adapters-and-pipeline.md` §2)
- **`#[server]` 게이팅·소유 기준 정리** (2026-11-24): 클라이언트 빌드 조건 표기를 `not(feature = "server")`로 바로잡고, 제거 기준이 타깃이 아니라 `server` 피처임을 명시. 매크로는 `hyve-macros` 소유이고 메서드·경로 인자는 `orbic`만 해석 (`01-dx-and-dsl.md` §2)
- **`orbic-core`와 `orbic-adapter-http`의 분담 정리** (2026-11-24): 코어 무오염 원칙에 맞춰 CORS·보안 헤더·`413`·`/health`·`/metrics` 노출은 `orbic-adapter-http`로, 전송 중립 요청 파이프라인과 셧다운 조율은 `orbic-core`로 정리. 파이프라인은 `orbic-core`가 소유하고 모든 어댑터와 `TestClient`가 같은 진입점을 호출 (`00-overview.md` §1, `02-architecture.md` §1, §3, `03-adapters-and-pipeline.md` §1)
- **포크 표 보정** (2026-11-24): `quinn` 라이선스 기재, 표의 버전은 확인 시점 스냅샷으로 취급 (`02-architecture.md` §3)
- **`sql!` 오프라인 검증 방식과 방언 테스트 분리 확정** (2026-11-24): 마이그레이션 AST 방식 대신 레포에 커밋하는 오프라인 메타데이터 캐시를 채택. 매크로는 컴파일 중 DB에 접속하지 않고, 캐시에 없는 쿼리는 `hyve codegen` 안내가 담긴 컴파일 오류가 됨. PostgreSQL은 캐시를 다시 만드는 머신에만 필요. `Db::in_memory()`는 SQLite 인메모리이며, 검증되지 않은 방언의 DB에서 `sql!`을 실행하면 명시적 오류. 캐시 생성(`codegen` 단계)과 임시 PostgreSQL 하네스는 `orbic-db`와 함께 Phase 1에 속함. 컴파일 타임 스키마 지문 비교는 **제안**으로 남김 (`00-overview.md` §1, `01-dx-and-dsl.md` §4)
- **`#[server]` 요청 모델·오류 복원 규칙 반영** (2026-11-24): `hyve`가 확정한 호출 스텁 계약(`hyve/docs/02-architecture.md` §2.4)에 맞춰, 전송 인자 1개는 요청 본문 그대로 받고 표준 레일 오류는 03 규약 봉투에 필드를 추가하지 않으며, HTTP 변환이 오류 종류와 재시도 가능 여부를 왕복 가능하게 보존 (`01-dx-and-dsl.md` §2, `03-adapters-and-pipeline.md` §1)
