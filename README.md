# Galaxy Clipboard Helper (Rust)

갤럭시에서 Windows로 공유한 클립보드 이미지를 PNG와 표준 Windows 비트맵(`CF_DIB`)으로 등록하는 트레이 앱의 Rust 포트입니다. C# 구현(`GalaxyClipboardHelper`)과 동일한 Samsung Continuity Service 파일 전송 흐름을 재현하며, 실행 파일 크기와 상주 메모리를 줄이는 것을 목표로 합니다.

## 동작 방식

1. `galaxy-clipboard-helper-rust.exe`를 실행합니다. C# 버전과 동일한 `Local\GalaxyClipboardHelper` 뮤텍스를 사용하므로 같은 Windows 세션에서 두 버전을 동시에 실행할 수 없습니다.
2. 갤럭시에서 이미지 한 장을 클립보드에 복사하면 Samsung Continuity Service가 등록한 자리표시자(`~ClipboardTransferring.tmp`)를 감지합니다.
3. `WINWORD`, `EXCEL`, `POWERPNT`, `APPLICATIONFRAMEHOST` 또는 프로세스 이름에 `SamsungNotes`가 포함된 앱이 포그라운드에 있으면 다른 앱으로 전환할 때까지 전송 시작을 보류합니다.
4. `%LOCALAPPDATA%\GalaxyClipboardHelper\Transfers\<GUID>` 폴더에 자리표시자를 복제해 삼성 서비스의 파일 수신을 시작하고, 수신 파일이 약 600ms 동안 안정화되면 Windows Imaging Component(WIC)로 디코딩합니다.
5. `CF_DIB`(top-down 24비트 BGR, 4바이트 행 정렬), `PNG`, 삼성 태그(`B0CD2DD9`)를 Windows 클립보드에 등록한 뒤 완료된 임시 파일을 정리합니다.
6. 이미지 한 장을 변환하며, 64 MiB 또는 4천만 픽셀을 초과하는 파일, 읽을 수 없는 이미지, 여러 항목은 전송 폴더에 보관합니다.

## 트레이 메뉴

- 현재 감시/전송 상태 표시
- **Windows 로그인 시 실행**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`의 `GalaxyClipboardHelper` 항목 토글 (기본값 꺼짐)
- **전송 폴더 열기**: `%LOCALAPPDATA%\GalaxyClipboardHelper\Transfers` 열기
- **종료**

## 테스트 및 빌드

`x86_64-pc-windows-gnu` 툴체인에서도 외부 `dlltool.exe` 없이 바로 빌드할 수 있도록 `windows` 크레이트 `0.58`(`windows-targets` 정적 임포트 라이브러리 사용)을 사용합니다.

```powershell
cargo test
cargo build --release
```

릴리스 실행 파일은 `target\release\galaxy-clipboard-helper-rust.exe`에 생성됩니다.
