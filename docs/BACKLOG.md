# Backlog

## `windows` 크레이트 `0.58` → `0.62` 마이그레이션

### 1. 목표 및 배경

- **현재 상태**: `Cargo.toml`은 `windows = "0.58"`(`windows-targets` + `windows_x86_64_gnu` 정적 임포트 라이브러리 사용)을 기준으로 빌드됩니다.
- **마이그레이션 목표**: 최신 Windows 메타데이터와 `windows-link`(`raw-dylib`) 기반의 `windows = "0.62"`로 업그레이드해 정적 임포트 라이브러리 크레이트 의존성을 제거합니다.
- **현재 보류 사유**: 현재 빌드 환경(`Rust stable GNU 1.100`, `x86_64-pc-windows-gnu` 단독 설치)에서는 `windows 0.60+`의 `raw-dylib` 처리 시 `dlltool.exe`가 필요합니다. `self-contained\dlltool.exe`를 `PATH`에 추가하더라도 내부적으로 어셈블러(`as.exe`)를 찾지 못해 `CreateProcess` 오류로 실패합니다.

---

### 2. 선행 조건 (툴체인 선택지)

아래 조건 중 하나를 충족한 환경에서 진행합니다.

1. **MSYS2 MinGW-w64 binutils 설치**: `dlltool.exe`와 `as.exe`가 모두 포함된 MinGW binutils 디렉터리를 `PATH`에 추가 (`x86_64-pc-windows-gnu` 유지).
2. **툴체인 전환**: `x86_64-pc-windows-msvc`(Visual Studio Build Tools 필요) 또는 `x86_64-pc-windows-gnullvm`(LLVM `dlltool` 포함) 타겟으로 빌드.

---

### 3. 코드 변경 체크리스트

#### `Cargo.toml` & `Cargo.lock`
- `[dependencies.windows]`의 `version`을 `"0.58"`에서 `"0.62"`로 변경합니다.
- `Win32_Security` 피처가 `0.62`에서도 필요한지 확인하고, 불필요하면 정리합니다.
- `cargo update` 또는 `cargo fetch`로 `Cargo.lock`을 갱신하고 `windows-targets` 대신 `windows-link` 그래프로 전환되었는지 확인합니다.

#### `src/winutil.rs`
- `show_message_box`: `MessageBoxW`의 첫 번째 인자에 `hwnd.unwrap_or_default()` 대신 `Option<HWND>`(`hwnd`)를 직접 전달합니다.

#### `src/image.rs`
- `encode_png`: `Error::from_win32()`를 `Error::from_thread()`로 변경합니다.

#### `src/clipboard.rs`
- `GlobalFree` 임포트를 `windows::Win32::Foundation`에서 `windows::Win32::System::Memory`로 이동합니다.
- `NativeClipboard::new` 및 `ClipboardMemory::new`: `Error::from_win32()`를 `Error::from_thread()`로 변경합니다.
- 선택적 핸들 인자를 `Option<T>` 형태로 변경합니다.
  - `OpenClipboard(hwnd)` → `OpenClipboard(Some(hwnd))`
  - `SetClipboardData(format, HANDLE(self.hglobal.0))` → `SetClipboardData(format, Some(HANDLE(self.hglobal.0)))`
  - `GlobalFree(self.hglobal)` → `GlobalFree(Some(self.hglobal))`

#### `src/startup.rs`
- 레지스트리 API의 예약(reserved) 인자 `0`을 `None`으로 변경합니다.
  - `RegOpenKeyExW(..., 0, ...)` → `RegOpenKeyExW(..., None, ...)`
  - `RegCreateKeyExW(..., 0, None, ...)` → `RegCreateKeyExW(..., None, None, ...)`
  - `RegSetValueExW(..., 0, ...)` → `RegSetValueExW(..., None, ...)`

#### `src/tray.rs`
- `RegisterClassExW`, `add_tray_icon` 실패 처리: `Error::from_win32()`를 `Error::from_thread()`로 변경합니다.
- 선택적 핸들 인자를 `Option<T>` 형태로 변경합니다.
  - `SetTimer(self.hwnd, ...)` → `SetTimer(Some(self.hwnd), ...)`
  - `KillTimer(self.hwnd, ...)` → `KillTimer(Some(self.hwnd), ...)`
  - `CreateWindowExW(..., hinstance, ...)` → `CreateWindowExW(..., Some(hinstance), ...)`
  - `PostMessageW(target, ...)` → `PostMessageW(Some(target), ...)`
  - `PostMessageW(hwnd, ...)` → `PostMessageW(Some(hwnd), ...)`

#### `README.md`
- `windows` `0.58` 및 `dlltool.exe` 회피 관련 안내 문구를 `0.62` 기준 요구 툴체인 안내로 갱신합니다.

---

### 4. 검증 절차

1. 포맷 및 린트 확인
   ```powershell
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   ```
2. 단위 및 상태 머신 통합 테스트 실행
   ```powershell
   cargo test
   ```
3. 릴리스 빌드 및 바이너리 크기 비교
   ```powershell
   cargo build --release
   ```
   - `target\release\galaxy-clipboard-helper-rust.exe` 크기 확인 (`0.58` 기준 `336,384` 바이트 대비 회귀 여부 비교)
4. 실기기/세션 수동 회귀 확인 (가능 시)
   - 트레이 메뉴 표시, `Windows 로그인 시 실행` 레지스트리 토글, 갤럭시 이미지 복사 → `CF_DIB`/`PNG`/`B0CD2DD9` 등록 및 앱 붙여넣기 동작 확인
