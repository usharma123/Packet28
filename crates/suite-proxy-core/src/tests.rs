use crate::{run_and_reduce, ProxyRunRequest};

#[test]
fn rejects_unsafe_commands() {
    let err = run_and_reduce(ProxyRunRequest {
        argv: vec!["cat".to_string(), "/etc/passwd".to_string()],
        ..ProxyRunRequest::default()
    })
    .unwrap_err();

    assert!(err
        .to_string()
        .contains("proxy.run only allows: ls, find, grep, git status, git log"));
}

#[test]
fn deterministic_reduction_for_same_input() {
    let req = ProxyRunRequest {
        argv: vec!["ls".to_string()],
        max_lines: Some(40),
        max_output_bytes: Some(4_000),
        ..ProxyRunRequest::default()
    };

    let left = run_and_reduce(req.clone()).unwrap();
    let right = run_and_reduce(req).unwrap();

    assert_eq!(left.hash, right.hash);
    assert_eq!(left.payload.lines_out, right.payload.lines_out);
}

#[test]
fn allows_safe_git_subset() {
    let result = run_and_reduce(ProxyRunRequest {
        argv: vec!["git".to_string(), "status".to_string(), "--short".to_string()],
        ..ProxyRunRequest::default()
    });

    assert!(result.is_ok());
}
