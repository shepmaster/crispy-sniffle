use std::{
    cell::{Cell, RefCell},
    io,
    panic::{self, AssertUnwindSafe},
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

macro_rules! skip {
    () => {
        $crate::spec().skip(Option::<&'static str>::None);
        return;
    };
    ($why:expr) => {
        $crate::spec().skip(Some($why));
        return;
    };
}

struct StaticSpecDeclaration {
    path: &'static str,
    name: &'static str,
    f: StaticKind,
}

struct SpecDeclaration {
    path: String,
    name: String,
    f: Kind,
}

impl From<&'_ StaticSpecDeclaration> for SpecDeclaration {
    fn from(other: &'_ StaticSpecDeclaration) -> Self {
        let StaticSpecDeclaration { path, name, f } = *other;

        // Remove our extra nesting from `specs_metadata`
        let path = path.rsplit_once("::").map(|(m, _)| m).unwrap_or("");

        Self {
            path: path.into(),
            name: name.into(),
            f: f.into(),
        }
    }
}

#[derive(Copy, Clone)]
enum StaticKind {
    Plain(fn() -> Result<(), String>),
}

enum Kind {
    Plain(Box<dyn FnOnce() -> Result<(), String> + Send + Sync + 'static>),
}

impl From<StaticKind> for Kind {
    fn from(other: StaticKind) -> Self {
        match other {
            StaticKind::Plain(f) => Kind::Plain(Box::new(f)),
        }
    }
}

thread_local! {
    static CURRENT_SPEC: RefCell<CurrentSpecInner> = Default::default();
}

#[derive(Copy, Clone)]
struct CurrentSpec(()); // TODO: make non-send / sync

fn spec() -> CurrentSpec {
    CurrentSpec(())
}

impl CurrentSpec {
    fn cycle() -> CurrentSpecInner {
        CURRENT_SPEC.with(|ctl| std::mem::take(&mut *ctl.borrow_mut()))
    }

    pub fn suffix(self, suffix: impl Into<String>) -> Self {
        CURRENT_SPEC.with(|ctl| ctl.borrow_mut().suffix = Some(suffix.into()));
        self
    }

    pub fn skip(self, reason: Option<impl Into<String>>) -> Self {
        CURRENT_SPEC.with(|ctl| {
            ctl.borrow_mut().skipped = Some(reason.map(Into::into));
        });
        self
    }

    pub fn spawn<F, T>(self, f: F) -> Self
    where
        F: FnOnce() -> T,
        F: Send + Sync + 'static,
        T: Termination,
    {
        CURRENT_SPEC.with(|ctl| {
            ctl.borrow_mut()
                .new_specs
                .push(Box::new(|| adapt_test_function(f)));
        });
        self
    }
}

#[derive(Default)]
struct CurrentSpecInner {
    suffix: Option<String>,
    skipped: Option<Option<String>>, // TODO: custom enum?
    new_specs: Vec<Box<dyn FnOnce() -> Result<(), String> + Send + Sync + 'static>>,
}

struct PendingSpec {
    spec: SpecDeclaration,
    queue_tx: Sender<Box<Self>>,
    status_tx: Sender<CompletedSpec>,
}

struct OwnedLocation {
    file: String,
    column: u32,
    line: u32,
}

struct OwnedPanicInfo {
    location: Option<OwnedLocation>,
    payload: Option<String>,
}

enum Status {
    Success,
    Failure(FailureKind),
    Skipped(Option<String>),
}

enum FailureKind {
    Panic(Option<OwnedPanicInfo>),
    Error(String),
}

struct CompletedSpec {
    path: String,
    name: String,
    suffix: Option<String>,
    status: Status,
    elapsed: Duration,
}

trait Termination {
    fn into_termination(self) -> Result<(), String>;
}

impl Termination for () {
    fn into_termination(self) -> Result<(), String> {
        Ok(())
    }
}

impl<T, E> Termination for Result<T, E>
where
    E: std::fmt::Display,
{
    fn into_termination(self) -> Result<(), String> {
        self.map(drop).map_err(|e| e.to_string())
    }
}

fn adapt_test_function<F, R>(f: F) -> Result<(), String>
where
    F: FnOnce() -> R,
    R: Termination,
{
    f().into_termination()
}

thread_local! {
    static CAPTURE_PANIC_INFORMATION: Cell<bool> = Default::default();
    static PANIC_INFORMATION: RefCell<Option<OwnedPanicInfo>> = Default::default();
}

fn register_capturing_panic_handler() {
    let base_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        CAPTURE_PANIC_INFORMATION.with(|capture| {
            let capture = capture.get();

            if capture {
                PANIC_INFORMATION.with(|captured_info| {
                    let location = info.location().map(|l| OwnedLocation {
                        file: l.file().to_string(),
                        line: l.line(),
                        column: l.column(),
                    });

                    let payload = info.payload();
                    let payload = if let Some(e) = payload.downcast_ref::<String>() {
                        Some(e.to_string())
                    } else if let Some(e) = payload.downcast_ref::<&str>() {
                        Some(e.to_string())
                    } else {
                        None
                    };

                    let info = OwnedPanicInfo { location, payload };

                    *captured_info.borrow_mut() = Some(info);
                })
            } else {
                base_hook(info);
            }
        })
    }));
}

fn capture_panics(f: Kind) -> Result<Result<(), String>, Option<OwnedPanicInfo>> {
    CAPTURE_PANIC_INFORMATION.with(|c| c.set(true));
    let test_result = panic::catch_unwind({
        let f = AssertUnwindSafe(f);

        move || match f.0 {
            Kind::Plain(f) => f(),
        }
    });
    CAPTURE_PANIC_INFORMATION.with(|c| c.set(false));

    test_result.map_err(|_| PANIC_INFORMATION.with(|info| info.borrow_mut().take()))
}

// This should probably not need to be called in multiple places
fn apply_user_name_modifications(name: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(suffix) => format!("{}{}", name, suffix),
        None => name.to_string(),
    }
}

fn spec_main(specs: impl IntoIterator<Item = impl Into<SpecDeclaration>>) {
    register_capturing_panic_handler();

    let (queue_tx, queue_rx) = std::sync::mpsc::channel();
    let (status_tx, status_rx) = std::sync::mpsc::channel();

    for spec in specs {
        let pending_spec = PendingSpec {
            spec: spec.into(),
            queue_tx: queue_tx.clone(),
            status_tx: status_tx.clone(),
        };
        queue_tx
            .send(Box::new(pending_spec))
            .expect("QueueNoLongerExists");
    }
    drop((queue_tx, status_tx));

    // Spawned to be able to print output status while running
    let spawn_thread = std::thread::spawn(|| {
        // TODO: support configuring N test threads
        let worker_pool = rayon::ThreadPoolBuilder::new().build().unwrap();

        worker_pool.scope(move |scope| {
            for pending_spec in queue_rx {
                scope.spawn(move |_scope| {
                    let PendingSpec {
                        spec,
                        queue_tx,
                        status_tx,
                    } = *pending_spec;
                    let SpecDeclaration { path, name, f, .. } = spec;

                    let start = Instant::now();
                    let test_result = capture_panics(f);
                    let elapsed = start.elapsed();

                    let CurrentSpecInner {
                        suffix,
                        skipped,
                        new_specs,
                    } = CurrentSpec::cycle();

                    let basename = apply_user_name_modifications(&name, suffix.as_deref());

                    for new_spec in new_specs {
                        let spec = SpecDeclaration {
                            path: path.clone(),
                            name: basename.clone(),
                            f: Kind::Plain(new_spec),
                        };
                        let pending_spec = PendingSpec {
                            spec,
                            queue_tx: queue_tx.clone(),
                            status_tx: status_tx.clone(),
                        };
                        queue_tx
                            .send(Box::new(pending_spec))
                            .expect("QueueNoLongerExists");
                    }

                    let status = match (test_result, skipped) {
                        (Err(info), _) => Status::Failure(FailureKind::Panic(info)),
                        (Ok(Err(info)), _) => Status::Failure(FailureKind::Error(info)),
                        (Ok(Ok(())), Some(why)) => Status::Skipped(why),
                        (Ok(Ok(())), None) => Status::Success,
                    };

                    let completed_spec = CompletedSpec {
                        path,
                        name,
                        suffix,
                        status,
                        elapsed,
                    };

                    status_tx
                        .send(completed_spec)
                        .expect("StatusNoLongerExists");
                })
            }
        });
    });

    for CompletedSpec {
        path,
        name,
        suffix,
        status,
        elapsed,
    } in status_rx
    {
        let name = apply_user_name_modifications(&name, suffix.as_deref());

        // Conceptually: summary / detail
        fn bonus_text(s: &str) -> (&str, &str) {
            let mut lines = s.splitn(2, "\n").fuse();
            let head = lines.next().unwrap_or("");
            let tail = lines.next().unwrap_or("");
            (head, tail)
        }

        let mut bonus = "";

        let status_text = match &status {
            Status::Success => "".to_string(),
            Status::Failure(e) => {
                let deets = match e {
                    FailureKind::Panic(Some(info)) => info.payload.as_deref().unwrap_or(""),
                    FailureKind::Panic(None) => "",
                    FailureKind::Error(s) => s,
                };

                let (deets_head, deets_tail) = bonus_text(deets);
                bonus = deets_tail;
                if deets_head.is_empty() {
                    " (FAILURE)".to_string()
                } else {
                    format!(" (FAILURE: {})", deets_head)
                }
            }
            Status::Skipped(Some(reason)) => {
                let (head, tail) = bonus_text(reason);
                bonus = tail;

                format!(" (SKIPPED: {})", head)
            }
            Status::Skipped(None) => " (SKIPPED)".to_string(),
        };

        use std::io::Write;
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        writeln!(stderr, "{}::{}{} {:?}", path, name, status_text, elapsed).unwrap();
        if !bonus.is_empty() {
            writeln!(stderr, "{}", bonus).unwrap();
        }

        drop(stderr);
    }

    spawn_thread.join().expect("Spawn thread panicked");

    // TODO: persistence file of failures, last known times
}

// ----------

// TODO: #[spec]
mod specs {
    use crate::spec;

    // TODO: #[spec]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }

    fn it_panics() {
        assert_eq!(2 + 2, 3);
    }

    fn it_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    fn it_returns_err() -> Result<(), Box<dyn std::error::Error>> {
        Err("oops".into())
    }

    fn it_returns_custom_error_ok() -> std::io::Result<()> {
        Ok(())
    }

    fn it_works_skipped() {
        skip!();
    }

    fn it_works_skipped_why() {
        skip!("Not available on macOS");
    }

    fn it_works_parameterized() {
        for i in 0..2 {
            // TODO: some way of hiding this parent in the test output?
            spec().spawn(move || {
                spec().suffix(format!("_{}", i));
                assert_eq!(2 + 2, 4);
            });
        }
    }

    fn it_works_parameterized_twice() {
        for i in 0..2 {
            // TODO: some way of hiding this parent in the test output?
            spec().spawn(move || {
                spec().suffix(format!("_{}", i));

                for j in 0..2 {
                    spec().spawn(move || {
                        spec().suffix(format!("_{}", j));
                        assert_eq!(2 + 2, 4);
                    });
                }
            });
        }
    }

    // TODO: Generate this
    mod specs_metadata {
        #![allow(non_upper_case_globals)]

        #[linkme::distributed_slice(crate::SPECS)]
        static it_works: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_works",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_works)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_panics: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_panics",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_panics)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_returns_ok: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_returns_ok",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_returns_ok)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_returns_err: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_returns_err",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_returns_err)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_returns_custom_error_ok: crate::StaticSpecDeclaration =
            crate::StaticSpecDeclaration {
                path: module_path!(),
                name: "it_returns_custom_error_ok",
                f: crate::StaticKind::Plain(|| {
                    crate::adapt_test_function(super::it_returns_custom_error_ok)
                }),
            };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_works_skipped: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_works_skipped",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_works_skipped)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_works_skipped_why: crate::StaticSpecDeclaration = crate::StaticSpecDeclaration {
            path: module_path!(),
            name: "it_works_skipped_why",
            f: crate::StaticKind::Plain(|| crate::adapt_test_function(super::it_works_skipped_why)),
        };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_works_parameterized: crate::StaticSpecDeclaration =
            crate::StaticSpecDeclaration {
                path: module_path!(),
                name: "it_works_parameterized",
                f: crate::StaticKind::Plain(|| {
                    crate::adapt_test_function(super::it_works_parameterized)
                }),
            };

        #[linkme::distributed_slice(crate::SPECS)]
        static it_works_parameterized_twice: crate::StaticSpecDeclaration =
            crate::StaticSpecDeclaration {
                path: module_path!(),
                name: "it_works_parameterized_twice",
                f: crate::StaticKind::Plain(|| {
                    crate::adapt_test_function(super::it_works_parameterized_twice)
                }),
            };
    }
}

// TODO: Generate this
#[linkme::distributed_slice]
static SPECS: [StaticSpecDeclaration] = [..];

// TODO: Generate this
fn main() {
    spec_main(&*SPECS);
}

// To ignore unused warnings
pub fn dummy() {
    main()
}
