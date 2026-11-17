# 03. 파이프라인과 호환 어댑터 (ring)

> 상태: 코어 `ref` + 호환 어댑터 분리 원칙은 **확정**, 도메인별 어댑터 세부는 **제안** · 기준일: 2026-11-17

`ring`의 모든 백엔드 도메인(DB, Cache, MSA, WS/WT)은 **내부 고성능 코어 파이프라인**과 **외부·인프라 호환 어댑터**의 2단 구조로 동작합니다(`hyperish/docs` `02-core-contracts.md` §2~§7).

## 1. 백엔드 요청·작업 파이프라인

1. **어댑터 수신 및 컨텍스트 생성**: 외부 어댑터(`ring-adapter-http`, `ring-adapter-ws` 등) 또는 내부 MSA 채널이 요청을 받아 상관관계 ID(`requestId`)와 구조적 취소 토큰(`CancellationToken`)을 가진 `RequestCtx`로 변환합니다.
2. **접근 권한·멱등성 게이트**:
   - 보호 대상은 요청 시점마다 소유 계층이 권한과 철회 여부를 검사합니다(`02-core-contracts.md` §6).
   - 생성·상태 변경 요청의 `Idempotency-Key`는 `ring-cache`를 통해 중복 실행과 동시 처리(`409 request_in_progress`)를 제어합니다.
3. **도메인 실행 및 커밋/보상**:
   - `ring-db` 트랜잭션과 `ring-jobs` 워커는 취소 가능 지점과 커밋 지점을 명시하며, 커밋 이후 실패 시 보상 절차 또는 재조정(reconciliation) 상태를 기록합니다(`02-core-contracts.md` §3).
4. **응답 변환**: 내부 구조화 오류(`Domain` / `Infra` / `Internal`)는 어댑터 경계에서 내부 스택 노출 없이 외부 규약의 오류 형식으로 변환됩니다.

## 2. 호환 어댑터 구성

| 영역 | 코어 추상화 (`ring-*`) | 호환 어댑터 역할 |
|---|---|---|
| **외부 API** | `hyper-api` + `ring-core` | **HTTP/JSON 어댑터**(`hyperish/docs` `03-api-protocol.md` 준수: 커서 페이지네이션, `expectedVersion` 낙관적 잠금, CSRF·쿠키/Bearer 인증, `RateLimit-*` 헤더) |
| **실시간 (WS / WT)** | `ring-realtime` (`seq`, 룸 상태, 중복 제거) | **WebSocket 어댑터**(`hyperish/docs` `04-realtime-communication.md` 준수: `?ticket=` 단기 일회용 티켓, `seq` 재연결 복구, 휴면 자동 응답) 및 **WebTransport(HTTP/3 QUIC) 어댑터** |
| **MSA 내부 통신** | `ring-msa` + `hyper-serde` | 서비스 간 고성능 바이너리/제로카피 RPC 및 메시지 브로커 어댑터 |
| **DB / Cache / Storage** | `ring-db`, `ring-cache`, `ring-storage` | 관계형/임베디드 DB 드라이버(Postgres, SQLite/D1 등), 인메모리/분산 캐시, S3/R2 호환 대용량 오브젝트 스토리지(무결성 검증 후 활성화) 어댑터 |
