# Backlog

## `windows` 크레이트 `0.58` → `0.62` 마이그레이션 (완료)

### 1. 목표 및 배경

- **이전 상태**: `Cargo.toml`은 `windows = "0.58"`(`windows-targets` + `windows_x86_64_gnu` 정적 임포트 라이브러리 사용)을 기준으로 빌드되었습니다.
- **마이그레이션 목표**: 최신 Windows 메타데이터와 `windows-link`(`raw-dylib`) 기반의 `windows = "0.62"`(`0.62.2`)로 업그레이드해 정적 임포트 라이브러리 크레이트 의존성을 제거합니다.
- **선행 조건 해소**: `C:\msys64\mingw64\bin`의 `mingw-w64-x86_64-binutils`(`dlltool.exe`, `as.exe`)를 `PATH`에 추가해 `rustc 1.100.0` (`x86_64-pc-windows-gnu`) 환경에서 `raw-dylib` 링크를 정상 처리하도록 구성했습니다.

---

### 2. 선행 조건 (툴체인 선택지)

아래 조건 중 하나를 충족한 환경에서 빌드할 수 있습니다.

1. **MSYS2 MinGW-w64 binutils 설치 (적용됨)**: `dlltool.exe`와 `as.exe`가 모두 포함된 MinGW binutils 디렉터리를 `PATH`에 추가 (`x86_64-pc-windows-gnu` 유지).
2. **툴체인 전환**: `x86_64-pc-windows-msvc`(Visual Studio Build Tools 필요) 또는 `x86_64-pc-windows-gnullvm`(LLVM `dlltool` 포함) 타겟으로 빌드.

---

### 3. 코드 변경 체크리스트

#### `Cargo.toml` & `Cargo.lock`
- [x] `[dependencies.windows]`의 `version`을 `"0.58"`에서 `"0.62"`로 변경했습니다.
- [x] `Win32_Security` 피처 필요 여부를 확인해 유지했습니다 (`CreateMutexW`와 `RegCreateKeyExW`가 `windows 0.62.2`에서도 `Win32_Security` 피처 게이트 뒤에 있음).
- [x] `Cargo.lock`을 갱신해 `windows 0.62.2` + `windows-link 0.2.1` 그래프로 전환되고 `windows-targets` 및 `windows_*_gnu`/`msvc`/`gnullvm` 크레이트가 제거되었음을 확인했습니다.

#### `src/winutil.rs`
- [x] `show_message_box`: `MessageBoxW`의 첫 번째 인자에 `hwnd.unwrap_or_default()` 대신 `Option<HWND>`(`hwnd`)를 직접 전달하도록 변경했습니다.
- [x] `VisualStylesGuard::activate`: `ActivateActCtx(handle, &mut cookie)`를 `ActivateActCtx(Some(handle), &mut cookie)`로 변경했습니다 (`windows 0.62`의 `Option<HANDLE>` 시그니처 반영).

#### `src/image.rs`
- [x] `encode_png`: `Error::from_win32()`를 `Error::from_thread()`로 변경했습니다.

#### `src/clipboard.rs`
- [x] `GlobalFree` 임포트 위치를 확인해 `windows::Win32::Foundation`에 유지했습니다 (`windows 0.62.2`에서도 `Foundation` 모듈에 정의됨).
- [x] `NativeClipboard::new` 및 `ClipboardMemory::new`: `Error::from_win32()`를 `Error::from_thread()`로 변경했습니다.
- [x] 선택적 핸들 인자를 `Option<T>` 형태로 변경했습니다.
  - `OpenClipboard(hwnd)` → `OpenClipboard(Some(hwnd))`
  - `SetClipboardData(format, HANDLE(self.hglobal.0))` → `SetClipboardData(format, Some(HANDLE(self.hglobal.0)))`
  - `GlobalFree(self.hglobal)` → `GlobalFree(Some(self.hglobal))`

#### `src/startup.rs`
- [x] 레지스트리 API의 예약(reserved) 인자 `0`을 `None`으로 변경했습니다.
  - `RegOpenKeyExW(..., 0, ...)` → `RegOpenKeyExW(..., None, ...)`
  - `RegCreateKeyExW(..., 0, None, ...)` → `RegCreateKeyExW(..., None, None, ...)`
  - `RegSetValueExW(..., 0, ...)` → `RegSetValueExW(..., None, ...)`

#### `src/tray.rs`
- [x] `RegisterClassExW`, `add_tray_icon` 실패 처리: `Error::from_win32()`를 `Error::from_thread()`로 변경했습니다.
- [x] 선택적 핸들/파라미터 인자를 `Option<T>` 형태로 변경했습니다.
  - `SetTimer(self.hwnd, ...)` → `SetTimer(Some(self.hwnd), ...)`
  - `KillTimer(self.hwnd, ...)` → `KillTimer(Some(self.hwnd), ...)`
  - `CreateWindowExW(..., hinstance, ...)` → `CreateWindowExW(..., Some(hinstance), ...)`
  - `PostMessageW(target, ...)` → `PostMessageW(Some(target), ...)`
  - `PostMessageW(hwnd, ...)` → `PostMessageW(Some(hwnd), ...)`
  - `FindWindowExW(None, after, ...)` → `after`를 `Option<HWND>`(`None` / `Some(menu_hwnd)`)로 변경 (`windows 0.62` 반영)
  - `SendMessageW(menu_hwnd, MN_SIZEWINDOW, WPARAM(0), LPARAM(0))` → `SendMessageW(menu_hwnd, MN_SIZEWINDOW, Some(WPARAM(0)), Some(LPARAM(0)))` (`windows 0.62` 반영)
  - `InvalidateRect(menu_hwnd, ...)` → `InvalidateRect(Some(menu_hwnd), ...)` (`windows 0.62` 반영)

#### `README.md` & `docs/README.ko.md`
- [x] `windows` `0.58` 및 `dlltool.exe` 회피 관련 안내 문구를 `0.62` 기준 요구 툴체인 안내와 실측 DLL 수(`10`개)로 갱신했습니다.

---

### 4. 검증 절차

1. [x] 포맷 및 린트 확인
   ```powershell
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   ```
   - 경고 없이 통과.
2. [x] 단위 및 상태 머신 통합 테스트 실행
   ```powershell
   cargo test
   ```
   - 20개 테스트 전부 통과 (`20 passed; 0 failed`).
3. [x] 릴리스 빌드, 바이너리 크기 및 DLL 임포트 비교
   ```powershell
   cargo build --release
   ```
   - `target\release\galaxy-clipboard-helper-rust.exe` 크기: **`339,968` 바이트 (`332 KiB`)** (`0.58` 실측 `339,968` 바이트와 동일, 기존 문서 기준 `336,384` 바이트 대비 +`3,584` 바이트).
   - `objdump -p` 확인 결과 MinGW 런타임 DLL 의존성 없이 Windows 기본 내장 시스템 DLL **10개**만 임포트 (`advapi32.dll`, `api-ms-win-core-synch-l1-2-0.dll`, `combase.dll`, `kernel32.dll`, `msvcrt.dll`, `ntdll.dll`, `ole32.dll`, `oleaut32.dll`, `shell32.dll`, `user32.dll`).
4. [ ] 실기기/세션 수동 회귀 확인 (선택)
   - 트레이 메뉴 표시, `Run at Windows login` 레지스트리 토글, 갤럭시 이미지 복사 → `CF_DIB`/`PNG`/`B0CD2DD9` 등록 및 앱 붙여넣기 동작 확인
