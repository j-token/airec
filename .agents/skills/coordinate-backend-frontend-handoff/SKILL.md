---
name: coordinate-backend-frontend-handoff
description: Split a user-facing GitHub feature or bug into parent, Rust backend, frontend, and optional integration sub-issues; define the backend-to-frontend contract; and link backend pull requests without closing the parent early. Use when planning screen-recorder-cli (airec) work that crosses the Rust core (capture, encoding, session, CLI) or Tauri v2 commands and the Svelte UI, store, or E2E work, or when drafting the related issues and PR body.
---

# 백엔드·프론트엔드 인계 조율

사용자 기능의 완료 상태와 개별 구현 PR의 완료 상태를 분리한다. 백엔드 계약을 먼저 안정화하고 프론트엔드가 명시적인 후속 이슈로 이어받게 한다.

## 이슈 구조 만들기

다음 구조를 기본으로 사용한다.

```text
상위 기능 이슈
├── 백엔드 서브 이슈
│   └── Rust PR 병합 시 백엔드 이슈만 종료
├── 프론트엔드 서브 이슈
│   └── 백엔드 계약 확정 후 구현
└── 통합 서브 이슈(필요한 경우)
    └── 실제 환경 연동과 전체 E2E 검증
```

1. 상위 이슈에 사용자 결과와 최종 수용 기준을 둔다.
2. Rust core(airec-core/capture/input/effects), CLI 인터페이스, Tauri command, DTO, 세션·저장 처리와 Rust 테스트를 백엔드 서브 이슈에 둔다.
3. API 호출, store, Svelte UI 상태와 데스크톱 E2E를 프론트엔드 서브 이슈에 둔다.
4. 실제 하드웨어(다중 모니터, GPU 인코더)나 여러 계층을 함께 검증해야 할 때만 통합 서브 이슈를 추가한다.
5. GitHub가 지원하면 체크리스트 링크 대신 실제 sub-issue 관계를 사용한다.
6. 이미 독립적인 백엔드 작업 이슈가 있으면 내용이 같은 서브 이슈를 중복 생성하지 않는다.

## 백엔드 인계 계약 작성하기

백엔드 서브 이슈의 완료 조건에 다음 항목을 명시한다.

- CLI 서브커맨드 또는 Tauri command 이름과 입력·출력 DTO(JSONL 이벤트 스키마 포함)
- `loading`, `unsupported`, `error` 상태와 오류 코드(PRD 13장 에러 모델과 일치)
- nullable 값, 미보고 값과 숫자 `0`의 차이
- `crates/` 공개 API, `apps/desktop/src-tauri`의 command·capabilities 등록 범위
- Tauri command 등록 검증 스크립트가 있으면 통과
- Rust 단위·통합 테스트
- 프론트엔드용 fixture 또는 mock 응답
- 기존 command·CLI 플래그와의 호환성 및 마이그레이션 여부
- 개인정보·비밀정보·영속화·로그 경계(녹화물·입력 이벤트 로그 포함)
- 프론트엔드에서 사용할 호출 예시

백엔드가 반환할 수 없는 상태를 프론트엔드가 추정하게 만들지 않는다. 계약이 확정되기 전 프론트엔드 병렬 작업이 필요하면 fixture를 먼저 합의하고 양쪽 이슈에 같은 계약을 기록한다.

## PR 연결하기

백엔드 PR 본문에는 다음 관계를 사용한다.

```md
Closes #<백엔드-서브-이슈>
Part of #<상위-이슈>

Frontend follow-up: #<프론트엔드-서브-이슈>
```

- `Closes`는 이 PR이 실제로 완료하는 백엔드 서브 이슈에만 사용한다.
- 상위 이슈에는 `Part of` 또는 `Related to`만 사용한다.
- 프론트엔드 작업이 남아 있으면 상위 이슈를 닫지 않는다.
- 프론트엔드 후속 이슈가 아직 없으면 PR을 올리기 전에 생성하거나, 생성 권한이 없으면 누락을 명시한다.
- 상위 이슈는 프론트엔드와 필요한 통합 검증까지 완료된 뒤 닫는다.

## 인계 전 확인하기

- 백엔드 서브 이슈의 수용 기준이 테스트로 확인됐는가?
- 프론트엔드가 Rust 내부 구현을 읽지 않고 DTO와 예시만으로 작업할 수 있는가?
- unsupported, stale, partial failure와 재시도 동작이 정의됐는가?
- fixture가 실제 DTO와 함께 갱신됐는가?
- PR의 `Closes`가 상위 이슈를 가리키지 않는가?
- 프론트엔드 및 통합 후속 이슈 링크가 유효한가?

하나라도 충족하지 않으면 백엔드 작업을 완료로 표시하지 말고 누락 항목을 백엔드 서브 이슈에 남긴다.
