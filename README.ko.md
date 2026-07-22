# airec

[English](README.md)

AI 에이전트가 자기 작업을 증거로 남기기 위한 윈도우 CLI 화면 녹화기입니다.

에이전트가 컴퓨터를 조작해 작업을 끝냈을 때, 그게 실제로 잘 된 건지 확인해야 합니다. 스크린샷은 순간만 보여줍니다. airec는 작업 과정 전체를 MP4로 녹화하므로, 프로그램을 다시 설치하고 직접 눌러보는 대신 PR에 영상을 붙여놓고 보면 됩니다.

## 기능

- 모니터 하나, 여러 모니터 동시, 또는 특정 앱 창을 각각 별도 파일로 녹화합니다
- Windows Graphics Capture를 쓰기 때문에 대상 창이 다른 창에 가려져 있어도 그 창의 내용이 녹화됩니다
- 클릭, 드래그, 포인터 이동 효과를 프레임에 직접 합성해서 재생할 때 마우스가 보입니다
- 녹화 상태를 stdout에 JSONL로 출력하므로, 스크립트가 녹화 시작 여부와 진행 상황, 종료 사유를 판단할 수 있습니다
- 녹화본을 GIF로 변환합니다

GUI, 오디오 녹음, 네트워크 접근, 업로드 기능, 키보드 후킹, 상주 데몬은 없습니다.

> 녹화본에는 비밀번호, 토큰, 개인정보가 담길 수 있습니다. 공유하기 전에 한 번 확인하십시오. UAC 보안 데스크톱과 DRM 보호 콘텐츠는 우회하지 않으며 검은 화면으로 기록됩니다.

## 설치

Windows 10 버전 2004 이상 또는 Windows 11, x86_64에서 동작합니다.

```powershell
irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex
```

최신 릴리즈를 받아 SHA256을 확인하고, `airec.exe`를 `%LOCALAPPDATA%\airec\bin`에 두고 그 경로를 사용자 PATH에 추가합니다. 관리자 권한도 Rust 툴체인도 필요 없고 빌드도 하지 않습니다. 끝나면 새 터미널을 열어야 PATH가 반영됩니다.

버전을 고정하거나, 다른 경로에 설치하거나, 지우려면 인자를 받을 수 있게 스크립트블록으로 실행합니다.

```powershell
$s = [scriptblock]::Create((irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1))
& $s -Version v0.2.2
& $s -InstallDir D:\tools\airec
& $s -Uninstall
```

삭제는 실행 파일과 PATH 항목만 지웁니다. 녹화본과 `airec.toml`은 그대로 둡니다.

스크립트를 셸에 바로 흘려 넣는 게 꺼려지면 [릴리즈 페이지](https://github.com/j-token/airec/releases)에서 zip을 받아 옆에 있는 `.sha256`으로 검증한 뒤 `airec.exe`를 PATH 아무 곳에나 두면 됩니다.

소스에서 빌드하려면 Rust 1.85 이상이 필요합니다.

```powershell
cargo build --release
```

실행 파일은 `target\release\airec.exe`에 생성됩니다. 사용하기 전에 이 컴퓨터에서 캡처와 인코딩이 되는지 먼저 확인하십시오.

```powershell
airec doctor --json
```

`doctor`는 Windows Graphics Capture 지원 여부, 선택될 인코더, 하드웨어 인코딩 성공 여부를 보고합니다. GPU가 없는 VM에서는 소프트웨어 인코딩으로 넘어가고 그 사실을 알려줍니다.

## 녹화

먼저 녹화할 대상을 찾습니다.

```powershell
airec list monitors --json
airec list windows --json
```

주 모니터를 30초 녹화합니다.

```powershell
airec record --duration 30s --out demo.mp4
```

모든 모니터를 각각 파일로 녹화합니다.

```powershell
airec record --monitor all --duration 30s --out-dir .\rec
```

창 두 개를 각각 파일로 녹화합니다. 창은 제목 일부, HWND, 프로세스명 중 하나로 지정합니다.

```powershell
airec record --window "Chrome" --window "Windows Terminal" --duration 30s --out-dir .\rec
airec record --window-handle 6758808 --process notepad.exe --duration 30s --out-dir .\rec
```

출력은 yuv420p 비디오 전용 H.264를 담은 fragmented MP4입니다. 윈도우 기본 플레이어, 크롬, 깃허브 웹 플레이어에서 재생됩니다. fragmented라서 프로세스가 강제 종료돼도 그 시점까지의 영상이 재생 가능한 파일로 남습니다.

## 에이전트에서 사용하기

녹화는 보통 작업 전체에 걸쳐 진행되므로 `start`와 `stop`이 분리돼 있습니다. `start`는 첫 프레임이 확인되면 반환하고 백그라운드로 넘어갑니다.

```powershell
airec start --window "MyApp" --out evidence.mp4 --json
# {"event":"started","session":"a1b2",...}

# 이 사이에 에이전트가 작업을 수행합니다

airec status --json
airec stop --json
# {"event":"saved","file":"...evidence.mp4","stop_reason":"requested","duration_ms":42180,"frames":2530,...}
```

대상이 종료되는 모든 이벤트에는 `stop_reason`이 붙습니다. 아래 세 개는 의도한 종료입니다.

| `stop_reason` | 의미 |
| --- | --- |
| `requested` | `stop` 호출 |
| `duration_limit` | `--duration` 도달 |
| `max_duration` | `--max-duration` 상한 도달 |

나머지는 의도치 않은 종료입니다. 창이 닫히면 `target_lost`, 파이프라인이 실패하면 `error`, `--on-failure abort` 상태에서 다른 대상이 실패하면 `aborted_on_failure`가 됩니다. 이 필드 하나만 확인하면 증거를 믿어도 되는지 판단할 수 있습니다.

여러 대상을 녹화하다가 일부가 실패했을 때의 동작은 직접 고릅니다.

```powershell
airec record --monitor all --on-failure continue   # 기본값. 나머지는 저장하고 PARTIAL_FAILURE로 종료 코드 6
airec record --monitor all --on-failure abort      # 전체를 finalize하고 실패 처리
```

증거가 일부라도 있으면 쓸모 있는 작업에는 `continue`를, 완전한 녹화만 의미 있는 작업에는 `abort`를 쓰십시오.

오류는 구조화돼 나오고 종료 코드가 코드와 대응됩니다.

```json
{"event":"error","code":"TARGET_NOT_FOUND","message":"no window matches title 'MyAp'","data":{"query":"MyAp","candidates":[]}}
```

| 코드 | 종료 코드 |
| --- | --- |
| `TARGET_NOT_FOUND`, `AMBIGUOUS_TARGET` | 2 |
| `CAPTURE_INIT_FAILED`, `ENCODER_UNAVAILABLE`, `FIRST_FRAME_TIMEOUT` | 3 |
| `NO_ACTIVE_SESSION`, `SESSION_AMBIGUOUS` | 4 |
| `OUTPUT_IO_ERROR` | 5 |
| `PARTIAL_FAILURE`, `ABORTED_ON_FAILURE` | 6 |

`AMBIGUOUS_TARGET`은 후보 창 목록을 `data`에 함께 반환합니다. 에이전트가 추측하지 않고 스스로 질의를 좁힐 수 있습니다.

이 내용은 `skills/airec`에 에이전트 스킬로 정리돼 있습니다. 설치하면 에이전트가 명령어와 `stop_reason` 판정 규칙을 아는 상태로 시작합니다.

```powershell
irm https://raw.githubusercontent.com/j-token/airec/main/install.ps1 | iex
npx skills add j-token/airec --skill airec
```

스킬은 사용법 문서지 녹화기 자체가 아닙니다. 첫 줄을 빼면 에이전트가 없는 명령어를 호출하게 되니 CLI도 같이 깔아야 합니다. 둘째 줄에 `-g`를 붙이면 현재 프로젝트가 아니라 전체 프로젝트에 설치됩니다. `--skill airec`을 빼면 이 저장소의 내부 워크플로 스킬까지 함께 목록에 잡히는데, 그건 녹화기와 무관합니다.

## 포인터 효과

클릭, 드래그, 포인터 이동은 인코딩 전에 프레임에 합성됩니다. 그래서 어떤 플레이어에서 봐도 보이고 후처리가 필요 없습니다. 좌클릭과 우클릭은 색이 다르고, 드래그는 시작 지점부터의 경로를 보여줍니다.

기본값은 좌클릭 `#FFD400`, 우클릭 `#00A2FF`에 34 px, 500 ms이고 드래그는 `#FF4081`에 4 px, 650 ms, 트레일은 `#00E5FF`에 3 px, 250 ms입니다. 누른 채로 4 px를 넘게 움직이거나 움직이면서 150 ms가 지나면 드래그로 인식합니다. 전부 조정할 수 있습니다.

```powershell
airec record --drag-color "#FF00AA" --drag-size 6 --trail-duration-ms 400 --duration 30s --out demo.mp4
```

`--no-effects`로 합성을 끄고, `--no-cursor`로 OS 커서를 캡처에서 뺍니다. 후킹하는 것은 마우스뿐이고 키보드 입력은 캡처하지도 기록하지도 않습니다.

`--event-log events.jsonl`을 주면 영상 기준 타임스탬프가 붙은 마우스 타임라인이 따로 저장됩니다. 에이전트가 "12.4초에 어디를 클릭했는지"를 영상 대신 텍스트로 교차 확인할 수 있습니다.

## GIF 변환

```powershell
airec convert demo.mp4 --fps 10 --width 960
```

디코딩과 GIF 작성 모두 Media Foundation을 통해 프로세스 내부에서 처리합니다. 다운로드하는 것도 없고 ffmpeg 설치도 필요 없습니다. 기본값은 10 fps에 최대 너비 960 px이며, 비율을 유지하고 원본이 더 작으면 확대하지 않습니다. 이미 있는 출력 파일은 덮어쓰지 않습니다.

첫 프레임 이후는 이전 프레임과의 차분으로 기록하므로, 화면이 거의 안 바뀌는 녹화본은 훨씬 작게 나오고 전체가 계속 바뀌는 녹화본은 그렇지 않습니다. 3초짜리 1080p 데스크톱 녹화본으로 재보니 MP4가 533 KB, 너비 960 px GIF가 501 KB였습니다. 스크롤이나 영상 재생처럼 매 프레임이 통째로 바뀌는 콘텐츠는 차분에서 얻을 게 없어서 원본보다 몇 배 커질 수 있습니다.

GIF는 영상 재생이 안 되는 곳에 붙이려고 쓰는 것이지 용량을 줄이려고 쓰는 게 아닙니다.

WebM은 지원하지 않습니다. 윈도우에 항상 쓸 수 있는 내장 WebM 인코더가 없고, 코덱이나 ffmpeg를 설치하게 만들면 실행 파일 하나로 끝낸다는 전제가 깨지기 때문입니다.

## 설정

기본값을 `airec.toml`에 둘 수 있습니다. 현재 디렉터리에서 찾고, 없으면 `%USERPROFILE%`에서 찾습니다.

```toml
[defaults]
fps = 30
quality = "medium"
effects = true

[effects]
click_color_left = "#FFD400"
click_color_right = "#00A2FF"
```

우선순위는 명령행 인자, 설정 파일, 내장 기본값 순입니다. 설정 파일이 있는데 내용이 잘못됐으면 조용히 홈 디렉터리로 넘어가지 않고 오류로 처리합니다.

## 현재 상태와 한계

지금은 윈도우만 지원합니다. macOS는 ScreenCaptureKit으로, 리눅스는 xdg-desktop-portal로 v0.3에서 지원할 계획이고, 둘 다 최초 실행 시 권한을 받는 과정이 필요합니다. Wayland에서는 컴포지터가 캡처 대상을 포털 대화상자에서 사용자가 직접 고르도록 강제하므로 창 지정을 완전 무인으로 하는 것은 불가능합니다. restore token을 저장해서 최초 1회만 사람이 개입하도록 만드는 방향으로 잡고 있습니다.

오디오, 영역 지정 녹화, 키보드 입력 표시, 스트리밍, 편집, 자동 업로드도 없습니다. 업로드는 일부러 뺐습니다. `gh` 같은 도구가 이미 하는 일이고 airec가 네트워크를 건드릴 이유가 없습니다.

v0.4에는 CLI와 같은 `airec-core` 크레이트 위에 Tauri v2 + Svelte GUI를 올릴 계획입니다.

## 라이선스

Apache License 2.0입니다. [LICENSE](LICENSE)를 참고하십시오.

`vendor/windows-capture`는 MIT 라이선스인 [windows-capture](https://github.com/NiiightmareXD/windows-capture) 크레이트를 수정한 사본이라 해당 디렉터리는 원래 라이선스를 그대로 따릅니다. 저작자 표기는 [NOTICE](NOTICE)에 있고, 무엇을 왜 고쳤는지는 `DECISIONS.md`의 D-015에 적어뒀습니다.
