use super::*;

// Resource limits are process-wide. Run every mutation in an isolated child,
// so neither parallel tests nor the developer's shell inherit the test limit.
#[test]
fn inherited_file_limits() {
    const CASE: &str = "ASTRA_TEST_FILE_LIMIT_CASE";
    let Ok(case) = std::env::var(CASE) else {
        for case in ["low_soft", "low_hard", "already_high"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::file_limit::tests::inherited_file_limits",
                    "--nocapture",
                ])
                .env(CASE, case)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };

    let mut inherited = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut inherited) },
        0
    );
    let ceiling = inherited.rlim_max.min(system_ceiling().unwrap());
    assert!(ceiling >= 512, "test requires a hard limit of at least 512");
    let initial = match case.as_str() {
        "low_soft" => libc::rlimit {
            rlim_cur: 256,
            rlim_max: inherited.rlim_max,
        },
        "low_hard" => libc::rlimit {
            rlim_cur: 256,
            rlim_max: 512,
        },
        "already_high" => libc::rlimit {
            rlim_cur: ceiling.min(TARGET + 1024),
            rlim_max: inherited.rlim_max,
        },
        _ => panic!("unknown test case"),
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &initial) }, 0);
    let (before, after) = raise().unwrap();
    assert_eq!(before.rlim_cur, initial.rlim_cur);
    assert_eq!(after.rlim_max, initial.rlim_max);
    assert_eq!(
        after.rlim_cur,
        initial
            .rlim_cur
            .max(TARGET.min(initial.rlim_max).min(ceiling))
    );
    let mut observed = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut observed) },
        0
    );
    assert_eq!(observed.rlim_cur, after.rlim_cur);
    assert_eq!(observed.rlim_max, initial.rlim_max);

    // Prove that the kernel actually allows more than 256 simultaneous FDs.
    let files: Vec<_> = (0..320)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    let (_, repeated) = raise().unwrap();
    assert_eq!(repeated.rlim_cur, after.rlim_cur);
    drop(files);
}
