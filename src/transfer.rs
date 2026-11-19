use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use crate::clipboard::{ClipboardBackend, ClipboardSnapshot, ClipboardWrite};
use crate::image::{self, ClipboardImage, DecodeOutcome};
use crate::winutil::{new_guid_hex, paths_equal_ignore_case};

pub const PLACEHOLDER: &str = "~ClipboardTransferring.tmp";
pub const RECEIVING: &str = "~BCF97A05F26299AA.tmp";

pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
pub const STABLE_DURATION: Duration = Duration::from_millis(600);

pub const STATUS_WATCHING: &str = "갤럭시 클립보드 감시 중";
pub const STATUS_WAIT_APP_SWITCH: &str = "다른 앱으로 전환하면 전송을 시작합니다";
pub const STATUS_WAITING_TRANSFER: &str = "갤럭시 이미지 전송 대기 중";
pub const STATUS_TIMEOUT: &str = "전송 시간 초과 · 갤럭시에서 다시 복사해 주세요";
pub const STATUS_MULTIPLE_ITEMS: &str = "여러 항목을 전송 폴더에 보관했습니다";
pub const STATUS_UNREADABLE: &str = "이미지로 읽을 수 없어 전송 파일을 보관했습니다";
pub const STATUS_ERROR: &str = "전송 오류 · 갤럭시에서 다시 복사해 주세요";

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Debug)]
struct TransferError;

impl From<io::Error> for TransferError {
    fn from(_: io::Error) -> Self {
        Self
    }
}

impl From<windows::core::Error> for TransferError {
    fn from(_: windows::core::Error) -> Self {
        Self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

enum DecodeState {
    Idle,
    Running(Receiver<DecodeOutcome>),
    Ready(ClipboardImage),
}

struct ActiveTransfer {
    folder: PathBuf,
    start_sequence: u32,
    start_time: Instant,
    candidate: Option<Candidate>,
    candidate_since: Option<Instant>,
    decode: DecodeState,
}

impl ActiveTransfer {
    fn new(folder: PathBuf, start_sequence: u32, start_time: Instant) -> Self {
        Self {
            folder,
            start_sequence,
            start_time,
            candidate: None,
            candidate_since: None,
            decode: DecodeState::Idle,
        }
    }
}

/// State machine coordinating Samsung placeholder detection, transfer folder polling,
/// background WIC decoding, and Windows clipboard publishing.
pub struct TransferController {
    samsung_folder: PathBuf,
    transfer_root: PathBuf,
    status: String,
    handled_sequence: Option<u32>,
    active: Option<ActiveTransfer>,
    on_decode_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    stable_duration: Duration,
    transfer_timeout: Duration,
}

impl TransferController {
    /// Creates a new controller with production 600ms stability and 30s timeout windows.
    pub fn new(
        samsung_folder: PathBuf,
        transfer_root: PathBuf,
        on_decode_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            samsung_folder,
            transfer_root,
            status: STATUS_WATCHING.to_string(),
            handled_sequence: None,
            active: None,
            on_decode_ready,
            stable_duration: STABLE_DURATION,
            transfer_timeout: TRANSFER_TIMEOUT,
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn transfer_root(&self) -> &Path {
        &self.transfer_root
    }

    /// Advances the transfer state machine once. Returns `true` when the polling timer should remain active.
    pub fn tick(&mut self, clipboard: &dyn ClipboardBackend, now: Instant) -> bool {
        match self.tick_inner(clipboard, now) {
            Ok(keep_running) => keep_running,
            Err(_) => {
                self.active = None;
                self.status = STATUS_ERROR.to_string();
                false
            }
        }
    }

    fn tick_inner(
        &mut self,
        clipboard: &dyn ClipboardBackend,
        now: Instant,
    ) -> Result<bool, TransferError> {
        let Some(snapshot) = clipboard.read() else {
            return Ok(true); // Another application currently holds the clipboard open.
        };

        if self.active.is_some() {
            self.check_transfer(&snapshot, clipboard, now)?;
        }

        if self.active.is_none() && self.handled_sequence != Some(snapshot.sequence) {
            if snapshot.has_samsung_tag
                && is_samsung_path(&self.samsung_folder, snapshot.file.as_deref(), PLACEHOLDER)
            {
                if !clipboard.can_request_transfer() {
                    self.status = STATUS_WAIT_APP_SWITCH.to_string();
                    return Ok(true);
                }
                if !self.request_transfer(&snapshot, clipboard, now)? {
                    return Ok(true);
                }
                self.handled_sequence = Some(snapshot.sequence);
            } else {
                self.handled_sequence = Some(snapshot.sequence);
            }
        }

        Ok(self.active.is_some())
    }

    fn request_transfer(
        &mut self,
        snapshot: &ClipboardSnapshot,
        clipboard: &dyn ClipboardBackend,
        now: Instant,
    ) -> Result<bool, TransferError> {
        let Some(marker_path) = snapshot.file.as_deref() else {
            return Ok(true);
        };

        let meta = match fs::metadata(marker_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(err.into()),
        };
        if !meta.is_file() || meta.len() != 0 {
            return Ok(true);
        }

        let folder = self.transfer_root.join(new_guid_hex()?);
        fs::create_dir_all(&folder)?;

        if clipboard.read().map(|s| s.sequence) != Some(snapshot.sequence) {
            let _ = fs::remove_dir(&folder);
            return Ok(false);
        }

        // Samsung starts a paste when a copy of its placeholder appears in the destination.
        if let Err(err) = fs::copy(marker_path, folder.join(PLACEHOLDER)) {
            let _ = fs::remove_dir(&folder);
            return Err(err.into());
        }

        self.active = Some(ActiveTransfer::new(folder, snapshot.sequence, now));
        self.status = STATUS_WAITING_TRANSFER.to_string();
        Ok(true)
    }

    fn check_transfer(
        &mut self,
        snapshot: &ClipboardSnapshot,
        clipboard: &dyn ClipboardBackend,
        now: Instant,
    ) -> Result<(), TransferError> {
        let Some(transfer) = self.active.as_mut() else {
            return Ok(());
        };

        let is_receiving = snapshot.has_samsung_tag
            && is_samsung_path(&self.samsung_folder, snapshot.file.as_deref(), RECEIVING);
        let is_result = snapshot
            .file
            .as_deref()
            .and_then(Path::parent)
            .is_some_and(|parent| paths_equal_ignore_case(parent, &transfer.folder));

        if snapshot.sequence != transfer.start_sequence && !is_receiving && !is_result {
            self.active = None;
            self.status = STATUS_WATCHING.to_string();
            return Ok(());
        }

        if now.saturating_duration_since(transfer.start_time) > self.transfer_timeout {
            self.end_transfer(snapshot.sequence, STATUS_TIMEOUT);
            return Ok(());
        }

        if !is_receiving && !is_result {
            return Ok(());
        }

        let mut entries = Vec::with_capacity(2);
        for entry_res in fs::read_dir(&transfer.folder)? {
            let entry = entry_res?;
            let name = entry.file_name();
            if name.eq_ignore_ascii_case(PLACEHOLDER) || name.eq_ignore_ascii_case(RECEIVING) {
                continue;
            }
            entries.push(entry);
            if entries.len() >= 2 {
                break;
            }
        }

        if entries.is_empty() {
            return Ok(());
        }

        if entries.len() > 1 {
            self.end_transfer(snapshot.sequence, STATUS_MULTIPLE_ITEMS);
            return Ok(());
        }

        let entry = &entries[0];
        let meta = entry.metadata()?;
        let attrs = meta.file_attributes();
        if (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0 || (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            self.end_transfer(snapshot.sequence, STATUS_MULTIPLE_ITEMS);
            return Ok(());
        }

        let candidate = Candidate {
            path: entry.path(),
            len: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        };

        if transfer.candidate.as_ref() != Some(&candidate) {
            transfer.candidate = Some(candidate.clone());
            transfer.candidate_since = Some(now);
            transfer.decode = DecodeState::Idle;
        }

        let Some(since) = transfer.candidate_since else {
            return Ok(());
        };
        if candidate.len == 0 || now.saturating_duration_since(since) < self.stable_duration {
            return Ok(());
        }

        if matches!(transfer.decode, DecodeState::Idle) {
            let (tx, rx) = mpsc::sync_channel(1);
            let path = candidate.path.clone();
            let notify = self.on_decode_ready.clone();
            std::thread::spawn(move || {
                let outcome = image::decode_file(&path);
                let _ = tx.send(outcome);
                if let Some(cb) = notify {
                    cb();
                }
            });
            transfer.decode = DecodeState::Running(rx);
            return Ok(());
        }

        if let DecodeState::Running(rx) = &transfer.decode {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    self.end_transfer(snapshot.sequence, STATUS_UNREADABLE);
                    return Ok(());
                }
                Ok(DecodeOutcome::Retry) => {
                    transfer.decode = DecodeState::Idle;
                    return Ok(());
                }
                Ok(DecodeOutcome::Unreadable) => {
                    self.end_transfer(snapshot.sequence, STATUS_UNREADABLE);
                    return Ok(());
                }
                Ok(DecodeOutcome::Image(img)) => {
                    transfer.decode = DecodeState::Ready(img);
                }
            }
        }

        let DecodeState::Ready(ref img) = transfer.decode else {
            return Ok(());
        };

        match clipboard.write_image(snapshot.sequence, img)? {
            ClipboardWrite::Written => {
                let message = format!("클립보드에 이미지 복사됨 ({} × {})", img.width, img.height);
                let folder = transfer.folder.clone();
                self.end_transfer(snapshot.sequence, message);
                remove_completed_image(&candidate.path, &folder);
            }
            ClipboardWrite::Changed | ClipboardWrite::Busy => {
                // Keep `DecodeState::Ready` so the next tick refreshes the snapshot or retries the write.
            }
        }

        Ok(())
    }

    fn end_transfer(&mut self, sequence: u32, message: impl Into<String>) {
        self.handled_sequence = Some(sequence);
        self.active = None;
        self.status = message.into();
    }
}

fn is_samsung_path(samsung_folder: &Path, path: Option<&Path>, name: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    let expected = samsung_folder.join(name);
    paths_equal_ignore_case(path, &expected)
}

fn remove_completed_image(path: &Path, folder: &Path) {
    let _ = fs::remove_file(path);
    if let Ok(mut entries) = fs::read_dir(folder)
        && entries.next().is_none()
    {
        let _ = fs::remove_dir(folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tests::{make_temp_dir, sample_3x2_bmp_bytes};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct MockClipboard {
        snapshot: RefCell<Option<ClipboardSnapshot>>,
        can_request: RefCell<bool>,
        write_results: RefCell<VecDeque<ClipboardWrite>>,
        written: RefCell<Vec<(u32, ClipboardImage)>>,
    }

    impl MockClipboard {
        fn new(snapshot: Option<ClipboardSnapshot>) -> Self {
            Self {
                snapshot: RefCell::new(snapshot),
                can_request: RefCell::new(true),
                write_results: RefCell::new(VecDeque::new()),
                written: RefCell::new(Vec::new()),
            }
        }

        fn set_snapshot(&self, snapshot: Option<ClipboardSnapshot>) {
            *self.snapshot.borrow_mut() = snapshot;
        }
    }

    impl ClipboardBackend for MockClipboard {
        fn read(&self) -> Option<ClipboardSnapshot> {
            self.snapshot.borrow().clone()
        }

        fn write_image(
            &self,
            expected_sequence: u32,
            image: &ClipboardImage,
        ) -> windows::core::Result<ClipboardWrite> {
            let outcome = self
                .write_results
                .borrow_mut()
                .pop_front()
                .unwrap_or(ClipboardWrite::Written);
            if outcome == ClipboardWrite::Written {
                self.written
                    .borrow_mut()
                    .push((expected_sequence, image.clone()));
            }
            Ok(outcome)
        }

        fn can_request_transfer(&self) -> bool {
            *self.can_request.borrow()
        }
    }

    fn setup_controller(
        label: &str,
        stable: Duration,
        timeout: Duration,
    ) -> (PathBuf, PathBuf, TransferController) {
        let root = make_temp_dir(label);
        let samsung_folder = root.join("samsung");
        let transfer_root = root.join("transfers");
        fs::create_dir_all(&samsung_folder).expect("samsung dir");
        fs::create_dir_all(&transfer_root).expect("transfers dir");

        let mut ctrl = TransferController::new(samsung_folder.clone(), transfer_root.clone(), None);
        ctrl.stable_duration = stable;
        ctrl.transfer_timeout = timeout;
        (root, samsung_folder, ctrl)
    }

    fn active_transfer_folder(ctrl: &TransferController) -> PathBuf {
        ctrl.active
            .as_ref()
            .expect("active transfer")
            .folder
            .clone()
    }

    fn drive_decode_to_completion(
        ctrl: &mut TransferController,
        clipboard: &MockClipboard,
        now: Instant,
    ) -> bool {
        for _ in 0..50 {
            let keep_running = ctrl.tick(clipboard, now);
            if !keep_running {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn happy_path_copies_placeholder_decodes_image_writes_clipboard_and_cleans_up() {
        let (root, samsung_folder, mut ctrl) =
            setup_controller("happy", Duration::from_millis(50), Duration::from_secs(30));
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("create 0-byte placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 10,
            file: Some(placeholder.clone()),
            has_samsung_tag: true,
        }));

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        assert_eq!(ctrl.status(), STATUS_WAITING_TRANSFER);
        let transfer_dir = active_transfer_folder(&ctrl);
        assert!(transfer_dir.join(PLACEHOLDER).is_file());

        // Samsung switches clipboard to receiving marker and writes capture_temp.jpg with BMP payload.
        mock.set_snapshot(Some(ClipboardSnapshot {
            sequence: 11,
            file: Some(samsung_folder.join(RECEIVING)),
            has_samsung_tag: true,
        }));
        let delivered = transfer_dir.join("capture_temp.jpg");
        fs::write(&delivered, sample_3x2_bmp_bytes()).expect("write delivered image");

        // Before stability window elapses, decoding does not start.
        assert!(ctrl.tick(&mock, t0 + Duration::from_millis(20)));
        assert!(mock.written.borrow().is_empty());

        // Remove copied placeholder as Samsung or helper cleanup would leave only delivered file -> empty folder cleanup.
        fs::remove_file(transfer_dir.join(PLACEHOLDER)).expect("remove copied placeholder");

        // Once stability window elapses, worker spawns and publishes image.
        let keep_running =
            drive_decode_to_completion(&mut ctrl, &mock, t0 + Duration::from_millis(80));
        assert!(!keep_running);
        assert_eq!(ctrl.status(), "클립보드에 이미지 복사됨 (3 × 2)");

        let written = mock.written.borrow();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].0, 11);
        assert_eq!((written[0].1.width, written[0].1.height), (3, 2));
        assert!(!delivered.exists(), "completed image should be deleted");
        assert!(
            !transfer_dir.exists(),
            "empty transfer dir should be deleted"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn defers_transfer_while_office_or_samsung_notes_is_foreground() {
        let (root, samsung_folder, mut ctrl) =
            setup_controller("defer", Duration::from_millis(50), Duration::from_secs(30));
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 20,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));
        *mock.can_request.borrow_mut() = false;

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        assert_eq!(ctrl.status(), STATUS_WAIT_APP_SWITCH);
        assert!(ctrl.active.is_none());

        // User switches to Chrome -> transfer starts immediately.
        *mock.can_request.borrow_mut() = true;
        assert!(ctrl.tick(&mock, t0 + Duration::from_millis(150)));
        assert_eq!(ctrl.status(), STATUS_WAITING_TRANSFER);
        assert!(ctrl.active.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn aborts_request_transfer_if_clipboard_sequence_changes_during_setup() {
        struct RaceClipboard {
            reads: RefCell<VecDeque<Option<ClipboardSnapshot>>>,
        }
        impl ClipboardBackend for RaceClipboard {
            fn read(&self) -> Option<ClipboardSnapshot> {
                self.reads.borrow_mut().pop_front().flatten()
            }
            fn write_image(
                &self,
                _: u32,
                _: &ClipboardImage,
            ) -> windows::core::Result<ClipboardWrite> {
                unreachable!()
            }
            fn can_request_transfer(&self) -> bool {
                true
            }
        }

        let (root, samsung_folder, mut ctrl) = setup_controller(
            "seq-race",
            Duration::from_millis(50),
            Duration::from_secs(30),
        );
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let race_mock = RaceClipboard {
            reads: RefCell::new(VecDeque::from([
                Some(ClipboardSnapshot {
                    sequence: 30,
                    file: Some(placeholder),
                    has_samsung_tag: true,
                }),
                Some(ClipboardSnapshot {
                    sequence: 31,
                    file: None,
                    has_samsung_tag: false,
                }),
            ])),
        };

        assert!(ctrl.tick(&race_mock, Instant::now()));
        assert!(ctrl.active.is_none());
        assert_eq!(ctrl.handled_sequence, None);
        assert_eq!(
            fs::read_dir(ctrl.transfer_root())
                .expect("read transfer root")
                .count(),
            0,
            "raced folder must be deleted"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancels_active_transfer_when_user_copies_something_else() {
        let (root, samsung_folder, mut ctrl) = setup_controller(
            "cancel-copy",
            Duration::from_millis(50),
            Duration::from_secs(30),
        );
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 40,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        assert_eq!(ctrl.status(), STATUS_WAITING_TRANSFER);

        // User copies text on PC -> sequence becomes 41 with no Samsung tag/file.
        mock.set_snapshot(Some(ClipboardSnapshot {
            sequence: 41,
            file: None,
            has_samsung_tag: false,
        }));
        assert!(!ctrl.tick(&mock, t0 + Duration::from_millis(100)));
        assert!(ctrl.active.is_none());
        assert_eq!(ctrl.status(), STATUS_WATCHING);
        assert_eq!(ctrl.handled_sequence, Some(41));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn times_out_after_transfer_timeout() {
        let (root, samsung_folder, mut ctrl) = setup_controller(
            "timeout",
            Duration::from_millis(50),
            Duration::from_secs(30),
        );
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 50,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        assert!(!ctrl.tick(&mock, t0 + Duration::from_secs(31)));
        assert!(ctrl.active.is_none());
        assert_eq!(ctrl.status(), STATUS_TIMEOUT);
        assert_eq!(ctrl.handled_sequence, Some(50));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn preserves_multiple_items_or_directory_in_transfer_folder() {
        let (root, samsung_folder, mut ctrl) =
            setup_controller("multi", Duration::from_millis(50), Duration::from_secs(30));
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 60,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        let transfer_dir = active_transfer_folder(&ctrl);
        fs::write(transfer_dir.join("a.png"), b"1").expect("a");
        fs::write(transfer_dir.join("b.png"), b"2").expect("b");

        mock.set_snapshot(Some(ClipboardSnapshot {
            sequence: 61,
            file: Some(samsung_folder.join(RECEIVING)),
            has_samsung_tag: true,
        }));
        assert!(!ctrl.tick(&mock, t0 + Duration::from_millis(10)));
        assert_eq!(ctrl.status(), STATUS_MULTIPLE_ITEMS);
        assert!(transfer_dir.join("a.png").exists());
        assert!(transfer_dir.join("b.png").exists());

        // Also test single subdirectory case.
        let (root2, samsung2, mut ctrl2) =
            setup_controller("subdir", Duration::from_millis(50), Duration::from_secs(30));
        let placeholder2 = samsung2.join(PLACEHOLDER);
        fs::write(&placeholder2, b"").expect("placeholder2");
        let mock2 = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 70,
            file: Some(placeholder2),
            has_samsung_tag: true,
        }));
        assert!(ctrl2.tick(&mock2, t0));
        let transfer_dir2 = active_transfer_folder(&ctrl2);
        fs::create_dir(transfer_dir2.join("nested")).expect("subdir");
        mock2.set_snapshot(Some(ClipboardSnapshot {
            sequence: 71,
            file: Some(samsung2.join(RECEIVING)),
            has_samsung_tag: true,
        }));
        assert!(!ctrl2.tick(&mock2, t0 + Duration::from_millis(10)));
        assert_eq!(ctrl2.status(), STATUS_MULTIPLE_ITEMS);
        assert!(transfer_dir2.join("nested").is_dir());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn preserves_unreadable_file_and_updates_status() {
        let (root, samsung_folder, mut ctrl) = setup_controller(
            "unreadable",
            Duration::from_millis(30),
            Duration::from_secs(30),
        );
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 80,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        let transfer_dir = active_transfer_folder(&ctrl);
        let bad_file = transfer_dir.join("broken.png");
        fs::write(&bad_file, b"definitely not a valid image").expect("bad file");

        mock.set_snapshot(Some(ClipboardSnapshot {
            sequence: 81,
            file: Some(transfer_dir.join("broken.png")),
            has_samsung_tag: false,
        }));

        assert!(ctrl.tick(&mock, t0 + Duration::from_millis(10)));
        let keep_running =
            drive_decode_to_completion(&mut ctrl, &mock, t0 + Duration::from_millis(50));
        assert!(!keep_running);
        assert_eq!(ctrl.status(), STATUS_UNREADABLE);
        assert!(bad_file.exists(), "unreadable file must be kept");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retains_decoded_image_across_clipboard_busy_and_retries_write() {
        let (root, samsung_folder, mut ctrl) = setup_controller(
            "busy-retry",
            Duration::from_millis(30),
            Duration::from_secs(30),
        );
        let placeholder = samsung_folder.join(PLACEHOLDER);
        fs::write(&placeholder, b"").expect("placeholder");

        let mock = MockClipboard::new(Some(ClipboardSnapshot {
            sequence: 90,
            file: Some(placeholder),
            has_samsung_tag: true,
        }));
        mock.write_results
            .borrow_mut()
            .push_back(ClipboardWrite::Busy);
        mock.write_results
            .borrow_mut()
            .push_back(ClipboardWrite::Written);

        let t0 = Instant::now();
        assert!(ctrl.tick(&mock, t0));
        let transfer_dir = active_transfer_folder(&ctrl);
        let delivered = transfer_dir.join("image.bmp");
        fs::write(&delivered, sample_3x2_bmp_bytes()).expect("bmp");

        mock.set_snapshot(Some(ClipboardSnapshot {
            sequence: 91,
            file: Some(samsung_folder.join(RECEIVING)),
            has_samsung_tag: true,
        }));

        assert!(ctrl.tick(&mock, t0 + Duration::from_millis(10)));

        // First run completes decode and hits Busy, keeping transfer active in DecodeState::Ready.
        for _ in 0..30 {
            assert!(ctrl.tick(&mock, t0 + Duration::from_millis(50)));
            if let Some(active) = &ctrl.active
                && matches!(active.decode, DecodeState::Ready(_))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matches!(
                ctrl.active.as_ref().map(|a| &a.decode),
                Some(DecodeState::Ready(_))
            ),
            "expected decoded image to be cached after ClipboardWrite::Busy"
        );
        assert!(mock.written.borrow().is_empty());

        // Next tick retries write_image directly using cached Ready image and succeeds!
        assert!(!ctrl.tick(&mock, t0 + Duration::from_millis(200)));
        assert_eq!(ctrl.status(), "클립보드에 이미지 복사됨 (3 × 2)");
        assert_eq!(mock.written.borrow().len(), 1);

        let _ = fs::remove_dir_all(&root);
    }
}
