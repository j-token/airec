# PRD: airec — AI 작업 검증용 CLI 화면 녹화 도구

**문서 상태:** Draft
**작성일:** 2026-07-22
**제품명:** airec (가칭)
**대상 플랫폼:** Windows 10 (2004+) / Windows 11 — v0.1 기준. macOS/Linux는 v0.3 이후
**핵심 방향:** AI 에이전트가 자기 작업을 스스로 녹화해 증거로 남길 수 있는, 사람 개입 없는 CLI 화면 녹화기

---

## 1. 제품 요약

airec는 GUI 없이 명령어만으로 동작하는 Windows 화면 녹화 CLI다. AI 에이전트(Claude Code, computer-use 에이전트 등)가 컴퓨터를 조작해 작업을 수행한 뒤, 그 과정을 영상으로 남겨 GitHub PR·이슈 등에 첨부하는 것이 1차 사용 목적이다.

지금 AI 작업 검증은 두 가지 방식뿐이다. 사람이 직접 프로그램을 내려받아 실행해보거나, AI가 남긴 스크린샷 몇 장을 보는 것이다. 전자는 비용이 크고, 후자는 단편적이라 "동작하는 것 같긴 한데 정말 문제가 없나?"라는 의심을 지우지 못한다. 연속된 영상은 이 간극을 메운다. 클릭 한 번, 화면 전환 하나까지 그대로 보이기 때문이다.

기존 도구는 이 시나리오에 맞지 않는다. OBS는 GUI 전제라 에이전트가 다루기 어렵고, ffmpeg의 gdigrab은 창 가림·마우스 깜빡임 문제가 있으며 ddagrab은 모니터 단위 캡처만 지원해 "특정 앱 창만 정확히 녹화"가 불가능하다. 어느 쪽도 "지금 잘 녹화되고 있다"는 기계가 읽을 수 있는 상태 신호를 주지 않는다.

> GUI 녹화기를 CLI로 감싸지 말고, AI 에이전트를 1급 사용자로 놓은 녹화기를 처음부터 만든다. 사람이 읽는 화면 효과(클릭 이펙트)와 기계가 읽는 상태 신호(JSONL)를 모두 1급 기능으로 취급한다.

v0.1은 Windows에서 전체 화면·다중 모니터·단일/다중 윈도우 녹화를 MP4로 저장하고, 클릭 지점을 시각 이펙트로 합성하며, 녹화 상태를 구조화된 신호로 출력하는 것까지를 범위로 한다.

## 2. 배경과 문제 정의

### 2.1 스크린샷 기반 검증의 문제

- 시점 사이의 과정이 빠진다. "버튼을 눌렀다"와 "결과가 나왔다" 사이에 무슨 일이 있었는지 알 수 없다.
- AI가 스크린샷 찍을 타이밍을 놓치면 증거 자체가 없다.
- 리뷰어가 여러 장을 머릿속에서 이어 붙여야 하므로 인지 부하가 크다.

### 2.2 OBS Studio의 문제

- GUI 전제 설계다. CLI/WebSocket 제어가 있지만 초기 설정(장면, 소스 구성)은 GUI 없이는 사실상 불가능하다.
- 설치 용량과 의존성이 커서 CI·에이전트 환경에 배포하기 무겁다.
- "창 N개를 각각 파일로" 같은 구성을 명령어 한 줄로 표현할 수 없다.

### 2.3 ffmpeg (gdigrab / ddagrab)의 문제

- `gdigrab`: GDI 기반이라 CPU 사용률이 높고, 마우스 커서 깜빡임 문제가 알려져 있으며, 창 캡처 시 다른 창에 가려지면 가려진 화면이 찍힌다.
- `ddagrab`: Desktop Duplication 기반이라 빠르지만 모니터 단위 캡처만 지원한다. 특정 창 지정이 불가능하다.
- 둘 다 클릭/드래그 시각 이펙트가 없고, 진행 상태 출력이 사람용 로그 텍스트라 기계 파싱이 어렵다.
- 다중 모니터·다중 창 동시 녹화를 하려면 ffmpeg 프로세스를 여러 개 띄우고 직접 오케스트레이션해야 한다.

### 2.4 직접 구현이 필요한 이유

Windows Graphics Capture(WGC) API는 창 단위 캡처(가림 무관), GPU 가속, 커서 포함/제외를 모두 지원하지만, 이를 CLI 제품으로 노출하면서 (1) 다중 대상 동시 녹화, (2) 입력 이벤트 기반 클릭 이펙트 합성, (3) 기계가 읽는 상태 프로토콜을 묶어주는 도구는 없다. 이 세 가지 조합이 airec의 존재 이유다.

## 3. 제품 목표

### 3.1 핵심 목표

1. AI 에이전트가 사람 개입 없이 명령어만으로 녹화를 시작·종료·확인할 수 있다.
2. 전체 화면, 모니터별(각각 파일), 단일 윈도우, 다중 윈도우(각각 파일) 녹화를 지원한다.
3. 마우스 클릭·드래그·이동을 시각 이펙트로 영상에 합성해, 사람과 AI 모두 조작 내용을 놓치지 않게 한다.
4. 녹화 상태(시작됨, 진행 중, 프레임 수, 종료·저장 완료)를 구조화된 형식(JSONL)으로 stdout에 출력한다.
5. GitHub PR/이슈에 바로 첨부 가능한 포맷(MP4)으로 저장한다.

### 3.2 제품 철학

- 기계 우선, 사람 겸용: 모든 출력은 기본이 기계 파싱 가능해야 하고, 사람용 출력은 옵션이다.
- 단일 바이너리: 설치·의존성 없이 exe 하나를 내려받아 바로 실행한다.
- 실패를 크게 말한다: 녹화가 안 되고 있으면 조용히 넘어가지 않고 즉시 구조화된 에러로 알린다.
- 녹화는 명시적으로만: 사용자가(또는 에이전트가) 명령을 내렸을 때만 녹화한다. 백그라운드 상시 감시 기능은 만들지 않는다.

## 4. 비목표

v0.1~v0.3 공통으로 하지 않을 것:

- GUI 셸 출시 — 단, 영구 비목표는 아니다. Tauri v2 + Svelte 기반 GUI 컴패니언을 v0.4+에서 계획하며, 코어(airec-core)는 처음부터 CLI와 GUI가 공유하는 구조로 설계한다. v0.1~v0.3의 제품 접점은 CLI뿐이다
- 라이브 스트리밍(RTMP 등), 실시간 전송
- 영상 편집 기능(자르기, 자막, 장면 전환)
- 오디오 녹음(마이크·시스템 사운드) — 검증용 영상에는 화면과 입력 이펙트로 충분하다고 판단. 수요 확인 후 재검토
- GitHub 업로드 자동화 — 업로드는 `gh` CLI 등 기존 도구의 몫. airec는 파일 생성까지만 책임진다
- 키보드 입력 내용 표시(키 캐스팅) — v0.1~v0.2 제외. 비밀번호 노출 위험이 있어 설계에 별도 보안 검토 필요

v0.1/v0.2에서 하지 않을 것:

- macOS, Linux 지원 (v0.3)
- 화면 영역(region) 지정 녹화 (v0.3)

## 5. 타깃 사용자

### 5.1 1차: AI 코딩/컴퓨터 사용 에이전트

- 누구: Claude Code, computer-use 기반 자동화 에이전트
- 원하는 것: 명령어 한 줄로 녹화 시작, 작업 수행, 종료 후 파일 경로 확보
- 쓰는 이유: 자기 작업의 증거를 스스로 만들 수 있는 유일한 방법

### 5.2 2차: AI 에이전트를 운용하는 개발자

- 누구: 에이전트에게 GUI 작업을 시키고 결과를 리뷰하는 개발자
- 원하는 것: PR에 첨부된 영상을 보고 재실행 없이 동작을 확신
- 쓰는 이유: 검증 비용을 "프로그램 설치 후 직접 실행"에서 "영상 30초 시청"으로 줄임

### 5.3 3차: QA·데모 제작자

- 누구: 버그 재현 영상, 기능 데모를 만드는 사람
- 원하는 것: 스크립트로 반복 가능한 녹화
- 쓰는 이유: OBS 없이 CI·스크립트에서 녹화 자동화

## 6. 핵심 사용 시나리오

### 시나리오 1: AI 에이전트의 작업 검증 영상

AI 에이전트가 데스크톱 앱의 설정 화면을 수정하는 작업을 수행하고, 그 과정을 녹화해 PR에 첨부한다.

```sh
# 1. 녹화 시작 (백그라운드, 대상: "MyApp" 창)
airec start --window "MyApp" --out evidence.mp4 --json

# 2. 에이전트가 computer-use로 작업 수행...

# 3. 녹화 종료
airec stop --json
# → {"event":"saved","file":"evidence.mp4","duration_ms":42180,"frames":2530}

# 4. gh CLI 등으로 PR에 첨부
```

요구사항:

- `start`는 첫 프레임 캡처가 확인된 뒤에 성공을 보고해야 한다.
- `stop`은 파일 finalize(재생 가능 상태)까지 완료한 뒤 반환해야 한다.

성공 기준:

- 에이전트가 출력 JSON만 파싱해 녹화 성공 여부와 파일 경로를 판단할 수 있다.
- 저장된 MP4를 GitHub PR에 드래그하면 바로 재생된다.

### 시나리오 2: 다중 모니터 전체 녹화

모니터 3대 환경에서 전체 작업 과정을 모니터별 파일로 남긴다.

```sh
airec start --monitor all --out-dir ./rec --json
# → ./rec/monitor-1.mp4, ./rec/monitor-2.mp4, ./rec/monitor-3.mp4
```

요구사항:

- 모니터별 캡처는 병렬로 실행되고 파일이 각각 생성된다.
- 한 모니터의 캡처 실패가 다른 모니터 녹화를 중단시키지 않는다(부분 실패 보고).

성공 기준:

- `airec stop` 후 모니터 수만큼의 재생 가능한 MP4가 존재한다.

### 시나리오 3: 여러 앱 창을 정확히 지정해 녹화

브라우저와 터미널 두 창만 각각 녹화한다. 다른 창이 위를 덮어도 영향받지 않는다.

```sh
airec list windows --json     # 창 목록·HWND 확인
airec start --window "Chrome" --window "Windows Terminal" --out-dir ./rec --json
```

요구사항:

- 창 지정은 제목 부분 일치, HWND, 프로세스명 중 하나로 가능해야 한다.
- 지정이 모호하면(2개 이상 매칭) 에러와 함께 후보 목록을 반환한다.
- 창 캡처는 WGC 기반이므로 다른 창에 가려져도 대상 창 내용이 그대로 녹화된다.

성공 기준:

- 두 파일 각각에 해당 창의 내용만 담긴다.

### 시나리오 4: 클릭·드래그가 보이는 영상

에이전트(또는 사람)가 어디를 클릭했고 어디로 드래그했는지 영상만 보고 알 수 있다.

요구사항:

- 클릭 시 클릭 지점에 확산 링(ripple) 이펙트가 좌/우 버튼 구분되어 그려진다.
- 드래그 중에는 시작점→현재점 궤적이 표시된다.
- 이펙트는 영상에 직접 합성되어, 어떤 플레이어에서 봐도 보인다.

성공 기준:

- 영상을 1배속으로 볼 때 모든 클릭 지점을 놓치지 않고 식별할 수 있다.

## 7. 제품 범위

### 7.1 v0.1 MVP 범위

실제로 시나리오 1~4를 end-to-end로 수행할 수 있는 최소 제품.

포함:

- `airec list monitors|windows` — 대상 열거 (JSON)
- `airec start` / `airec stop` / `airec status` — 세션 제어
- `airec record` — 포그라운드 단발 녹화 (`--duration` 필수 또는 Ctrl+C)
- 전체 화면(주 모니터), 특정 모니터, `--monitor all`(모니터별 파일)
- 단일/다중 `--window` 지정 (제목 부분 일치 / HWND / 프로세스명)
- MP4(H.264, 하드웨어 인코딩) 출력
- OS 커서 캡처 포함/제외 옵션
- 클릭 이펙트(좌/우 버튼 링) 실시간 합성
- `--json` 모드: 상태 이벤트 JSONL 출력 (started / heartbeat / saved / error)
- 입력 이벤트 로그 sidecar(JSONL) 저장 옵션

제외(이후 버전): 드래그 궤적·이동 트레일 이펙트, GIF 변환, 설정 파일, macOS/Linux.

#### 미해결 문제

- **문제**: 클릭 이펙트 실시간 합성이 v0.1 일정 내 구현이 무거울 경우의 축소선
- **영향**: v0.1 릴리즈 일정
- **후보**: (A) v0.1은 이벤트 로그만 남기고 이펙트는 v0.2 / (B) 일정을 늘려 v0.1에 포함
- **결정 필요 시점**: v0.1 구현 착수 전 (현재 권장: B — 이펙트는 이 제품의 핵심 차별점)

### 7.2 v0.2 범위

- 드래그 궤적, 마우스 이동 트레일 이펙트
- 이펙트 스타일 옵션(색상, 크기, 지속시간)
- GIF/WebM 내보내기(`airec convert` 또는 `--format`)
- 설정 파일(`airec.toml`) 지원
- 창 이동·리사이즈 추적 개선, 창 닫힘 시 우아한 종료 보장 강화
- Claude Code용 스킬(`SKILL.md`) 패키지 동봉 — 에이전트가 airec 사용법을 즉시 아는 상태로 시작. 스킬에는 `--on-failure` 정책 선택 기준(증거 일부라도 확보 = continue / 완전한 증거만 유효 = abort)과 `stop_reason`으로 의도된 종료·비의도 종료를 판정하는 방법을 반드시 명시한다

### 7.3 v0.3 범위

- macOS 지원: 초회 실행(콜드 스타트) 시 화면 기록 권한(TCC) 요청 플로우 — `airec doctor`로 권한 상태 진단, 권한 없으면 안내 메시지와 함께 구조화된 에러 반환
- Linux 지원: Wayland(xdg-desktop-portal/PipeWire) 우선, 동일한 콜드 스타트 권한 플로우
- 화면 영역(region) 지정 녹화
- 키보드 입력 표시(보안 설계 선행 조건)

## 8. 기능 요구사항

### FR-001: 캡처 대상 열거

제품은 CLI에서 현재 연결된 모니터와 열려 있는 최상위 창 목록을 제공해야 한다.

요구사항:

- `airec list monitors --json`: 인덱스, 이름, 해상도, 주 모니터 여부, 배율(DPI)
- `airec list windows --json`: HWND, 창 제목, 프로세스명, PID, 크기, 최소화 여부
- 캡처 불가능한 창(0×0, 비가시 시스템 창)은 기본 제외

수용 기준:

- 모니터 2대 환경에서 `list monitors`가 2개 항목을 정확한 해상도와 함께 반환한다.
- `list windows` 출력의 HWND를 그대로 `--window-handle`에 넘기면 해당 창이 녹화된다.

### FR-002: 전체 화면(모니터) 녹화

제품은 주 모니터 또는 지정 모니터의 전체 화면을 녹화해야 한다.

요구사항:

- 대상 미지정 시 주 모니터를 기본값으로 한다.
- `--monitor <index>`로 특정 모니터 지정

수용 기준:

- `airec record --duration 10s --out a.mp4` 실행 후 주 모니터 10초 분량의 재생 가능한 MP4가 생성된다.
- 결과 영상 해상도가 해당 모니터의 물리 해상도와 일치한다.

### FR-003: 다중 모니터 동시 녹화

제품은 모든(또는 선택한 여러) 모니터를 동시에 녹화하고 모니터별 파일로 저장해야 한다.

요구사항:

- `--monitor all` 또는 `--monitor 1 --monitor 2`
- 출력은 `--out-dir` 아래 `monitor-<index>.mp4` (또는 `--out` 템플릿)
- 모니터별 캡처·인코딩 파이프라인은 독립 스레드로 병렬 실행
- 실패 정책 옵션 `--on-failure <continue|abort>` (기본 `continue`, 다중 창 녹화에도 동일 적용):
  - `continue`: 실패한 대상만 중단하고 나머지는 계속. 종료 시 `PARTIAL_FAILURE`로 성공/실패 내역을 모두 보고
  - `abort`: 한 대상이라도 실패하면 전체 세션을 즉시 finalize하고 실패로 종료. 이때도 나머지 대상의 그 시점까지 영상은 재생 가능하게 저장

수용 기준:

- 모니터 3대 환경에서 `--monitor all`로 3개 파일이 생성되고 각 파일 길이 차이가 1초 이내다.
- `continue`(기본): 1개 모니터 캡처가 실패해도 나머지 파일은 정상 저장되고, 실패는 `error` 이벤트와 `PARTIAL_FAILURE`(종료 코드 6)로 보고된다.
- `abort`: 1개 대상 실패 시 전체 세션이 finalize되어 종료되고, 종료 사유가 `aborted_on_failure`로, 원인 대상이 이벤트에 명시된다.

### FR-004: 단일 윈도우 녹화

제품은 지정한 앱 창 하나만 녹화해야 한다.

요구사항:

- 지정 방법: `--window <제목 부분 일치>`, `--window-handle <HWND>`, `--process <프로세스명>`
- 제목 부분 일치 결과가 2개 이상이면 에러 `AMBIGUOUS_TARGET`과 후보 목록 반환
- WGC 기반: 대상 창이 다른 창에 가려져도 대상 창 내용이 녹화된다
- 녹화 중 창 이동·리사이즈에 대응한다 (리사이즈 시 처리 방식은 10장 참고)
- 창이 닫히면 그 시점까지를 finalize하고 `window_closed` 사유와 함께 정상 종료한다

수용 기준:

- 메모장을 다른 창으로 완전히 덮은 상태에서 녹화해도 메모장 내용이 영상에 담긴다.
- 녹화 중 대상 창을 닫으면 프로세스가 0이 아닌 대기 없이 종료되고, 그때까지의 영상이 재생 가능하다.

### FR-005: 다중 윈도우 동시 녹화

제품은 여러 창을 동시에 각각의 파일로 녹화해야 한다.

요구사항:

- `--window`를 복수 지정 가능
- 파일명: `--out-dir` 아래 `<sanitized-제목>-<HWND>.mp4` 기본, 템플릿 지정 가능
- 창별 파이프라인 독립 실행, 부분 실패 허용(FR-003과 동일 원칙)

수용 기준:

- 창 2개 지정 시 2개 파일이 생성되고 각각 해당 창 내용만 담긴다.
- 그 중 1개 창을 녹화 중 닫아도 나머지 창 녹화는 계속된다.

### FR-006: 세션 제어 (start / stop / status)

제품은 녹화를 백그라운드 세션으로 시작하고 별도 명령으로 종료·조회할 수 있어야 한다.

요구사항:

- `airec start ...`: 백그라운드 프로세스로 detach. 첫 프레임 캡처 확인 후 `started` 이벤트를 출력하고 반환한다. 세션 ID 발급.
- `airec stop [--session <id>]`: 인코더 finalize 완료 후 `saved` 이벤트(파일 경로 포함) 출력. 세션 미지정 시 유일한 활성 세션을 대상으로 하고, 복수면 에러.
- `airec status --json`: 활성 세션 목록, 각 대상별 경과 시간·프레임 수·드롭 수.
- 제어 채널: 세션 프로세스는 named pipe(`\\.\pipe\airec-<session-id>`)를 연다.
- `--max-duration <시간>` (기본 30분): 상한 도달 시 자동 finalize. stop을 잊은 세션이 디스크를 무한정 채우는 것을 방지.
- 세션 프로세스 비정상 종료 대비: `record`/`start` 모두 fragmented MP4로 기록해, 크래시 시에도 그 시점까지의 영상이 재생 가능해야 한다.

수용 기준:

- `start` 반환 직후 `status`가 해당 세션을 `recording` 상태로 보여준다.
- `stop` 반환 시점에 파일이 존재하고 즉시 재생 가능하다.
- 세션 프로세스를 강제 kill해도 그 시점까지의 파일이 재생 가능하다.
- 활성 세션이 없는데 `stop`을 호출하면 에러 코드 `NO_ACTIVE_SESSION`을 반환한다.

### FR-007: 기계가 읽는 상태 신호

제품은 `--json` 지정 시 모든 상태를 JSONL(줄 단위 JSON)로 stdout에 출력해야 한다.

요구사항:

- 이벤트 종류: `started`, `heartbeat`(기본 5초 간격: 경과·프레임·드롭 수), `target_lost`, `saved`, `error`
- 모든 이벤트에 `ts`(ISO 8601), `session`, `target` 필드 포함
- 모든 종료(`saved` 이벤트, 세션 종료)에 `stop_reason` 필드로 의도된 종료와 의도치 않은 종료를 명확히 구분:
  - 의도된 종료: `requested`(stop 명령), `duration_limit`(--duration 도달), `max_duration`(--max-duration 상한)
  - 의도치 않은 종료: `target_lost`(창 닫힘), `error`(파이프라인 실패), `aborted_on_failure`(다른 대상 실패로 abort 정책 발동)
  - AI 에이전트는 `stop_reason`이 의도된 종료군인지 확인하는 것만으로 "정상적으로 끝났는가"를 판정할 수 있어야 한다
- `--json` 미지정 시 사람용 한 줄 로그 출력 (예: `● REC monitor-1 00:00:12  360 frames`)
- 진단 로그는 stderr, 이벤트는 stdout으로 분리

이벤트 예시:

```json
{"event":"started","ts":"2026-07-22T10:00:00.120Z","session":"a1b2","targets":[{"type":"window","title":"MyApp","file":"evidence.mp4"}]}
{"event":"heartbeat","ts":"2026-07-22T10:00:05.120Z","session":"a1b2","target":"evidence.mp4","elapsed_ms":5000,"frames":300,"dropped":0}
{"event":"saved","ts":"2026-07-22T10:00:42.300Z","session":"a1b2","target":"evidence.mp4","file":"C:\\work\\evidence.mp4","stop_reason":"requested","duration_ms":42180,"frames":2530,"size_bytes":8412345}
```

수용 기준:

- `--json` 출력의 각 줄이 유효한 JSON으로 파싱된다.
- 첫 프레임이 30초(설정 가능한 타임아웃) 내에 도착하지 않으면 `error` 이벤트 후 비정상 종료한다. 조용한 무한 대기는 없다.

### FR-008: 커서 캡처

제품은 OS 마우스 커서를 영상에 포함하거나 제외할 수 있어야 한다.

요구사항:

- 기본값 포함, `--no-cursor`로 제외 (WGC `CursorCaptureSettings` 사용)

수용 기준:

- 기본 녹화 영상에 커서가 보이고, `--no-cursor` 시 보이지 않는다.

### FR-009: 클릭 이펙트 합성

제품은 마우스 클릭을 시각 이펙트로 영상 프레임에 직접 합성해야 한다.

요구사항:

- 좌클릭/우클릭을 색으로 구분한 확산 링(ripple)을 클릭 좌표에 약 0.5초간 렌더링
- 더블클릭은 이중 링으로 구분
- 이펙트는 인코딩 전 프레임에 합성한다(후처리 불필요, 결과물은 단일 MP4)
- 창 녹화 시 스크린 좌표를 창 클라이언트 좌표로 변환해 그린다. 창 밖 클릭은 그리지 않는다
- `--no-effects`로 비활성화 가능
- 입력 수집은 저수준 마우스 훅(WH_MOUSE_LL) 기반, 키보드는 수집하지 않는다

수용 기준:

- 녹화 중 임의 지점 클릭 시 결과 영상의 해당 프레임(±100ms)에 링 이펙트가 그 좌표에 나타난다.
- 창 녹화 중 창을 이동한 뒤 클릭해도 이펙트가 올바른 상대 좌표에 그려진다.
- 대상 창 밖을 클릭하면 해당 창 영상에는 이펙트가 나타나지 않는다.

### FR-010: 입력 이벤트 로그 (sidecar)

제품은 `--event-log` 지정 시 마우스 입력 이벤트 타임라인을 JSONL 파일로 저장해야 한다.

요구사항:

- 이벤트: `click`(button, x, y), `drag_start`/`drag_end`, `move`(스로틀링, 예: 100ms 간격)
- 각 이벤트에 영상 기준 타임스탬프(`t_ms`) 포함 — AI가 "몇 초에 어디를 클릭했는지" 텍스트로 교차 검증 가능
- 키보드 이벤트는 기록하지 않는다 (v0.3에서 보안 설계와 함께 재검토)

수용 기준:

- 영상 t초의 클릭이 로그에서 `t_ms ± 100ms` 항목으로 발견된다.

### FR-011: 출력 포맷

제품은 H.264 + AAC-없음(비디오 전용) MP4를 기본 출력으로 해야 한다.

요구사항:

- Media Foundation 하드웨어 인코더 우선, 실패 시 소프트웨어 인코더 폴백(폴백 발생은 stderr에 알림)
- 기본 프레임레이트 30fps (`--fps`로 조절, 최대 60)
- 픽셀 포맷은 일반 플레이어·GitHub 웹 재생 호환(yuv420p 상당) 보장
- 파일 크기 가이드: 1080p/30fps 기준 분당 목표 ≤ 20MB (초기 가정, 품질 옵션 `--quality low|medium|high`)

수용 기준:

- 산출 MP4가 Windows 기본 플레이어, Chrome, GitHub PR 웹 플레이어에서 재생된다.

### FR-012: 종료 코드와 시그널 처리

제품은 스크립트에서 신뢰 가능한 종료 동작을 제공해야 한다.

요구사항:

- Ctrl+C(SIGINT 상당) 수신 시: finalize 후 정상 종료 (파일 손상 없음)
- 종료 코드: 0 성공 / 그 외는 13장 에러 모델의 분류를 따름

수용 기준:

- `record` 실행 중 Ctrl+C를 눌러도 재생 가능한 MP4가 남는다.

## 9. 비기능 요구사항

수치는 초기 가정이며 v0.1 구현 후 재보정한다.

| 항목 | 목표 |
|---|---|
| CPU 사용률 | 1080p/30fps 단일 대상 녹화 시 5% 이하 (하드웨어 인코딩 기준) |
| 동시 대상 | 최소 4개(모니터+창 합산)를 프레임 드롭률 1% 미만으로 |
| 시작 지연 | `start` 호출 → `started` 이벤트까지 2초 이내 |
| 메모리 | 대상당 상주 메모리 200MB 이하 |
| 안정성 | 프로세스 강제 종료 시에도 기록분 재생 가능 (fragmented MP4) |
| 디버깅성 | `--verbose` 시 stderr에 캡처·인코딩 파이프라인 진단 로그 |
| 이식성 | 캡처/입력 훅 계층을 플랫폼 trait으로 분리해 v0.3 macOS/Linux 구현이 코어를 건드리지 않게 함 |

## 10. 기술 아키텍처

```txt
airec CLI (clap)
  ├─ list / status / stop ─── named pipe ──┐
  └─ record / start                        │
        ↓                                  ↓
   Session Manager ◄──────────── Control Server (per-session named pipe)
        ↓ (대상별 1 파이프라인, 병렬)
   Capture Pipeline
        ├─ WGC Capture (windows-capture crate, Windows.Graphics.Capture)
        │     · 모니터/윈도우 대상, 커서 포함 옵션, 가림 무관
        ├─ Effect Compositor
        │     · Input Hook Thread(WH_MOUSE_LL) → 이벤트 큐
        │     · 프레임 타임스탬프에 맞춰 클릭 링을 프레임 버퍼에 합성
        │     · 창 대상이면 스크린→클라이언트 좌표 변환
        └─ Encoder (Media Foundation H.264, HW 우선 / fragmented MP4)
              ↓
         monitor-1.mp4 / myapp-1a2b.mp4 ... + events.jsonl
```

핵심 결정과 근거:

- **언어: Rust.** 단일 정적 바이너리 배포, WGC를 감싼 성숙한 `windows-capture` crate(v2.x)가 캡처+하드웨어 인코딩+모니터/윈도우 열거를 모두 제공. GC 없는 프레임 경로로 성능 예측 가능. Tauri v2가 Rust 기반이므로, v0.4+ GUI 컴패니언(Tauri v2 + Svelte)이 airec-core를 그대로 Tauri command로 노출할 수 있다.
- **캡처 API: Windows Graphics Capture.** 창 단위 캡처에서 가림 문제가 없는 유일한 공식 API. Desktop Duplication(DXGI)은 모니터 캡처 폴백으로만 고려.
- **이펙트는 실시간 합성.** 후처리 방식(원본 + 이벤트 로그 → 재인코딩)은 결과물이 나오기까지 추가 시간이 들고 임시 파일 관리가 필요하다. AI 워크플로우에서는 `stop` 즉시 최종 파일이 나오는 것이 중요하므로 인코딩 전 프레임 합성을 택한다. 이벤트 로그(FR-010)는 별도로 남겨 후처리 확장 여지를 유지한다.
- **세션 모델: detach된 세션 프로세스 + named pipe 제어.** AI 에이전트는 "시작 → 다른 작업 → 종료" 패턴을 쓰므로 start/stop 분리가 필수. 별도 상주 데몬은 두지 않고 세션 프로세스가 곧 서버가 된다.

repo 구조(안):

```txt
crates/
  airec-cli/        # 명령 파싱, 출력 포맷, 세션 클라이언트
  airec-core/       # Session Manager, 파이프라인 오케스트레이션, 이벤트 모델
  airec-capture/    # 캡처 trait + Windows(WGC) 구현  ← v0.3에서 mac/linux 구현 추가
  airec-input/      # 입력 훅 trait + Windows(WH_MOUSE_LL) 구현
  airec-effects/    # 이펙트 렌더러 (프레임 버퍼 합성)
apps/
  desktop/          # (v0.4+) Tauri v2 + Svelte GUI 컴패니언, airec-core를 command로 노출
skill/              # Claude Code용 SKILL.md (v0.2)
.agents/skills/     # 저장소 로컬 에이전트 스킬 (백엔드·프론트엔드 인계 등)
```

#### 미해결 문제

- **문제**: WGC의 캡처 노란 테두리(border) 처리. Windows 버전에 따라 `IsBorderRequired=false`가 지원되지 않을 수 있음
- **영향**: 구버전 Windows 10에서 결과 영상 품질(테두리 노출)
- **후보**: (A) 지원 버전에서만 테두리 제거, 미지원이면 경고 후 진행 / (B) 최소 지원 버전을 테두리 제거 가능 버전으로 상향
- **결정 필요 시점**: v0.1 구현 전 (권장: A)

- **문제**: 창 리사이즈 시 인코딩 해상도 처리. MP4 스트림 중간 해상도 변경은 플레이어 호환성이 나쁨
- **영향**: FR-004 구현 방식
- **후보**: (A) 시작 시점 해상도 고정, 이후 프레임은 letterbox/scale / (B) 리사이즈 시 파일 분할
- **결정 필요 시점**: v0.1 설계 전 (권장: A)

## 11. CLI 요구사항

```sh
# 열거
airec list monitors [--json]
airec list windows [--json]

# 단발 녹화 (포그라운드)
airec record [대상 옵션] --duration 30s --out demo.mp4 [--json]

# 세션 녹화 (백그라운드)
airec start [대상 옵션] --out evidence.mp4 [--json] [--event-log events.jsonl]
airec status [--json]
airec stop [--session <id>] [--json]

# 대상 옵션 (조합 가능, 복수 지정 가능)
--monitor <index|all>      --window <title-substring>
--window-handle <hwnd>     --process <name.exe>

# 공통 옵션
--out <file> | --out-dir <dir>    --fps <n=30>    --quality low|medium|high
--no-cursor    --no-effects    --max-duration <t=30m>    --verbose
--on-failure continue|abort   # 다중 대상 중 일부 실패 시 정책 (기본 continue)

# 진단
airec doctor [--json]     # 인코더 가용성, WGC 지원, (v0.3) OS 권한 상태
```

규칙:

- 모든 서브커맨드는 `--json`을 지원한다.
- 사람용 출력에서도 진행 표시는 한 줄 갱신으로 스크립트 로그를 오염시키지 않는다.
- `--out` 미지정 시 `./airec-<timestamp>/` 아래 자동 명명.

## 12. 설정 파일

v0.1은 설정 파일 없이 플래그만 지원한다. v0.2에서 `airec.toml`(현재 디렉터리 → 사용자 홈 순서 탐색)로 기본값 재정의를 지원한다:

```toml
[defaults]
fps = 30
quality = "medium"
effects = true

[effects]
click_color_left = "#FFD400"
click_color_right = "#00A2FF"
```

## 13. 에러 모델

모든 에러는 `--json` 모드에서 구조화되어 출력된다:

```json
{"event":"error","code":"TARGET_NOT_FOUND","message":"no window matches title 'MyAp'","data":{"query":"MyAp","candidates":[]}}
```

| code | 의미 | 종료 코드 |
|---|---|---|
| `TARGET_NOT_FOUND` | 모니터/창 매칭 실패 | 2 |
| `AMBIGUOUS_TARGET` | 창 지정이 2개 이상 매칭 (`data.candidates` 포함) | 2 |
| `CAPTURE_INIT_FAILED` | WGC 세션 생성 실패 | 3 |
| `ENCODER_UNAVAILABLE` | HW/SW 인코더 모두 사용 불가 | 3 |
| `FIRST_FRAME_TIMEOUT` | 시작 후 첫 프레임 미도착 | 3 |
| `NO_ACTIVE_SESSION` | stop/status 대상 세션 없음 | 4 |
| `SESSION_AMBIGUOUS` | stop 대상 세션이 복수 | 4 |
| `OUTPUT_IO_ERROR` | 파일 쓰기 실패(디스크 부족 등) | 5 |
| `PARTIAL_FAILURE` | 다중 대상 중 일부 실패, 나머지는 저장됨 (`--on-failure continue`) | 6 |
| `ABORTED_ON_FAILURE` | 대상 실패로 전체 세션 중단 (`--on-failure abort`), 원인 대상 포함 | 6 |
| `PERMISSION_DENIED` | (v0.3) OS 화면 기록 권한 없음 | 7 |
| `TARGET_LOST` | 첫 프레임 이후 대상 창이 사라졌지만 인코더 finalize가 성공함 | 8 |

원칙: 부분 실패(`PARTIAL_FAILURE`)는 성공한 파일 목록과 실패한 대상·사유를 모두 `data`에 담는다.

## 14. 보안 정책

화면 녹화는 본질적으로 민감 정보(토큰, 비밀번호 화면, 개인정보)를 담을 수 있다.

- 녹화는 명시적 명령으로만 시작된다. 자동 시작, 상시 감시, 원격 트리거 기능을 두지 않는다.
- 네트워크 기능 없음: airec는 어떤 데이터도 외부로 전송하지 않는다. 업로드는 사용자의 별도 행동이다.
- 키보드 입력을 캡처하지 않는다(마우스 훅만 사용). 키 표시 기능은 마스킹 설계가 끝나기 전에는 출시하지 않는다.
- 입력 이벤트 로그는 좌표와 버튼만 기록하며 옵트인(`--event-log`)이다.
- 문서(README·스킬)에 "녹화물에 민감 정보가 포함될 수 있으니 공유 전 확인" 경고를 명시한다.
- UAC 보안 데스크톱, DRM 보호 콘텐츠는 OS가 캡처를 차단하며 airec는 이를 우회하지 않는다(검은 화면으로 기록됨을 문서화).

## 15. 개발자 경험

- 배포: GitHub Releases 단일 exe + `winget`/`scoop`(v0.2 검토). 설치 스크립트 한 줄로 에이전트가 자가 설치 가능.
- `airec doctor`: 환경 문제(인코더 없음, OS 버전 미달)를 실행 전에 진단.
- Claude Code 스킬(v0.2): "작업을 녹화해서 PR에 첨부해줘" 같은 지시에 에이전트가 airec 명령 시퀀스를 바로 구성할 수 있도록 사용 패턴·에러 대응을 담은 `SKILL.md` 제공.
- `--help`가 곧 레퍼런스가 되도록 각 옵션에 예시 포함.

## 16. 성공 지표

- AI 에이전트가 문서만 보고(사람 개입 없이) 녹화 시작→종료→파일 확인을 성공하는 비율 ≥ 95% (내부 에이전트 테스트 기준)
- `start`→`started` 지연 p95 ≤ 2초
- 1시간 연속 녹화 시 크래시 0, 프레임 드롭률 < 1%
- 생성된 MP4의 GitHub 웹 플레이어 재생 성공률 100%

## 17. 릴리즈 계획

| 버전 | 내용 | 게이트 |
|---|---|---|
| v0.1.0 | 8장 FR 전체 (Windows) | 21장 수용 기준 통과 + 시나리오 1~4 e2e 데모 |
| v0.2.0 | 드래그/트레일 이펙트, GIF, 설정 파일, Claude 스킬 | 에이전트 실사용 피드백 반영 |
| v0.3.0 | macOS → Linux, region 녹화 | 플랫폼별 권한 콜드 스타트 플로우 검증 |
| v0.4.0+ | Tauri v2 + Svelte GUI 컴패니언 (airec-core 공유) | CLI 기능과 동등성, 백엔드·프론트엔드 인계 스킬 절차 준수 |

## 18. 주요 리스크

### 18.1 클릭 이펙트-프레임 동기화 정밀도

문제:

- 마우스 훅 이벤트 시각과 WGC 프레임 도착 시각은 서로 다른 클록·스레드에서 발생한다. 동기화가 어긋나면 "클릭했는데 링이 다른 위치/시점에 뜨는" 영상이 되어 검증 신뢰도를 해친다.

대응:

- 이벤트와 프레임 모두 QPC(QueryPerformanceCounter) 단일 클록으로 타임스탬프.
- 수용 기준을 ±100ms로 명시하고 자동화 테스트(합성 클릭 → 프레임 분석)로 회귀 방지.

### 18.2 창 캡처의 경계 조건

문제:

- 창 최소화 시 WGC는 프레임 업데이트를 멈춘다. 창 이동·리사이즈·DPI 변경·닫힘 등 상태 전이가 많다.

대응:

- 최소화 시 마지막 프레임을 유지(freeze)하고 `heartbeat`에 `minimized:true` 표시.
- 리사이즈는 10장 결정(고정 해상도 + letterbox)으로 흡수.
- 상태 전이별 통합 테스트 목록을 21장 수용 기준에 포함.

### 18.3 하드웨어 인코더 다양성

문제:

- NVIDIA/AMD/Intel/가상머신(인코더 없음) 환경별로 Media Foundation 동작이 다르다. CI·클라우드 VM은 GPU가 없는 경우가 많다 — 정확히 AI 에이전트가 도는 환경이다.

대응:

- SW 인코더 폴백을 v0.1 필수로 하고, `doctor`가 어떤 인코더가 선택될지 미리 보고.
- GPU 없는 VM을 CI 테스트 매트릭스에 포함.

### 18.4 다중 대상 병렬 녹화의 자원 경합

문제:

- 4개 이상 대상 동시 녹화 시 인코더 세션 수 제한(특히 NVENC 세션 제한)이나 대역폭 문제로 드롭 발생 가능.

대응:

- 대상 수가 임계치를 넘으면 자동으로 일부를 SW 인코딩으로 배분하거나 경고.
- `heartbeat`의 `dropped` 카운트로 드롭을 숨기지 않고 노출.

## 19. 섹션별 미해결 문제 인덱스

| 섹션 | 우선순위 | 미해결 문제 | 결정 필요 시점 |
|---|---|---|---|
| `7.1 v0.1 MVP 범위` | 높음 | 클릭 이펙트의 v0.1 포함 확정 (권장: 포함) | 구현 착수 전 |
| `10. 기술 아키텍처` | 중간 | WGC 노란 테두리 — 최소 지원 OS 버전 정책 | v0.1 구현 전 |
| `10. 기술 아키텍처` | 중간 | 창 리사이즈 시 해상도 고정+letterbox 확정 | v0.1 설계 전 |

## 20. 의사결정

| 항목 | 결정 |
|---|---|
| 구현 언어 | Rust (단일 바이너리) |
| GUI 컴패니언 스택 | Tauri v2 + Svelte (v0.4+, airec-core를 Tauri command로 공유) |
| 캡처 API | Windows Graphics Capture (`windows-capture` crate) |
| 인코딩 | Media Foundation H.264, HW 우선 + SW 폴백, fragmented MP4 |
| 기본 출력 포맷 | MP4 (H.264, 비디오 전용) |
| 클릭 이펙트 | 실시간 합성 (후처리 아님) + 이벤트 로그 sidecar |
| 입력 수집 | WH_MOUSE_LL 마우스 훅만, 키보드 제외 |
| 세션 제어 | detach 세션 프로세스 + named pipe, 상주 데몬 없음 |
| 상태 프로토콜 | stdout JSONL (`--json`), 진단은 stderr |
| 다중 대상 실패 정책 | `--on-failure continue`(기본) / `abort` 선택제 |
| 종료 사유 구분 | 모든 종료에 `stop_reason` 필수 — 의도(requested/duration_limit/max_duration) vs 비의도(target_lost/error/aborted_on_failure) |
| 오디오 | v0.1~v0.3 비목표 |
| 플랫폼 순서 | Windows → macOS → Linux |

## 21. 상세 수용 기준

v0.1 릴리즈 게이트 체크리스트.

캡처:

- [ ] 주 모니터 / 지정 모니터 / `--monitor all` 각각 재생 가능한 MP4 생성
- [ ] 창 제목 부분 일치·HWND·프로세스명 지정으로 창 녹화 성공
- [ ] 모호한 창 지정 시 `AMBIGUOUS_TARGET` + 후보 목록 반환
- [ ] 가려진 창의 내용이 올바르게 녹화됨
- [ ] 창 2개 + 모니터 1개 동시 녹화(3파일) 시 드롭률 < 1%
- [ ] 녹화 중 대상 창 닫힘 → 해당 파일 finalize + `target_lost` 이벤트, 다른 대상 지속

이펙트:

- [ ] 좌/우 클릭 링이 올바른 좌표·시점(±100ms)에 합성됨
- [ ] 창 이동 후 클릭도 올바른 상대 좌표에 표시
- [ ] `--no-effects`, `--no-cursor` 동작

세션·신호:

- [ ] `start`→`started` p95 ≤ 2초, 첫 프레임 타임아웃 시 `FIRST_FRAME_TIMEOUT`
- [ ] `stop` 반환 시점에 파일 재생 가능
- [ ] 세션 프로세스 강제 kill 후에도 기록분 재생 가능
- [ ] `--json` 전체 이벤트가 줄 단위 유효 JSON
- [ ] Ctrl+C로 중단해도 파일 정상
- [ ] 모든 종료 경로(stop, duration, 창 닫힘, 실패, abort)에서 `stop_reason`이 올바르게 구분되어 기록됨
- [ ] `--on-failure continue`: 일부 실패 시 나머지 저장 + 종료 코드 6 / `abort`: 전체 finalize 후 실패 종료

호환·환경:

- [ ] 산출물이 Windows 기본 플레이어·Chrome·GitHub 웹 플레이어에서 재생
- [ ] GPU 없는 VM에서 SW 폴백으로 녹화 성공, `doctor`가 인코더 상태 보고
- [ ] 1시간 연속 녹화 무크래시

## 22. 예시: AI 에이전트 통합 패턴

Claude Code가 데스크톱 앱 검증 작업에서 사용하는 전형적 시퀀스:

```sh
# 0. 환경 확인
airec doctor --json || exit 1

# 1. 대상 확인
airec list windows --json | jq '.[] | select(.title | contains("MyApp"))'

# 2. 녹화 시작
airec start --window "MyApp" --out evidence.mp4 --event-log events.jsonl --json

# 3. (computer-use로 작업 수행)

# 4. 종료 및 결과 확보
airec stop --json
# {"event":"saved","file":"C:\\work\\evidence.mp4","duration_ms":42180,...}

# 5. 업로드는 기존 도구로
gh pr comment 123 --body "검증 영상 첨부" # + 파일 첨부
```

## 23. 최종 요약

airec v0.1은 다음을 하는 Windows CLI 단일 바이너리다:

1. 모니터·창을 열거하고, 전체 화면 / 모니터별 / 단일·다중 창을 각각의 MP4로 동시 녹화한다 (WGC 기반, 가림 무관).
2. 클릭 지점을 링 이펙트로 영상에 직접 합성해 사람과 AI가 조작을 눈으로 확인할 수 있게 한다.
3. `start`/`stop`/`status`와 stdout JSONL 이벤트로, AI 에이전트가 사람 없이 녹화 전 과정을 제어·검증할 수 있게 한다.

하지 않는 것: GUI, 스트리밍, 편집, 오디오, 자동 업로드. macOS/Linux는 권한 콜드 스타트 플로우와 함께 v0.3에서 다룬다.
