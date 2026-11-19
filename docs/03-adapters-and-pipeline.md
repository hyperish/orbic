# 03. 파이프라인과 호환 어댑터 (ring)

> 상태: 코어 `ref` + 호환 어댑터 분리 원칙은 **확정**, 도메인별 어댑터 세부는 **제안** · 기준일: 2026-11-19

`ring`의 모든 백엔드 도메인(DB, Cache, MSA, WS/WT)은 **내부 고성능 코어 파이프라인**과 **외부·인프라 호환 어댑터**의 2단 구조로 동작합니다(`hyperish/docs` `02-core-contracts.md` §2~§7).

## 1. 백엔드 요청·작업 파이프라인

1. **어댑터 수신 및 컨텍스트 생성**: 외부 어댑터(`ring-adapter-http`, `ring-adapter-ws` 등) 또는 내부 MSA 채널이 요청을 받아 상관관계 ID(`requestId`)와 구조적 취소 토큰(`CancellationToken`)을 포함한 요청 컨텍스트(핸들러에는 `Ctx<T>`로 주입)를 생성합니다.
2. **접근 권한·멱등성 게이트**:
   - 보호 대상은 요청 시점마다 소유 계층이 권한과 철회 여부를 검사합니다(`02-core-contracts.md` §6).
   - 생성·상태 변경 요청의 `Idempotency-Key`는 `ring-cache`를 통해 중복 실행과 동시 처리(`409 request_in_progress`)를 제어합니다.
3. **도메인 실행 및 커밋/보상**:
   - `ring-db` 트랜잭션과 `ring-jobs` 워커는 취소 가능 지점과 커밋 지점을 명시하며, 커밋 이후 실패 시 보상 절차 또는 재조정(reconciliation) 상태를 기록합니다(`02-core-contracts.md` §3).
4. **응답 변환**: 내부 구조화 오류(`Domain` / `Infra` / `Internal`)는 어댑터 경계에서 내부 스택 노출 없이 외부 규약의 오류 형식으로 변환됩니다.

## 2. 호환 어댑터 구성

| 영역 | 코어 추상화 (`ring-*`) | 호환 어댑터 및 동작 방식 |
|---|---|---|
| **Dual-Rail API 전송** | `hyper-api` + `ring-core` | • **표준 레일**: 외부 호환용 **HTTP/JSON** (`hyperish/docs` `03-api-protocol.md` 준수: 커서 페이지네이션, `expectedVersion` 낙관적 잠금, `RateLimit-*` 헤더)<br>• **고속 레일**: `lace` ↔ `ring` 인라인 서버 함수 및 MSA 호출 시 **`hyper-serde` 제로카피 바이너리** 직결 및 클라이언트 언마운트 시 종단 간 취소(`CancellationToken`) 전파 |
| **Dual-Rail 실시간 (WT / WS)** | `ring-realtime` (`seq`, 룸 상태, 중복 제거) | **WebTransport(HTTP/3 QUIC 다중 스트림·데이터그램)**를 1순위로 연결하고, QUIC/UDP가 차단된 네트워크에서는 **WebSocket**(`04-realtime-communication.md`)으로 자동 폴백. 브라우저 WebTransport도 커스텀 인증 헤더를 보낼 수 없으므로 동일한 `?ticket=` 일회용 티켓 핸드셰이크를 쓰며, 폴백 전환 시에도 `seq`·중복 제거 `id` 연속성을 유지 |
| **인증·세션 (`ring-auth`)** | `ring-auth` + `ring-cache` | 웹(`HttpOnly` 쿠키 + `X-CSRF-Token`)과 네이티브/CLI(`Bearer` 토큰) 이중 전송 어댑터. 즉시 철회 보장을 위해 **L2(Redis/Valkey) Opaque 세션 직접 조회를 기본**으로 하며, L1 캐싱 옵트인 시 초단기 TTL·단절 시 L1 전체 플러시·노출 창 명시(`02-core-contracts.md` §6) |
| **DB & Cache** | `ring-db`, `ring-cache` | **PostgreSQL** 및 **SQLite/LibSQL** 드라이버 어댑터(`#[table]` 체이닝 + `sql!` 매크로), **L1 인메모리(`Moka` 계열) + L2 `Redis/Valkey`** Tiered 캐시 어댑터(Singleflight·태그 무효화) |
| **Storage · Jobs · Notify** | `ring-storage`, `ring-jobs`, `ring-notify` | 로컬/S3/R2 멀티파트 업로드(크기·체크섬 검증 후 활성화), 지연·재시도·DLQ 작업 큐 및 Cron 스케줄러, 이메일·푸시(APNs/FCM)·HMAC 웹훅 어댑터 |
| **`lace` 웹 서빙** | `ring-adapter-lace` | `lace`가 산출한 정적 에셋 매니페스트(`LaceBundleManifest`), 라우트 메타데이터, `lace-web-ssr` 스트리밍 핸들러(`LaceSsrHandler`)를 받아 프로덕션 서빙 (`lace/docs/03-adapters-and-pipeline.md` §3 참조) |
